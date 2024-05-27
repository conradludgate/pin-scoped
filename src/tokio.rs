use core::future::Future;

use crate::Runtime;

#[derive(Clone, Copy, Default)]
pub struct Global;

impl Runtime for Global {
    type JoinHandle<R> = tokio::task::JoinHandle<R>;
    type AbortHandle = tokio::task::AbortHandle;

    fn spawn<F>(&self, future: F) -> Self::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        tokio::task::spawn(future)
    }

    fn abort_handle<R>(handle: &Self::JoinHandle<R>) -> Self::AbortHandle {
        handle.abort_handle()
    }
    fn abort(handle: Self::AbortHandle) {
        handle.abort()
    }

    fn block_in_place<F, R>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        tokio::task::block_in_place(f)
    }
}

pub struct Handle(pub tokio::runtime::Handle);

impl Runtime for Handle {
    type JoinHandle<R> = tokio::task::JoinHandle<R>;
    type AbortHandle = tokio::task::AbortHandle;

    fn spawn<F>(&self, future: F) -> Self::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.0.spawn(future)
    }

    fn abort_handle<R>(handle: &Self::JoinHandle<R>) -> Self::AbortHandle {
        handle.abort_handle()
    }
    fn abort(handle: Self::AbortHandle) {
        handle.abort()
    }

    fn block_in_place<F, R>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        tokio::task::block_in_place(f)
    }
}
