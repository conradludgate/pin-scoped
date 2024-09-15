#![cfg_attr(docsrs, feature(doc_auto_cfg))]
#![warn(clippy::significant_drop_tightening)]

use pin_project_lite::pin_project;
use std::future::Future;
use std::mem::MaybeUninit;
use std::task::{Context, Poll};
use std::{mem::ManuallyDrop, pin::Pin, task::Waker};

mod sync;

#[cfg(pin_scoped_loom)]
pub mod loom_rt;
#[cfg(pin_scoped_loom)]
use loom_rt::{block_in_place, AbortHandle, Handle, JoinHandle};

#[cfg(not(pin_scoped_loom))]
use tokio::{
    runtime::Handle,
    task::{block_in_place, AbortHandle, JoinHandle},
};

use slotmap::{DefaultKey, SlotMap};
use sync::{Aliasable, Condvar, GuardPtr, ManuallyDropCell, Mutex};

// The waker is set
const WAKER: usize = 0b0001;
// The scope owner has parked the thread
const PARKED: usize = 0b0010;
// The scope is closed and all tasks should stop
const CLOSED: usize = 0b0100;

// value of 1 task sharing the state
const SHARED: usize = 0b1000;
const SHARED_MASK: usize = !SHARED + 1;

// /// The state is exclusively acquired from the scope
// const EXCLUSIVE: usize = -1;

struct SharedState<State> {
    group: LockGroup,
    /// initialised if and only if !scope.removed
    state: ManuallyDropCell<State>,
}

struct LockGroup {
    lock: Mutex<LockState>,
    condvar: Condvar,
}

struct LockState {
    /// positive for read-only tasks
    /// [`isize::MIN`] for a single mut task
    /// -1 for when the state is removed
    tasks: usize,

    /// Set when [`WAKER`] bit of `tasks` is set.
    waker: MaybeUninit<Waker>,

    /// The list of currently active tasks
    cancel_list: SlotMap<DefaultKey, Option<AbortHandle>>,
}

impl LockState {
    fn drop_waker(&mut self) {
        // drop the waker if it's set.
        if self.tasks & WAKER == WAKER {
            // unset the bit
            self.tasks &= !WAKER;

            // SAFETY: the waker bit was set, so this is init
            unsafe {
                MaybeUninit::assume_init_drop(&mut self.waker);
            }
        }
    }

    fn take_waker(&mut self) -> Option<Waker> {
        // take the waker if it's set.
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

pin_project! {
    /// Scope represents a scope holding some values.
    ///
    /// [`tokio`] tasks can be spawned in the context of this scope.
    ///
    /// Should the scope be dropped before those tasks complete,
    /// the tasks will be [`aborted`](JoinHandle::abort) and the runtime
    /// thread will block until all tasks have dropped.
    pub struct Scope<State: 'static> {
        #[pin]
        aliased: Aliasable<SharedState<State>>,

        started_locking: bool,
        removed: bool,

        runtime: Handle,
    }

    impl<State: 'static> PinnedDrop for Scope<State> {
        #[inline(never)]
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            let aliased = this.aliased.as_ref().get();
            *this.started_locking = true;

            if *this.removed {
                return;
            }

            // lock and drop the state.
            let mut lock = aliased.group.lock.lock().unwrap();

            lock.tasks |= CLOSED; // mark as closed.

            // abort the tasks that are still in flight
            for (_, handle) in &mut lock.cancel_list {
                if let Some(handle) = handle.take() {
                    handle.abort();
                }
            }

            // wait until there are no tasks left holding the shared lock
            if lock.tasks & SHARED_MASK != 0 {
                lock = block_in_place(move || {
                    loop {
                        // set parked bit
                        lock.tasks |= PARKED;

                        // park
                        lock = aliased.group.condvar.wait(lock).unwrap();

                        // unset parked bit
                        lock.tasks &= !PARKED;

                        // exit if no more tasks are holding the shared lock
                        if lock.tasks & SHARED_MASK == 0 {
                            break lock;
                        }
                    }
                });
            }

            lock.drop_waker();

            debug_assert!(lock.cancel_list.is_empty());
            *this.removed = true;

            // SAFETY:
            // 0 tasks means we have exclusive access to state now.
            // and it is currently initialised.
            unsafe { aliased.state.drop() };

            drop(lock);
        }
    }
}

impl<State> Future for Scope<State> {
    type Output = State;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<State> {
        let this = self.project();
        assert!(!*this.removed, "polled after completion");

        *this.started_locking = true;

        let state = {
            // acquire access to the shared state
            let aliased = this.aliased.as_ref().get();
            let mut lock = aliased.group.lock.lock().unwrap();

            // if there are currently tasks running
            // return pending and register the waker.
            if lock.tasks & SHARED_MASK != 0 {
                lock.register_waker(cx.waker());
                return Poll::Pending;
            }

            lock.drop_waker();

            debug_assert!(lock.cancel_list.is_empty());
            *this.removed = true;

            // SAFETY:
            // 0 tasks means we have exclusive access to state now.
            // and it is currently initialised.
            let state = unsafe { aliased.state.take() };

            drop(lock);

            state
        };

        Poll::Ready(state)
    }
}

impl<State: 'static + Sync> Scope<State> {
    pub fn new(state: State) -> Self {
        Self::with_runtime(state, Handle::current())
    }
}

