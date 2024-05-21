use futures_util::{task::AtomicWaker, Future};
use pin_project_lite::pin_project;
use pinned_aliasable::Aliasable;
use send_rwlock::RawRwLock;
use std::{cell::UnsafeCell, mem::ManuallyDrop, pin::Pin};
use tokio::task::{AbortHandle, JoinHandle};

mod send_rwlock;

pin_project! {
    pub struct Scoped<State: 'static> {
        handles: Vec<AbortHandle>,

        #[pin]
        aliased: Aliasable<SharedState<State>>,
    }

    impl<State: 'static> PinnedDrop for Scoped<State> {
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
            aliased.raw_lock.lock_exclusive();
            unsafe { ManuallyDrop::drop(&mut *aliased.state.get()) };
        }
    }
}

struct SharedState<State> {
    raw_lock: RawRwLock,
    state: UnsafeCell<ManuallyDrop<State>>,
    waker: AtomicWaker,
}

impl<State> Future for Scoped<State> {
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

impl<State: 'static + Sync> Scoped<State> {
    pub fn new(state: State) -> Self {
        Scoped {
            handles: vec![],
            aliased: Aliasable::new(SharedState {
                state: UnsafeCell::new(ManuallyDrop::new(state)),
                waker: AtomicWaker::new(),
                raw_lock: RawRwLock::new(),
            }),
        }
    }

    /// # Panics
    /// Panics if called after awaiting/polling the state starts
    pub fn spawn<F, R>(self: Pin<&mut Self>, f: F) -> JoinHandle<R>
    where
        F: for<'state> AsyncFnOnceRef<'state, State, R>,
        R: Send + 'static,
    {
        let this = self.project();

        // SAFETY:
        // Scoped will block if dropped before all the state refs are dropped
        let state = unsafe { this.aliased.as_ref().get_extended() };

        // acquire the lock
        assert!(
            state.raw_lock.try_lock_shared(),
            "spawn should not be called after polling the Scoped handle"
        );

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

            // first, drop the future
            this.future.set(None);

            this.waker.wake();
            unsafe { this.raw_lock.unlock_shared() }
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

#[doc(hidden)]
pub trait Captures<U> {}
impl<T: ?Sized, U> Captures<U> for T {}

pub trait AsyncFnOnceRef<'state, S: 'static, R: Send + 'static>: 'static {
    fn call(self, state: &'state S) -> impl Future<Output = R> + Captures<&'state S> + Send;
}

impl<'state, S: 'static, F, Fut, R: Send + 'static> AsyncFnOnceRef<'state, S, R> for F
where
    F: std::ops::FnOnce(&'state S) -> Fut + 'static,
    Fut: Future<Output = R> + Send + Captures<&'state S>,
{
    fn call(self, state: &'state S) -> impl Future<Output = R> + Captures<&'state S> + Send {
        (self)(state)
    }
}

#[cfg(test)]
mod tests {
    use std::{pin::pin, sync::Mutex, task::Context};

    use futures_util::{task::noop_waker_ref, Future};
    use tokio::task::yield_now;

    use crate::Scoped;

    async fn run(n: u64) -> u64 {
        let mut scoped = pin!(Scoped::new(Mutex::new(0)));
        struct Ex(u64);
        impl<'state> super::AsyncFnOnceRef<'state, Mutex<u64>, ()> for Ex {
            async fn call(self, state: &'state Mutex<u64>) {
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
