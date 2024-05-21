#![feature(async_fn_traits)]

use futures_util::{future::BoxFuture, task::AtomicWaker, Future};
use parking_lot_core::{ParkResult, ParkToken, UnparkToken};
use pin_project::{pin_project, pinned_drop};
use pinned_aliasable::Aliasable;
use std::{
    num::NonZeroUsize,
    pin::Pin,
    process::abort,
    sync::atomic::{AtomicUsize, Ordering},
};
use tokio::task::{AbortHandle, JoinHandle};

#[pin_project(PinnedDrop)]
pub struct Scoped<State: 'static> {
    handles: Vec<AbortHandle>,
    #[pin]
    aliased: Option<Aliasable<ScopedAliased<State>>>,

    key: Option<NonZeroUsize>,
}

#[pinned_drop]
impl<State: 'static> PinnedDrop for Scoped<State> {
    #[inline(never)]
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();

        let Some(key) = this.key else {
            // scope handle was consumed
            return;
        };

        for handle in this.handles.drain(..) {
            handle.abort();
        }

        // avoid touching aliasable, go through key pointer instead
        // SAFETY: key was initiated from `AtomicUsize::to_ptr`.
        // the allocation it points to is owned by self, and is therefore still valid
        let tasks = unsafe { AtomicUsize::from_ptr(key.get() as *mut usize) };
        if tasks.load(Ordering::Acquire) > 0 {
            // SAFETY:
            // 1. We control the aliased.tasks address
            // 2. validate and timed_out do not panic or call parking_lot functions
            // 3. before_sleep does not call park
            let res = tokio::task::block_in_place(|| unsafe {
                parking_lot_core::park(
                    tasks.as_ptr() as usize,
                    || true,
                    || {},
                    |_, _| {},
                    ParkToken(0),
                    None,
                )
            });
            match res {
                ParkResult::Invalid | ParkResult::TimedOut => abort(),
                ParkResult::Unparked(_) => {}
            }
        }
    }
}

struct ScopedAliased<State> {
    tasks: AtomicUsize,
    state: State,
    notification: AtomicWaker,
}

impl<State> Future for Scoped<State> {
    type Output = State;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<State> {
        let mut this = self.project();

        {
            let aliased = this
                .aliased
                .as_mut()
                .as_pin_mut()
                .expect("aliased state must be set while scoped is alive");

            // register first to prevent race condition
            aliased.as_ref().get().notification.register(cx.waker());
            if aliased.as_ref().get().tasks.load(Ordering::Acquire) > 0 {
                return std::task::Poll::Pending;
            }

            _ = this.key.take();
        }

        // SAFETY: this is no longer aliased
        let mut unaliased = unsafe {
            this.aliased
                .get_unchecked_mut()
                .take()
                .unwrap()
                .into_inner()
        };
        assert_eq!(*unaliased.tasks.get_mut(), 0);
        std::task::Poll::Ready(unaliased.state)
    }
}

impl<State: 'static + Sync> Scoped<State> {
    pub fn new(state: State) -> Self {
        Scoped {
            handles: vec![],
            aliased: Some(Aliasable::new(ScopedAliased {
                tasks: AtomicUsize::new(0),
                state,
                notification: AtomicWaker::new(),
            })),
            key: None,
        }
    }

    pub fn spawn<F, R>(self: Pin<&mut Self>, f: F) -> JoinHandle<R>
    where
        F: for<'state> FnOnce(&'state State) -> BoxFuture<'state, R>,
        R: Send + 'static,
    {
        let this = self.project();
        let aliased = this
            .aliased
            .as_pin_mut()
            .expect("aliased state must be set while scoped is alive");

        let addr = NonZeroUsize::new(aliased.as_ref().get().tasks.as_ptr() as usize)
            .expect("refs should never be null");
        if let Some(old_addr) = this.key.replace(addr) {
            if old_addr != addr {
                abort()
            };
        }

        // acquire the lock
        aliased.as_ref().get().tasks.fetch_add(1, Ordering::Release);

        // SAFETY:
        // Scoped will block if dropped before all the state refs are dropped
        let state = unsafe { aliased.as_ref().get_extended() };
        let future = (f)(&state.state);
        let task = tokio::task::spawn(ScopedFuture {
            state,
            future: Some(future),
        });
        this.handles.push(task.abort_handle());
        task
    }
}

#[pin_project(PinnedDrop)]
struct ScopedFuture<State: 'static, F> {
    state: &'static ScopedAliased<State>,
    #[pin]
    future: Option<F>,
}

#[pinned_drop]
impl<State: 'static, F> PinnedDrop for ScopedFuture<State, F> {
    fn drop(self: Pin<&mut Self>) {
        let mut this = self.project();

        // first, drop the future
        this.future.set(None);

        if this.state.tasks.fetch_sub(1, Ordering::Release) == 1 {
            this.state.notification.wake();
            // Wake up the owner on release of the last task
            // SAFETY: We control the aliased.tasks address
            unsafe {
                parking_lot_core::unpark_all(this.state.tasks.as_ptr() as usize, UnparkToken(0))
            };
        }
    }
}

impl<State: 'static, F: Future> Future for ScopedFuture<State, F> {
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

// pub trait AsyncFnOnce<S>:
//     FnOnce(S) -> Self::Future + 'static
// {
//     type Future: Future<Output = Self::Output> + Captures<S> + Send;
//     type Output;
// }

// impl<F, Fut, S> AsyncFnOnce<S> for F where
//     F: FnOnce(S) -> Self::Future + 'static,
//     Fut: Future + 'static,
// {
//     type Future = Fut;
//     type Output = Fut::Output;
// }
// pub trait MyAsyncFnOnce<'state, S: 'static>:
//     AsyncFnOnce(&'state S) -> Self::OutputVal + 'static
// where
//     Self::CallOnceFuture: Captures<&'state S> + Send,
// {
//     type Future: Captures<&'state S> + Send;
//     type OutputVal: Send + 'static;
// }

// impl<'state, F: ?Sized, Fut, S: 'static> MyAsyncFnOnce<'state, S> for F
// where
//     F: AsyncFnOnce(&'state S) -> Fut + 'static,
//     Fut: Future + Send + Captures<&'state S> + 'fut,
// {
//     type Future = Fut;
//     type Output = Fut::Output;
// }

#[cfg(test)]
mod tests {
    use std::{pin::pin, sync::Mutex, task::Context};

    use futures_util::{task::noop_waker_ref, Future, FutureExt};
    use tokio::task::yield_now;

    use crate::Scoped;

    async fn run(n: u64) -> u64 {
        let mut scoped = pin!(Scoped::new(Mutex::new(0)));

        for i in 0..n {
            scoped.as_mut().spawn(move |state: &Mutex<u64>| {
                async move {
                    tokio::time::sleep(tokio::time::Duration::from_millis(10 * i)).await;
                    *state.lock().unwrap() += 1;
                    tokio::time::sleep(tokio::time::Duration::from_millis(10 * i)).await;
                }
                .boxed()
            });
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
