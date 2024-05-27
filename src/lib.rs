#![cfg_attr(docsrs, feature(doc_auto_cfg))]
#![no_std]

// temporary measure
extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

use core::future::Future;
use core::marker::PhantomPinned;
use core::mem::MaybeUninit;
use core::task::{Context, Poll};
use core::{mem::ManuallyDrop, pin::Pin, task::Waker};
use pin_project_lite::pin_project;

mod sync;
use sync::{Aliasable, GuardPtr, ManuallyDropCell, Mutex};

#[cfg(any(loom, feature = "std"))]
use sync::Condvar;

#[cfg(feature = "tokio")]
pub mod tokio;
#[cfg(feature = "tokio")]
pub type TokioScope<State> = Scope<State, tokio::Global>;

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

/// The state is removed from the scope
const REMOVED: isize = -SHARED;

const WAKER: isize = 0b001;
#[allow(dead_code)]
const PARKED: isize = 0b010;
const SHARED: isize = 0b100;

// /// The state is exclusively acquired from the scope
// const EXCLUSIVE: usize = -1;

pin_project! {
    /// Scope represents a scope holding some values.
    ///
    /// [`tokio`] tasks can be spawned in the context of this scope.
    ///
    /// Should the scope be dropped before those tasks complete,
    /// the tasks will be [`aborted`](JoinHandle::abort) and the runtime
    /// thread will block until all tasks have dropped.
    pub struct Scope<State: 'static, Rt: Runtime> {
        handles: alloc::vec::Vec<Rt::AbortHandle>,

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
                let mut lock = aliased.group.lock.lock().unwrap();
                _ = lock.take_waker();

                // waker is taken, thread is not parked.
                debug_assert!(lock.tasks == 0 || lock.tasks == REMOVED);

                // drop the state if it's set
                if lock.tasks == 0 {
                    lock.tasks = REMOVED;
                    // SAFETY:
                    // 0 tasks means we have exclusive access to state now.
                    // and it is currently initialised.
                    unsafe { aliased.state.drop() };
                }
                return;
            }

            for handle in this.handles.drain(..) {
                Rt::abort(handle);
            }

            // lock and drop the state.
            let mut lock = aliased.group.lock.lock().unwrap();
            while lock.tasks & REMOVED != 0 {
                #[cfg(any(loom, feature = "std"))]
                {
                    lock.tasks |= PARKED;
                    lock = aliased.group.condvar.wait(lock).unwrap();
                    lock.tasks &= !PARKED;
                }

                #[cfg(not(any(loom, feature = "std")))]
                {
                    drop(lock);
                    core::hint::spin_loop();
                    lock = aliased.group.lock.lock().unwrap();
                }

                debug_assert!(lock.tasks != REMOVED, "state should not be dropped");
            }

            _ = lock.take_waker();

            // waker is taken, thread is not parked.
            debug_assert!(lock.tasks == 0, "state should not be dropped or accessed by any tasks");
            lock.tasks = REMOVED;

            // SAFETY:
            // 0 tasks means we have exclusive access to state now.
            // and it is currently initialised.
            unsafe { aliased.state.drop() };
        }
    }
}

struct SharedState<State> {
    group: LockGroup,
    /// initialised if and only if group.lock.tasks != [`REMOVED`]
    state: ManuallyDropCell<State>,
}

struct LockGroup {
    lock: Mutex<LockState>,
    #[cfg(any(loom, feature = "std"))]
    condvar: Condvar,
}

struct LockState {
    /// positive for read-only tasks
    /// isize::MIN for a single mut task
    /// -1 for when the state is removed
    tasks: isize,

    /// Set when [`WAKER`] bit of `tasks` is set.
    waker: MaybeUninit<Waker>,
}

impl LockState {
    fn take_waker(&mut self) -> Option<Waker> {
        // drop the waker if it's set.
        if self.tasks & WAKER == WAKER {
            // unset the bit
            self.tasks &= !WAKER;

            // SAFETY: the waker bit was set, so this is init
            unsafe { Some(MaybeUninit::assume_init_read(&self.waker)) }
        } else {
            None
        }
    }

    fn register_waker(&mut self, waker: &Waker) {
        // the waker is set.
        if self.tasks & WAKER == WAKER {
            // SAFETY: the waker bit was set, so this is init
            unsafe { MaybeUninit::assume_init_mut(&mut self.waker).clone_from(waker) }
        } else {
            // set the waker bit
            self.tasks |= WAKER;
            self.waker.write(waker.clone());
        }
    }
}

