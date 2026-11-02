#![cfg(not(pin_scoped_loom))]

use std::{
    future::Future,
    panic::{catch_unwind, AssertUnwindSafe},
    pin::pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex, MutexGuard,
    },
    task::{Context, Poll, Wake, Waker},
    thread,
};

use futures_util::future::{AbortHandle, Abortable, Aborted};
use pin_scoped::{Runtime, Scope, ScopeState, Stack};

struct Unpark(thread::Thread);
impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}

#[derive(Clone, Copy)]
enum Failure {
    None,
    Clone,
    Spawn,
    #[cfg_attr(miri, allow(dead_code))] // Only the native subprocess test aborts.
    Abort,
}

struct Threads(Failure);
impl Clone for Threads {
    fn clone(&self) -> Self {
        assert!(!matches!(self.0, Failure::Clone), "runtime clone failed");
        Self(self.0)
    }
}

impl Runtime for Threads {
    type JoinHandle<R: Send + 'static> = thread::JoinHandle<Result<R, Aborted>>;
    type AbortHandle = AbortHandle;

    fn spawn<F>(&self, future: F) -> (Self::JoinHandle<F::Output>, Self::AbortHandle)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        assert!(!matches!(self.0, Failure::Spawn), "runtime spawn failed");
        let (abort, registration) = AbortHandle::new_pair();
        let handle = thread::spawn(move || block_on(Abortable::new(future, registration)));
        (handle, abort)
    }

    fn abort(&self, handle: Self::AbortHandle) {
        assert!(!matches!(self.0, Failure::Abort), "runtime abort failed");
        handle.abort();
    }

    fn block_in_place<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        f()
    }
}

#[test]
fn custom_runtime_returns_native_handles_and_owned_state() {
    let scope = pin!(Scope::with_runtime(
        AtomicUsize::new(0),
        Threads(Failure::None)
    ));
    let handle = scope
        .as_ref()
        .spawn(async |state: Stack![AtomicUsize; Threads]| {
            state.fetch_add(1, Ordering::SeqCst);
            42
        });
    assert_eq!(handle.join().unwrap().unwrap(), 42);
    assert_eq!(block_on(scope).into_inner(), 1);
}

#[test]
fn custom_runtime_supports_nested_scopes() {
    let scope = pin!(Scope::with_runtime(
        AtomicUsize::new(0),
        Threads(Failure::None)
    ));
    scope
        .as_ref()
        .spawn(async |state: Stack![AtomicUsize; Threads]| {
            let nested = pin!(state.nest(7_usize));
            nested
                .as_ref()
                .spawn(async |state: Stack![usize, AtomicUsize; Threads]| {
                    state.parent_scope().fetch_add(**state, Ordering::SeqCst);
                });
            assert_eq!(nested.await, 7);
        });
    assert_eq!(block_on(scope).into_inner(), 7);
}

#[test]
fn non_send_state_is_reowned_and_dropped_on_its_owner_thread() {
    static VALUE: Mutex<usize> = Mutex::new(0);
    let guard = VALUE.lock().unwrap();
    let scope = pin!(Scope::with_runtime(guard, Threads(Failure::None)));
    scope.as_ref().spawn(
        async |state: ScopeState<'_, MutexGuard<'static, usize>, Threads>| {
            assert_eq!(**state, 0);
        },
    );
    let mut guard = block_on(scope);
    *guard = 1;
    drop(guard);
    assert_eq!(*VALUE.lock().unwrap(), 1);
}

#[test]
fn custom_runtime_cancellation_finishes_before_scope_drop_returns() {
    struct Dropped(Arc<AtomicUsize>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let observed = drops.clone();
    let scope = Box::pin(Scope::with_runtime((), Threads(Failure::None)));
    let (started, ready) = mpsc::channel();
    let handle = scope
        .as_ref()
        .spawn(async move |state: ScopeState<'_, (), Threads>| {
            let guard = Dropped(observed);
            started.send(()).unwrap();
            std::future::pending::<()>().await;
            std::hint::black_box((state, guard));
        });
    ready.recv().unwrap();
    drop(scope);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(handle.join().unwrap().is_err());
}

#[test]
fn runtime_can_destroy_a_task_synchronously_during_abort() {
    #[derive(Clone)]
    struct DropOnAbort;
    impl Runtime for DropOnAbort {
        type JoinHandle<R: Send + 'static> = std::marker::PhantomData<R>;
        type AbortHandle = Box<dyn Send>;

        fn spawn<F>(&self, future: F) -> (Self::JoinHandle<F::Output>, Self::AbortHandle)
        where
            F: Future + Send + 'static,
            F::Output: Send + 'static,
        {
            (std::marker::PhantomData, Box::new(future))
        }

        fn abort(&self, handle: Self::AbortHandle) {
            // Destruction releases a scope registration and reacquires its lock.
            drop(handle);
        }

        fn block_in_place<F, R>(&self, _: F) -> R
        where
            F: FnOnce() -> R,
        {
            panic!("synchronous cancellation should leave no registrations");
        }
    }

    let scope = Box::pin(Scope::with_runtime((), DropOnAbort));
    scope
        .as_ref()
        .spawn(async |state: ScopeState<'_, (), DropOnAbort>| {
            std::future::pending::<()>().await;
            std::hint::black_box(state);
        });
    assert_eq!(scope.as_ref().num_tasks(), 1);
    drop(scope);
}

#[test]
fn runtime_spawn_panic_rolls_back_registration() {
    let scope = pin!(Scope::with_runtime((), Threads(Failure::Spawn)));
    assert!(catch_unwind(AssertUnwindSafe(|| {
        scope
            .as_ref()
            .spawn(async |_: ScopeState<'_, (), Threads>| {});
    }))
    .is_err());
    assert_eq!(scope.as_ref().num_tasks(), 0);
    block_on(scope);
}

#[test]
fn runtime_clone_panic_during_nesting_keeps_registration_consistent() {
    let scope = pin!(Scope::with_runtime((), Threads(Failure::Clone)));
    let handle = scope
        .as_ref()
        .spawn(async |state: ScopeState<'_, (), Threads>| {
            assert!(catch_unwind(AssertUnwindSafe(|| state.nest(()))).is_err());
        });
    handle.join().unwrap().unwrap();
    assert_eq!(scope.as_ref().num_tasks(), 0);
    block_on(scope);
}

#[cfg(not(miri))]
#[test]
fn runtime_cleanup_panic_is_contained() {
    const CHILD: &str = "PIN_SCOPED_RUNTIME_ABORT_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let scope = Box::pin(Scope::with_runtime((), Threads(Failure::Abort)));
        scope
            .as_ref()
            .spawn(async |_: ScopeState<'_, (), Threads>| {
                std::future::pending::<()>().await;
            });
        drop(scope);
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runtime_cleanup_panic_is_contained",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(output.status.signal(), Some(6), "{output:?}");
    }
    #[cfg(not(unix))]
    assert!(!output.status.success(), "{output:?}");
}
