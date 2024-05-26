use pin_project_lite::pin_project;
use pinned_aliasable::Aliasable;
use std::future::Future;
use std::{mem::ManuallyDrop, pin::Pin, task::Waker};

mod sync;
use sync::{Condvar, ConstPtr, ManuallyDropCell, Mutex};

pub mod tokio;

pub trait Runtime {
    type JoinHandle<R>;
    type AbortHandle;

    fn spawn<F>(&self, future: F) -> Self::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static;

    fn abort_handle<R>(handle: &Self::JoinHandle<R>) -> Self::AbortHandle;
    fn abort(handle: Self::AbortHandle);

    fn block_in_place<F, R>(f: F) -> R
    where
        F: FnOnce() -> R;
}

pin_project! {
    /// Scope represents a scope holding some values.
    ///
    /// [`tokio`] tasks can be spawned in the context of this scope.
    ///
    /// Should the scope be dropped before those tasks complete,
    /// the tasks will be [`aborted`](JoinHandle::abort) and the runtime
    /// thread will block until all tasks have dropped.
    pub struct Scope<State: 'static, Rt: Runtime> {
        handles: Vec<Rt::AbortHandle>,

        #[pin]
        aliased: Aliasable<SharedState<State>>,

        started_locking: bool,

        runtime: Rt,
    }

    impl<State: 'static, Rt: Runtime> PinnedDrop for Scope<State, Rt> {
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
                    unsafe { aliased.state.drop() };
                }
                return;
            }

            for handle in this.handles.drain(..) {
                Rt::abort(handle);
            }

            // lock and drop the state.
            let mut lock = aliased.lock.lock().unwrap();
            while lock.tasks != 0 {
                lock.parked = true;
                lock = aliased.condvar.wait(lock).unwrap();
                lock.parked = false;
                debug_assert!(lock.tasks != -1, "state should not be dropped");
            }
            unsafe { aliased.state.drop() };
        }
    }
}

struct SharedState<State> {
    lock: Mutex<LockState>,
    condvar: Condvar,

    /// initialised if and only if lock.tasks != -1
    state: ManuallyDropCell<State>,
}

struct LockState {
    /// positive for read-only tasks
    /// isize::MIN for a single mut task
    /// -1 for when the state is removed
    tasks: isize,
    parked: bool,
    waker: Option<Waker>,
}

impl<State, Rt: Runtime> Future for Scope<State, Rt> {
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

            debug_assert_eq!(lock.tasks, 0, "state should not be dropped");

            // take the state now it's locked
            lock.tasks = -1;

            unsafe { aliased.state.take() }
        };

        this.handles.clear();

        std::task::Poll::Ready(state)
    }
}

pub type TokioScope<State> = Scope<State, tokio::Global>;
impl<State: 'static + Sync, Rt: Runtime + Default> Scope<State, Rt> {
    pub fn new(state: State) -> Self {
        Scope::with_runtime(state, Rt::default())
    }
}

impl<State: 'static + Sync, Rt: Runtime> Scope<State, Rt> {
    pub fn with_runtime(state: State, rt: Rt) -> Self {
        Scope {
            handles: vec![],
            aliased: Aliasable::new(SharedState {
                lock: Mutex::new(LockState {
                    waker: None,
                    parked: false,
                    tasks: 0,
                }),
                condvar: Condvar::new(),
                state: ManuallyDropCell::new(state),
            }),
            started_locking: false,
            runtime: rt,
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
    pub fn spawn<F, R>(self: Pin<&mut Self>, f: F) -> Rt::JoinHandle<R>
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
        let (val, ptr_guard) = unsafe { state.state.borrow() };
        let future = f.call(val);

        let task = this.runtime.spawn(ScopedFuture {
            lock: &state.lock,
            condvar: &state.condvar,
            future: Some(future),
            ptr_guard: Some(SendWrapper(ptr_guard)),
        });
        this.handles.push(Rt::abort_handle(&task));
        task
    }
}

pin_project! {
    struct ScopedFuture<F, S> {
        lock: &'static Mutex<LockState>,
        condvar: &'static Condvar,
        #[pin]
        future: Option<F>,
        ptr_guard: Option<SendWrapper<ManuallyDrop<S>>>,
    }

    impl<F, S> PinnedDrop for ScopedFuture<F, S> {
        fn drop(this: Pin<&mut Self>) {
            let mut this = this.project();
            this.future.set(None);

            // false positive clippy warning
            #[allow(clippy::mut_mutex_lock)]
            let waker = {
                let mut lock = this.lock.lock().unwrap();

                *this.ptr_guard = None;

                lock.tasks -= 1;
                if lock.tasks == 0 {
                    if lock.parked {
                        this.condvar.notify_one();
                    }
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

impl<F: Future, S> Future for ScopedFuture<F, S> {
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

#[allow(dead_code)]
struct SendWrapper<S>(ConstPtr<S>);
unsafe impl<T> Send for SendWrapper<T> {}

#[cfg(all(test, not(loom)))]
mod tests {
    use crate::{sync::Mutex, Runtime};
    use std::{future::Future, pin::pin, task::Context, time::Duration};

    use futures_util::task::noop_waker_ref;
    use tokio::task::yield_now;

    use crate::Scope;

    async fn run(n: u32, rt: impl Runtime) -> u64 {
        let mut scoped = pin!(Scope::with_runtime(Mutex::new(0), rt));
        struct Ex(u32);
        impl super::AsyncFnOnceRef<Mutex<u64>, ()> for Ex {
            async fn call(self, state: &Mutex<u64>) {
                let i = self.0;
                tokio::time::sleep(Duration::from_millis(10) * i).await;
                *state.lock().unwrap() += 1;
                tokio::time::sleep(Duration::from_millis(10) * i).await;
            }
        }

        for i in 0..n {
            scoped.as_mut().spawn(Ex(i));
        }

        scoped.await.into_inner().unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn scoped() {
        assert_eq!(run(64, crate::tokio::Global).await, 64);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropped() {
        let mut task = pin!(run(64, crate::tokio::Global));
        assert!(task
            .as_mut()
            .poll(&mut Context::from_waker(noop_waker_ref()))
            .is_pending());
        yield_now().await;
    }
}