impl<State, Rt: Runtime> Future for Scope<State, Rt> {
    type Output = State;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<State> {
        let this = self.project();
        *this.started_locking = true;

        let state = {
            // acquire access to the shared state
            let aliased = this.aliased.as_ref().get();
            let mut lock = aliased.group.lock.lock().unwrap();

            // if there are currently tasks running
            // return pending and register the waker.
            if lock.tasks & REMOVED != 0 {
                lock.register_waker(cx.waker());
                return Poll::Pending;
            }

            lock.take_waker();

            debug_assert!(
                lock.tasks == 0,
                "state should not be dropped or accessed by any tasks"
            );
            lock.tasks = REMOVED;

            // SAFETY:
            // 0 tasks means we have exclusive access to state now.
            // and it is currently initialised.
            unsafe { aliased.state.take() }
        };

        this.handles.clear();

        Poll::Ready(state)
    }
}

impl<State: 'static + Sync, Rt: Runtime + Default> Scope<State, Rt> {
    pub fn new(state: State) -> Self {
        Scope::with_runtime(state, Rt::default())
    }
}

impl<State: 'static + Sync, Rt: Runtime> Scope<State, Rt> {
    pub fn with_runtime(state: State, rt: Rt) -> Self {
        Scope {
            handles: alloc::vec::Vec::new(),
            aliased: Aliasable::new(SharedState {
                group: LockGroup {
                    lock: Mutex::new(LockState {
                        waker: MaybeUninit::uninit(),
                        tasks: 0,
                    }),
                    #[cfg(any(loom, feature = "std"))]
                    condvar: Condvar::new(),
                },
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

        if *this.started_locking {
            panic!("spawn should not be called after awaiting the Scope handle")
        }

        // SAFETY:
        // 1. `state` cannot outlive the returned futures.
        // 2. futures cannot outlive `Scope` as scope blocked the current runtime thread on drop.
        let state = unsafe { this.aliased.as_ref().get_extended() };

        // acquire the shared access lock
        {
            let mut lock = state.group.lock.lock().unwrap();
            if lock.tasks < 0 {
                todo!(
                    "support waiting for exclusive tasks to complete before starting shared tasks"
                )
            }
            let Some(tasks) = lock.tasks.checked_add(SHARED) else {
                // takes some very strange system to achieve this.
                panic!("number of active exceeded maximum")
            };
            lock.tasks = tasks;
        }

        // SAFETY:
        // state will stay valid for shared access until the returned future gets dropped.
        let (val, ptr_guard) = unsafe { state.state.borrow() };
        let future = f.call(val);

        let task = this.runtime.spawn(ScopedFuture {
            group: &state.group,
            future: ManuallyDrop::new(future),
            ptr_guard,
            _pinned: PhantomPinned,
        });
        this.handles.push(Rt::abort_handle(&task));
        task
    }
}

struct ScopedFuture<F, S> {
    group: &'static LockGroup,
    future: ManuallyDrop<F>,
    ptr_guard: GuardPtr<ManuallyDrop<S>>,
    _pinned: PhantomPinned,
}

impl<F, S> Drop for ScopedFuture<F, S> {
    fn drop(&mut self) {
        // SAFETY: this future is always init until we drop.
        // we must drop this early to release the aliased shared access on state.
        // we must also be careful to uphold the pin invariants.
        unsafe { ManuallyDrop::drop(&mut self.future) };
        self.ptr_guard.release();

        // false positive clippy warning
        #[allow(clippy::mut_mutex_lock)]
        let waker = {
            let mut lock = self.group.lock.lock().unwrap();

            lock.tasks -= SHARED;
            if lock.tasks & REMOVED == 0 {
                #[cfg(any(loom, feature = "std"))]
                if lock.tasks & PARKED == PARKED {
                    self.group.condvar.notify_one();
                }
                lock.take_waker()
            } else {
                None
            }
        };

        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<F: Future, S> Future for ScopedFuture<F, S> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: this future is always init until the ScopedFuture is dropped.
        // we also do not move any data.
        let fut = unsafe { self.map_unchecked_mut(|s| &mut *s.future) };
        fut.poll(cx)
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

#[cfg(all(test, not(loom), feature = "tokio"))]
mod tests {
    use crate::Runtime;
    use core::{future::Future, pin::pin, task::Context, time::Duration};
    use spin::Mutex;

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
                *state.lock() += 1;
                tokio::time::sleep(Duration::from_millis(10) * i).await;
            }
        }

        for i in 0..n {
            scoped.as_mut().spawn(Ex(i));
        }

        scoped.await.into_inner()
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
