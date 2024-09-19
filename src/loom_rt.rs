use loom::sync::{Arc, Mutex};
use std::{
    cell::OnceCell,
    future::{poll_fn, Future},
    marker::PhantomData,
    pin::Pin,
    task::Waker,
};

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

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

loom::thread_local! {
    static GLOBAL_RT: OnceCell<Handle> = OnceCell::new();
}

#[derive(Default, Clone)]
pub struct Handle {
    task_queue: AsyncChannel<BoxFuture<'static, ()>>,
}

pub struct JoinHandle<R>(PhantomData<R>, AbortHandle);

impl<R> JoinHandle<R> {
    pub fn abort_handle(&self) -> AbortHandle {
        self.1.clone()
    }

    pub fn abort(&self) {}
}

#[derive(Clone)]
pub struct AbortHandle;

impl AbortHandle {
    pub fn abort(&self) {}
}

impl Handle {
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let mut queue = self.task_queue.inner.lock().unwrap();
        queue.messages.push(Box::pin(async move {
            future.await;
        }) as _);
        if let Some(waker) = queue.waker.take() {
            waker.wake();
        }
        JoinHandle(PhantomData, AbortHandle)
    }
}

impl Handle {
    pub fn current() -> Self {
        GLOBAL_RT.with(|x| x.get_or_init(Self::default).clone())
    }

    pub fn spawn_worker(&self) -> loom::thread::JoinHandle<()> {
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
                    match futures[i].as_mut().poll(cx) {
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

    pub fn shutdown(&self) {
        let mut queue = self.task_queue.inner.lock().unwrap();
        queue.closed = true;
        if let Some(waker) = queue.waker.take() {
            waker.wake();
        }
    }
}

pub async fn yield_now() {
    poll_fn(|cx| {
        cx.waker().wake_by_ref();
        std::task::Poll::Ready(())
    })
    .await
}

pub fn block_in_place<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    f()
}
