//! Executor integration for scoped tasks.
//!
//! Tokio is enabled by default. Other executors implement [`Runtime`] and are
//! selected with [`Scope::with_runtime`](crate::Scope::with_runtime). The scope
//! remains awaitable; individual tasks retain the executor's native handles.
//!
//! The default `tokio` feature provides the `TokioScope<State>` alias and its
//! `new` constructor. Other scope and callback types take an explicit runtime
//! parameter. For example, `ScopeState<'_, State, tokio::runtime::Handle>` is a
//! Tokio task's view of its scope's state.
//!
//! With default features disabled, construct scopes with `Scope::with_runtime`.
//! This interface currently uses std, Send tasks and blocking cleanup;
//! local-task/no_std support is separate.

use std::future::Future;

/// The executor operations used by a scope.
///
/// This is a safe trait: scoped lifetime accounting stays inside this crate.
/// An executor that retains a future indefinitely can prevent scope completion.
/// A panic during cancellation cleanup causes the scope to abort the process
/// rather than unwind past outstanding registrations.
pub trait Runtime: Clone + Send + Sync + 'static {
    /// The executor's native task handle, with its own joining/error semantics.
    type JoinHandle<R: Send + 'static>;

    /// A cancellation token for a spawned task.
    type AbortHandle: Send + 'static;

    /// Takes ownership of a future and schedules it for execution.
    ///
    /// Return the native task handle and its cancellation token. Tasks must
    /// continue making progress while the scope's owner waits in
    /// [`block_in_place`](Self::block_in_place).
    fn spawn<F>(&self, future: F) -> (Self::JoinHandle<F::Output>, Self::AbortHandle)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static;

    /// Requests cancellation and consumes the token.
    ///
    /// The scope independently waits for the future to be destroyed. This call
    /// does not need to synchronously finish cancellation.
    fn abort(&self, handle: Self::AbortHandle);

    /// Runs blocking cleanup on the calling thread while tasks can progress.
    ///
    /// The closure is deliberately not required to be Send: the owner may hold
    /// state whose destruction must remain on this thread. An unsupported
    /// blocking context may panic, invoking the scope's cleanup containment.
    fn block_in_place<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R;
}

#[cfg(feature = "tokio")]
impl Runtime for tokio::runtime::Handle {
    type JoinHandle<R: Send + 'static> = tokio::task::JoinHandle<R>;
    type AbortHandle = tokio::task::AbortHandle;

    fn spawn<F>(&self, future: F) -> (Self::JoinHandle<F::Output>, Self::AbortHandle)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handle = tokio::runtime::Handle::spawn(self, future);
        let abort = handle.abort_handle();
        (handle, abort)
    }

    fn abort(&self, handle: Self::AbortHandle) {
        handle.abort();
    }

    fn block_in_place<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        tokio::task::block_in_place(f)
    }
}
