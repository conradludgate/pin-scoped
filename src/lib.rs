use futures_util::{task::AtomicWaker, Future};
use pin_project_lite::pin_project;
use pinned_aliasable::Aliasable;
use send_rwlock::RawRwLock;
use std::{cell::UnsafeCell, mem::ManuallyDrop, pin::Pin};
use tokio::task::{AbortHandle, JoinHandle};

mod send_rwlock;

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
    }

    impl<State: 'static> PinnedDrop for Scope<State> {
        #[inline(never)]
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            let aliased = this.aliased.as_ref().get();

            if this.handles.is_empty() {
                // lock and drop the state, if it's not already locked
                if aliased.raw_lock.try_lock_exclusive() {
                    unsafe { ManuallyDrop::drop(&mut *aliased.state.get()) };
                }
                return;
            }

            for handle in this.handles.drain(..) {
                handle.abort();
            }

            // lock and drop the state.
            tokio::task::block_in_place(|| aliased.raw_lock.lock_exclusive());
            unsafe { ManuallyDrop::drop(&mut *aliased.state.get()) };
        }
    }
}

// cannot be deallocated while read lock is acquired
struct SharedState<State> {
    // read locks imply that tasks are still in flight
    // write lock implies that the state has been taken out of the scope
    raw_lock: RawRwLock,
    // only dropped if write lock is acquired.
    state: UnsafeCell<ManuallyDrop<State>>,
    waker: AtomicWaker,
}

impl<State> Future for Scope<State> {
    type Output = State;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<State> {
        let this = self.project();

        let state = {
            let aliased = this.aliased.as_ref().get();

            // register first to prevent race condition
            aliased.waker.register(cx.waker());
            if !aliased.raw_lock.try_lock_exclusive() {
                return std::task::Poll::Pending;
            }

            // take the state now it's locked
            unsafe { ManuallyDrop::take(&mut *aliased.state.get()) }
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
                state: UnsafeCell::new(ManuallyDrop::new(state)),
                waker: AtomicWaker::new(),
                raw_lock: RawRwLock::new(),
            }),
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

        // acquire the lock
        assert!(
            state.raw_lock.try_lock_shared(),
            "spawn should not be called after awaiting the Scope handle"
        );

        // SAFETY:
        // state will stay valid until the returned future gets dropped.
        let future = f.call(unsafe { &*state.state.get() });
        let task = tokio::task::spawn(ScopedFuture {
            raw_lock: &state.raw_lock,
            waker: &state.waker,
            future: Some(future),
        });
        this.handles.push(task.abort_handle());
        task
    }
}

pin_project! {
    struct ScopedFuture<F> {
        raw_lock: &'static RawRwLock,
        waker: &'static AtomicWaker,
        #[pin]
        future: Option<F>,
    }

    impl<F> PinnedDrop for ScopedFuture<F> {
        fn drop(this: Pin<&mut Self>) {
            let mut this = this.project();
            this.future.set(None);
            let waker = this.waker.take();
            unsafe { this.raw_lock.unlock_shared(); }

            // wake scope after unlocking
            // todo: only do this if unlock_shared became exclusive
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
