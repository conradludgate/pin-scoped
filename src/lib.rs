use pin_project_lite::pin_project;
use pinned_aliasable::Aliasable;
use std::future::Future;
use std::{mem::ManuallyDrop, pin::Pin, task::Waker};
use tokio::task::{AbortHandle, JoinHandle};

mod sync;
use sync::{Condvar, Mutex, UnsafeCell};

pin_project! {
    /// Scope represents a scope holding some values.
    ///
    /// [`tokio`] tasks can be spawned in the context of this scope.
    ///
    /// Should the scope be dropped before those tasks complete,
    /// the tasks will be [`aborted`](JoinHandle::abort) and the runtime
    /// thread will block until all tasks have dropped.
    pub struct Scope<State: 'static> {
        handles: Vec<AbortHandle>,

        #[pin]
        aliased: Aliasable<SharedState<State>>,

        started_locking: bool,
    }

    impl<State: 'static> PinnedDrop for Scope<State> {
        #[inline(never)]
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            let aliased = this.aliased.as_ref().get();
            *this.started_locking = true;

            // handles could be empty if
            // 1. we have never spawned a task in this scope
            // 2. we have awaited the scope and extracted the inner state
            if this.handles.is_empty() {
                let mut lock = aliased.lock.lock().unwrap();
                lock.waker.take();
                debug_assert!(lock.tasks == 0 || lock.tasks == -1);
                if lock.tasks == 0 {
                    lock.tasks = -1;

                    #[cfg(loom)]
                    unsafe { aliased.state.with_mut(|s| ManuallyDrop::drop(&mut *s)) };
                    #[cfg(not(loom))]
                    unsafe { ManuallyDrop::drop(&mut *aliased.state.get()) };
                }
                return;
            }

            for handle in this.handles.drain(..) {
                handle.abort();
            }

            // lock and drop the state.
            let mut lock = aliased.lock.lock().unwrap();
            while lock.tasks != 0 {
                lock = aliased.condvar.wait(lock).unwrap();
                debug_assert!(lock.tasks != -1, "state should not be dropped");
            }
            #[cfg(loom)]
            unsafe { aliased.state.with_mut(|s| ManuallyDrop::drop(&mut *s)) };
            #[cfg(not(loom))]
            unsafe { ManuallyDrop::drop(&mut *aliased.state.get()) };
        }
    }
}

struct SharedState<State> {
    lock: Mutex<LockState>,
    condvar: Condvar,

    /// initialised if and only if lock.tasks != -1
    state: UnsafeCell<ManuallyDrop<State>>,
}

struct LockState {
    /// positive for read-only tasks
    /// isize::MIN for a single mut task
    /// -1 for when the state is removed
    tasks: isize,
    waker: Option<Waker>,
}

impl<State> Future for Scope<State> {
    type Output = State;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<State> {
        let this = self.project();
        *this.started_locking = true;

        let state = {
            let aliased = this.aliased.as_ref().get();

            let mut lock = aliased.lock.lock().unwrap();
            if lock.tasks != 0 {
                if let Some(waker) = &mut lock.waker {
                    waker.clone_from(cx.waker())
                } else {
                    lock.waker = Some(cx.waker().clone())
                }
                return std::task::Poll::Pending;
            }

            debug_assert!(lock.tasks != -1, "state should not be dropped");

            // take the state now it's locked
            lock.tasks = -1;

            #[cfg(loom)]
            let state = unsafe { aliased.state.with_mut(|s| ManuallyDrop::take(&mut *s)) };
            #[cfg(not(loom))]
            let state = unsafe { ManuallyDrop::take(&mut *aliased.state.get()) };
            state
        };

        this.handles.clear();

        std::task::Poll::Ready(state)
    }
}

