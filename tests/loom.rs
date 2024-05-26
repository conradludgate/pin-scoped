#![cfg(loom)]

use futures_util::{future::BoxFuture, task::noop_waker_ref, FutureExt};
use loom::sync::{Arc, Mutex};
use std::{
    future::{poll_fn, Future},
    marker::PhantomData,
    pin::pin,
    task::{Context, Waker},
};

use pin_scoped::{Runtime, Scope};

#[test]
fn scoped() {
    loom::model(|| {
        let rt = Rt::default();

        let worker_thread = rt.spawn_worker();

        let val = loom::future::block_on(run(2, rt.clone()));
        assert_eq!(val, 2);

        rt.shutdown();
        worker_thread.join().unwrap();
    });
}

#[test]
fn dropped() {
    loom::model(|| {
        let rt = Rt::default();

        let worker_thread = rt.spawn_worker();

        {
            let mut task = pin!(run(2, rt.clone()));
            let _ = task
                .as_mut()
                .poll(&mut Context::from_waker(noop_waker_ref()));
            loom::thread::yield_now();
        }

        rt.shutdown();
        worker_thread.join().unwrap();
    });
}

async fn run(n: u32, rt: impl Runtime) -> u64 {
    let mut scoped = pin!(Scope::with_runtime(Mutex::new(0), rt));
    struct Ex;
    impl pin_scoped::AsyncFnOnceRef<Mutex<u64>, ()> for Ex {
        async fn call(self, state: &Mutex<u64>) {
            yield_now().await;
            *state.lock().unwrap() += 1;
            yield_now().await;
        }
    }

    for _ in 0..n {
        scoped.as_mut().spawn(Ex);
    }

    scoped.await.into_inner().unwrap()
}

struct AsyncChannelInner<T> {
    messages: Vec<T>,
    // waker for the recv
    waker: Option<Waker>,
    closed: bool,
}

impl<T> Default for AsyncChannelInner<T> {
    fn default() -> Self {
        AsyncChannelInner {
            messages: Default::default(),
            waker: Default::default(),
            closed: Default::default(),
        }
    }
}

/// a really dumb async channel that is safe to use with loom
struct AsyncChannel<T> {
    inner: Arc<Mutex<AsyncChannelInner<T>>>,
}
impl<T> Default for AsyncChannel<T> {
    fn default() -> Self {
        AsyncChannel {
            inner: Default::default(),
        }
    }
}
impl<T> Clone for AsyncChannel<T> {
    fn clone(&self) -> Self {
        AsyncChannel {
            inner: self.inner.clone(),
        }
    }
}

#[derive(Default, Clone)]
struct Rt {
    task_queue: AsyncChannel<BoxFuture<'static, ()>>,
}

impl Runtime for Rt {
    type JoinHandle<R> = PhantomData<R>;
    type AbortHandle = ();

    fn spawn<F>(&self, future: F) -> Self::JoinHandle<F::Output>
    where
        F: futures_util::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let mut queue = self.task_queue.inner.lock().unwrap();
        queue.messages.push(
            async move {
                future.await;
            }
            .boxed(),
        );
        if let Some(waker) = queue.waker.take() {
            waker.wake();
        }
        PhantomData
    }

    fn abort_handle<R>(_handle: &Self::JoinHandle<R>) -> Self::AbortHandle {}

    fn abort(_handle: Self::AbortHandle) {}

    fn block_in_place<F, R>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        f()
    }
}

impl Rt {
    fn spawn_worker(&self) -> loom::thread::JoinHandle<()> {
        let queue = self.task_queue.clone();
        let mut futures = Vec::<BoxFuture<'static, ()>>::new();
        loom::thread::spawn(move || {
            loom::future::block_on(poll_fn(|cx| {
                let done = {
                    let mut tasks = queue.inner.lock().unwrap();
                    tasks.waker = Some(cx.waker().clone());

                    for task in tasks.messages.drain(..) {
                        futures.push(task);
                    }

                    tasks.closed
                };

                let mut i = 0;
                while i < futures.len() {
                    match futures[i].poll_unpin(cx) {
                        std::task::Poll::Pending => i += 1,
                        std::task::Poll::Ready(()) => {
                            drop(futures.remove(i));
                        }
                    }
                }

                if done && futures.is_empty() {
                    std::task::Poll::Ready(())
                } else {
                    std::task::Poll::Pending
                }
            }))
        })
    }

    fn shutdown(&self) {
        let mut queue = self.task_queue.inner.lock().unwrap();
        queue.closed = true;
        if let Some(waker) = queue.waker.take() {
            waker.wake();
        }
    }
}

async fn yield_now() {
    poll_fn(|cx| {
        cx.waker().wake_by_ref();
        std::task::Poll::Ready(())
    })
    .await
}