impl<State: 'static + Sync> Scope<State> {
    pub fn with_runtime(state: State, rt: Handle) -> Self {
        Self {
            aliased: Aliasable::new(SharedState {
                group: LockGroup {
                    lock: Mutex::new(LockState {
                        waker: MaybeUninit::uninit(),
                        tasks: 0,
                        cancel_list: SlotMap::new(),
                    }),
                    condvar: Condvar::new(),
                },
                state: ManuallyDropCell::new(state),
            }),
            started_locking: false,
            removed: false,
            runtime: rt,
        }
    }

    /// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
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

        assert!(
            !*this.started_locking,
            "spawn should not be called after awaiting the Scope handle"
        );

        // SAFETY:
        // 1. `state` cannot outlive the returned futures.
        // 2. futures cannot outlive `Scope` as scope blocked the current runtime thread on drop.
        let state = unsafe { this.aliased.as_ref().get_extended() };

        // acquire the shared access lock
        let task_key = {
            let mut lock = state.group.lock.lock().unwrap();
            let Some(tasks) = lock.tasks.checked_add(SHARED) else {
                // takes some very strange system to achieve this.
                panic!("number of active exceeded maximum")
            };
            lock.tasks = tasks;
            lock.cancel_list.insert(None)
        };

        // SAFETY:
        // state will stay valid for shared access until the returned future gets dropped.
        let (val, ptr_guard) = unsafe { state.state.borrow() };
        let future = f.call(val);

        let inner = ScopeGuard {
            group: &state.group,
            task_key,
        };

        let handle = this.runtime.spawn(ScopedFuture {
            inner,
            future: ManuallyDrop::new(future),
            ptr_guard,
        });

        // fill in abort handle
        {
            let mut lock = state.group.lock.lock().unwrap();
            if let Some(handle_slot) = lock.cancel_list.get_mut(task_key) {
                *handle_slot = Some(handle.abort_handle());
            }
            drop(lock);
        }

        handle
    }
}

impl<State: 'static> Scope<State> {
    pub fn num_tasks(self: Pin<&Self>) -> usize {
        let group = &self.project_ref().aliased.get().group.lock;
        group.lock().unwrap().cancel_list.len()
    }

    pub fn get(self: Pin<&Self>) -> &State {
        assert!(!self.removed);
        unsafe { self.project_ref().aliased.get().state.borrow().0 }
    }

    pub fn poll_until_empty(self: Pin<&Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.project_ref();
        if *this.removed {
            return Poll::Ready(());
        };

        // acquire access to the shared state
        let aliased = this.aliased.as_ref().get();
        let mut lock = aliased.group.lock.lock().unwrap();

        // if there are currently tasks running
        // return pending and register the waker.
        if lock.tasks & SHARED_MASK != 0 {
            lock.register_waker(cx.waker());
            return Poll::Pending;
        }

        debug_assert!(lock.cancel_list.is_empty());

        Poll::Ready(())
    }
}

struct ScopeGuard {
    group: &'static LockGroup,
    task_key: DefaultKey,
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        let waker = {
            let mut lock = self.group.lock.lock().unwrap();
            lock.cancel_list.remove(self.task_key);

            lock.tasks -= SHARED;
            if lock.tasks & SHARED_MASK == 0 {
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

pin_project! {
    struct ScopedFuture<F, S> {
        inner: ScopeGuard,
        #[pin]
        future: ManuallyDrop<F>,
        ptr_guard: GuardPtr<ManuallyDrop<S>>,
    }

    impl<F, S> PinnedDrop for ScopedFuture<F, S> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();

            // SAFETY: it is safe to drop a pinned value. in fact, it is required.
            // we drop it early to ensure that state is not aliased while we release the locks
            unsafe { ManuallyDrop::drop(this.future.get_unchecked_mut()) }
            this.ptr_guard.release();
        }
    }
}

impl<F: Future, S> Future for ScopedFuture<F, S> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        // SAFETY: future is always init and we do not move anything
        let future = unsafe { this.future.map_unchecked_mut(|f| &mut **f) };
        future.poll(cx)
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

#[cfg(all(test, not(pin_scoped_loom)))]
mod tests {
    use std::{future::Future, pin::pin, sync::Mutex, task::Context, time::Duration};

    use futures_util::task::noop_waker_ref;

    use crate::Scope;

    struct Ex(u32);
    impl super::AsyncFnOnceRef<Mutex<u64>, ()> for Ex {
        async fn call(self, state: &Mutex<u64>) {
            let i = self.0;
            tokio::time::sleep(Duration::from_millis(10) * i).await;
            *state.lock().unwrap() += 1;
            tokio::time::sleep(Duration::from_millis(10) * i).await;
        }
    }

    async fn run(n: u32) -> u64 {
        let mut scoped = pin!(Scope::new(Mutex::new(0)));

        for i in 0..n {
            scoped.as_mut().spawn(Ex(i));
        }

        scoped.await.into_inner().unwrap()
    }

    #[test]
    fn scoped() {
        let res = tokio::runtime::Builder::new_multi_thread()
            .enable_time()
            .worker_threads(1)
            .build()
            .unwrap()
            .block_on(run(64));

        assert_eq!(res, 64);
    }

    #[test]
    fn dropped() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_time()
            .worker_threads(1)
            .build()
            .unwrap();
        let _guard = rt.enter();

        let mut task = pin!(run(64));
        assert!(task
            .as_mut()
            .poll(&mut Context::from_waker(noop_waker_ref()))
            .is_pending());
    }
}