impl<State: 'static + Sync> Scope<State> {
    pub fn new(state: State) -> Self {
        Scope {
            handles: vec![],
            aliased: Aliasable::new(SharedState {
                lock: Mutex::new(LockState {
                    waker: None,
                    tasks: 0,
                }),
                condvar: Condvar::new(),
                state: UnsafeCell::new(ManuallyDrop::new(state)),
            }),
            started_locking: false,
        }
    }

    /// Spawns a new asynchronous task, returning a
    /// [`JoinHandle`] for it.
    ///
    /// The provided future will start running in the background immediately
    /// when `spawn` is called, even if you don't await the returned
    /// `JoinHandle`.
    ///
    /// Spawning a task enables the task to execute concurrently to other tasks. The
    /// spawned task may execute on the current thread, or it may be sent to a
    /// different thread to be executed.
    ///
    /// It is guaranteed that spawn will not synchronously poll the task being spawned.
    /// This means that calling spawn while holding a lock does not pose a risk of
    /// deadlocking with the spawned task.
    ///
    /// There is no guarantee that a spawned task will execute to completion.
    /// When a runtime is shutdown, all outstanding tasks are dropped,
    /// regardless of the lifecycle of that task.
    ///
    /// This function must be called from the context of a Tokio runtime. Tasks running on
    /// the Tokio runtime are always inside its context, but you can also enter the context
    /// using the [`Runtime::enter`](crate::runtime::Runtime::enter()) method.
    ///
    /// # Panics
    ///
    /// * Panics if called from **outside** of the Tokio runtime.
    /// * Panics if called after **awaiting** `Scope`
    pub fn spawn<F, R>(self: Pin<&mut Self>, f: F) -> JoinHandle<R>
    where
        F: AsyncFnOnceRef<State, R> + 'static,
        R: Send + 'static,
    {
        let this = self.project();

        // SAFETY:
        // Scoped will block if dropped before all the state refs are dropped
        let state = unsafe { this.aliased.as_ref().get_extended() };

        if *this.started_locking {
            panic!("spawn should not be called after awaiting the Scope handle")
        }

        // acquire the lock
        {
            let mut lock = state.lock.lock().unwrap();
            if lock.tasks < 0 {
                todo!(
                    "support waiting for exclusive tasks to complete before starting shared tasks"
                )
            }
            lock.tasks += 1;
        }

        // state.raw_lock.lock_shared();

        // SAFETY:
        // state will stay valid until the returned future gets dropped.
        let ptr = state.state.get();

        #[cfg(loom)]
        let future = {
            let future = f.call(ptr.with(|p| unsafe { &*p }));
            let ptr_guard = SendConstPtr(ptr);
            async move {
                let res = future.await;
                drop(ptr_guard);
                res
            }
        };

        #[cfg(not(loom))]
        let future = f.call(unsafe { &*ptr });

        let task = tokio::task::spawn(ScopedFuture {
            lock: &state.lock,
            condvar: &state.condvar,
            future: Some(future),
        });
        this.handles.push(task.abort_handle());
        task
    }
}

pin_project! {
    struct ScopedFuture<F> {
        lock: &'static Mutex<LockState>,
        condvar: &'static Condvar,
        #[pin]
        future: Option<F>,
    }

    impl<F> PinnedDrop for ScopedFuture<F> {
        fn drop(this: Pin<&mut Self>) {
            let mut this = this.project();
            this.future.set(None);

            // false positive clippy warning
            #[allow(clippy::mut_mutex_lock)]
            let waker = {
                let mut lock = this.lock.lock().unwrap();
                lock.tasks -= 1;
                if lock.tasks == 0 {
                    this.condvar.notify_one();
                    lock.waker.take()
                } else {
                    None
                }
            };

            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }
}

impl<F: Future> Future for ScopedFuture<F> {
    type Output = F::Output;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.project()
            .future
            .as_pin_mut()
            .expect("should not be polled after drop")
            .poll(cx)
    }
}

pub trait AsyncFnOnceRef<S, R> {
    fn call(self, state: &S) -> impl Send + Future<Output = R>;
}

trait MyFnOnce<Arg>: FnOnce(Arg) -> Self::Ret {
    type Ret;
}
impl<F: FnOnce(A) -> R, A, R> MyFnOnce<A> for F {
    type Ret = R;
}

impl<S: 'static, F, R: Send + 'static> AsyncFnOnceRef<S, R> for F
where
    F: 'static + for<'state> MyFnOnce<&'state S>,
    for<'state> <F as MyFnOnce<&'state S>>::Ret: Send + Future<Output = R>,
{
    fn call(self, state: &S) -> impl Send + Future<Output = R> {
        (self)(state)
    }
}

#[cfg(loom)]
#[allow(dead_code)]
/// needed to track the UnsafeCell immutable borrow when we send it across threads in the tokio::spawn
struct SendConstPtr<T>(loom::cell::ConstPtr<T>);
#[cfg(loom)]
unsafe impl<T> Send for SendConstPtr<T> {}

#[cfg(test)]
mod tests {
    use std::{pin::pin, sync::Mutex, task::Context};

    use futures_util::{task::noop_waker_ref, Future};
    use tokio::task::yield_now;

    use crate::Scope;

    async fn run(n: u64) -> u64 {
        let mut scoped = pin!(Scope::new(Mutex::new(0)));
        struct Ex(u64);
        impl super::AsyncFnOnceRef<Mutex<u64>, ()> for Ex {
            async fn call(self, state: &Mutex<u64>) {
                let i = self.0;
                tokio::time::sleep(tokio::time::Duration::from_millis(10 * i)).await;
                *state.lock().unwrap() += 1;
                tokio::time::sleep(tokio::time::Duration::from_millis(10 * i)).await;
            }
        }

        for i in 0..n {
            scoped.as_mut().spawn(Ex(i));
        }

        scoped.await.into_inner().unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn scoped() {
        assert_eq!(run(64).await, 64);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropped() {
        let mut task = pin!(run(64));
        assert!(task
            .as_mut()
            .poll(&mut Context::from_waker(noop_waker_ref()))
            .is_pending());
        yield_now().await;
    }
}
