#![cfg_attr(docsrs, feature(doc_auto_cfg))]
#![warn(clippy::significant_drop_tightening)]

use async_fn::AsyncFnOnceRef;
use pin_project_lite::pin_project;
use std::future::Future;
use std::marker::{PhantomData, PhantomPinned};
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};
use std::task::{ready, Context, Poll};
use std::{mem::ManuallyDrop, pin::Pin, task::Waker};

pub mod async_fn;
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
use sync::{Condvar, GuardPtr, ManuallyDropCell, Mutex};

#[macro_export]
macro_rules! Stack {
    [$($t:ty),* $(,)?] => {
        $crate::ScopeState<'_, $crate::StackInner![$($t),*]>
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! StackInner {
    [$t:ty $(, $rest:ty)+] => {
        $crate::NestedStack<$crate::StackInner![$($rest),*], $t>
    };
    [$t:ty] => {
        $t
    };
}

// The waker is set
const WAKER: usize = 0b0001;
// The scope owner has parked the thread
const PARKED: usize = 0b0010;
// The scope is closed and all tasks should stop
const CLOSED: usize = 0b0100;

// value of 1 task sharing the state
const SHARED: usize = 0b1000;
const SHARED_MASK: usize = !SHARED + 1;

struct SharedState<State> {
    group: LockGroup,
    /// initialised if and only if !scope.removed
    state: ManuallyDropCell<State>,

    /// runtime to spawn tasks into
    runtime: Handle,
}

unsafe impl<T: Sync> Sync for SharedState<T> {}

impl<State: 'static + Sync> SharedState<State> {
    fn spawn<F, R>(self: Pin<&Self>, f: F) -> JoinHandle<R>
    where
        F: AsyncFnOnceRef<State, R> + 'static,
        R: Send + 'static,
    {
        // acquire the shared access lock
        let task_key = {
            let mut lock = self.group.lock.lock().unwrap();

            debug_assert_eq!(lock.tasks >> 3, lock.cancel_list.len());

            if lock.tasks & CLOSED == CLOSED {
                drop(lock);
                // todo: return an error
                panic!("scope is in a closing state.");
            }

            let Some(tasks) = lock.tasks.checked_add(SHARED) else {
                drop(lock);
                // takes some very strange system to achieve this.
                panic!("number of active exceeded maximum")
            };

            lock.tasks = tasks;
            lock.cancel_list.insert(None)
        };

        // SAFETY:
        // state will stay valid for shared access until the returned future gets dropped.
        let (_, ptr_guard) = unsafe { self.state.borrow() };

        // SAFETY:
        // 1. `shared_state` cannot outlive the returned futures.
        // 2. futures cannot outlive `Scope` as scope will block the current runtime thread on drop.
        let shared_state = unsafe { &*(&*self as *const SharedState<State>) };

        let future = f.call(ScopeState(unsafe { Pin::new_unchecked(shared_state) }));

        let inner = ScopeGuard {
            // SAFETY: shared_state is already pinned
            group: unsafe { Pin::new_unchecked(&shared_state.group) },
            task_key,
        };

        let handle = self.runtime.spawn(ScopedFuture {
            inner,
            future: ManuallyDrop::new(future),
            ptr_guard,
        });

        // fill in abort handle
        {
            let mut lock = self.group.lock.lock().unwrap();
            debug_assert_eq!(lock.tasks >> 3, lock.cancel_list.len());
            let tasks = lock.tasks;

            if let Some(handle_slot) = lock.cancel_list.get_mut(task_key) {
                if tasks & CLOSED == CLOSED {
                    // if we are now closed, pre-emptively abort the new task.
                    handle.abort();
                } else {
                    *handle_slot = Some(handle.abort_handle());
                }
            }

            drop(lock);
        }

        handle
    }
}

pub struct ScopeState<'a, State>(Pin<&'a SharedState<State>>);

impl<'a, State> Clone for ScopeState<'a, State> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, State> Copy for ScopeState<'a, State> {}

impl<State: 'static + Sync> ScopeState<'_, State> {
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
    /// * Panics if called after **awaiting** or **dropping** the parent `Scope`
    pub fn spawn<F, R>(self, f: F) -> JoinHandle<R>
    where
        F: AsyncFnOnceRef<State, R> + 'static,
        R: Send + 'static,
    {
        self.0.spawn(f)
    }
}

impl<'parent, State: 'static + Sync> ScopeState<'parent, State> {
    pub fn nest<State2>(self, state: State2) -> NestedScope<'parent, State, State2> {
        // SAFETY:
        // 1. `shared_state` cannot outlive the returned futures.
        // 2. futures cannot outlive `Scope` as scope will block the current runtime thread on drop.
        // 3. We know it is pinned
        let shared_state =
            unsafe { Pin::new_unchecked(&*(self.0.get_ref() as *const SharedState<State>)) };

        let scope = Scope {
            aliased: SharedState {
                group: LockGroup {
                    lock: Mutex::new(LockState {
                        waker: MaybeUninit::uninit(),
                        tasks: 0,
                        cancel_list: SlotMap::new(),
                    }),
                    condvar: Condvar::new(),
                    _pinned: PhantomPinned,
                },
                state: ManuallyDropCell::new(NestedStack(shared_state, state)),
                runtime: self.0.runtime.clone(),
            },

            started_locking: false,
            removed: false,
        };
        NestedScope {
            scope,
            _covariant: PhantomData,
        }
    }
}

impl<State> Deref for ScopeState<'_, State> {
    type Target = State;

    fn deref(&self) -> &Self::Target {
        // SAFETY: while we have the scopestate, it is guaranteed that the state is init.
        unsafe { self.0.state.borrow().0 }
    }
}

struct LockGroup {
    lock: Mutex<LockState>,
    condvar: Condvar,

    /// mark as !Unpin since we share the state ref
    _pinned: PhantomPinned,
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
    pub struct Scope<State> {
        #[pin]
        aliased: SharedState<State>,

        started_locking: bool,
        removed: bool,
    }

    impl<State> PinnedDrop for Scope<State> {
        #[inline(never)]
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            let aliased = this.aliased.as_ref();
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
            let aliased = this.aliased.as_ref();
            let mut lock = aliased.group.lock.lock().unwrap();

            // if there are currently tasks running
            // return pending and register the waker.
            if lock.tasks & SHARED_MASK != 0 {
                lock.register_waker(cx.waker());
                return Poll::Pending;
            }

            lock.tasks |= CLOSED; // mark as closed.

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

pub struct NestedStack<Parent: 'static, State>(Pin<&'static SharedState<Parent>>, State);

impl<Parent: 'static, State> NestedStack<Parent, State> {
    pub fn parent_scope(&self) -> ScopeState<'_, Parent> {
        ScopeState(self.0)
    }

    pub fn into_inner(self) -> State {
        self.1
    }
}

impl<Parent: 'static, State> Deref for NestedStack<Parent, State> {
    type Target = State;

    fn deref(&self) -> &Self::Target {
        &self.1
    }
}

impl<Parent: 'static, State> DerefMut for NestedStack<Parent, State> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.1
    }
}

pin_project! {
    /// NestedScope represents a scope holding some values.
    ///
    /// [`tokio`] tasks can be spawned in the context of this scope.
    ///
    /// Should the scope be dropped before those tasks complete,
    /// the tasks will be [`aborted`](JoinHandle::abort) and the runtime
    /// thread will block until all tasks have dropped.
    pub struct NestedScope<'parent, Head: 'static, State: 'static> {
        #[pin]
        scope: Scope<NestedStack<Head, State>>,
        _covariant: PhantomData<&'parent Head>
    }
}

impl<'parent, Head, State> Future for NestedScope<'parent, Head, State> {
    type Output = State;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<State> {
        let state = ready!(self.project().scope.poll(cx));
        Poll::Ready(state.1)
    }
}

impl<'parent, Head: 'static + Sync, State: 'static + Sync> NestedScope<'parent, Head, State> {
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
    /// * Panics if called after **awaiting** `Scope`
    pub fn spawn<F, R>(self: Pin<&Self>, f: F) -> JoinHandle<R>
    where
        F: AsyncFnOnceRef<NestedStack<Head, State>, R> + 'static,
        R: Send + 'static,
    {
        self.project_ref().scope.spawn(f)
    }
}

impl<State: 'static + Sync> Scope<State> {
    /// # Panics
    ///
    /// * Panics if called from **outside** of the Tokio runtime.
    pub fn new(state: State) -> Self {
        Self::with_runtime(state, Handle::current())
    }
}

impl<State: 'static + Sync> Scope<State> {
    pub fn with_runtime(state: State, rt: Handle) -> Self {
        Self {
            aliased: SharedState {
                group: LockGroup {
                    lock: Mutex::new(LockState {
                        waker: MaybeUninit::uninit(),
                        tasks: 0,
                        cancel_list: SlotMap::new(),
                    }),
                    condvar: Condvar::new(),
                    _pinned: PhantomPinned,
                },
                state: ManuallyDropCell::new(state),
                runtime: rt,
            },
            started_locking: false,
            removed: false,
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
    /// * Panics if called after **awaiting** `Scope`
    pub fn spawn<F, R>(self: Pin<&Self>, f: F) -> JoinHandle<R>
    where
        F: AsyncFnOnceRef<State, R> + 'static,
        R: Send + 'static,
    {
        let this = self.project_ref();

        assert!(
            !*this.started_locking,
            "spawn should not be called after awaiting the Scope handle"
        );

        let state = this.aliased.as_ref();

        state.spawn(f)
    }
}

impl<State: 'static> Scope<State> {
    pub fn num_tasks(self: Pin<&Self>) -> usize {
        let group = &self.aliased.group.lock;
        group.lock().unwrap().cancel_list.len()
    }

    pub fn get(self: Pin<&Self>) -> &State {
        assert!(!self.removed);
        unsafe { self.aliased.state.borrow().0 }
    }

    pub fn poll_until_empty(self: Pin<&Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.project_ref();
        if *this.removed {
            return Poll::Ready(());
        };

        // acquire access to the shared state
        let aliased = this.aliased.as_ref();
        let mut lock = aliased.group.lock.lock().unwrap();

        // if there are currently tasks running
        // return pending and register the waker.
        if lock.tasks & SHARED_MASK != 0 {
            lock.register_waker(cx.waker());
            return Poll::Pending;
        }

        debug_assert!(lock.cancel_list.is_empty());

        drop(lock);

        Poll::Ready(())
    }
}

struct ScopeGuard {
    group: Pin<&'static LockGroup>,
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

#[cfg(all(test, not(pin_scoped_loom)))]
mod tests {
    use std::{future::Future, pin::pin, sync::Mutex, task::Context, time::Duration};

    use futures_util::task::noop_waker_ref;

    use crate::{Scope, ScopeState};

    struct Ex(u32);
    impl super::AsyncFnOnceRef<Mutex<u64>, ()> for Ex {
        async fn call(self, state: ScopeState<'_, Mutex<u64>>) {
            let i = self.0;
            tokio::time::sleep(Duration::from_millis(10) * i).await;
            *state.lock().unwrap() += 1;
            tokio::time::sleep(Duration::from_millis(10) * i).await;
        }
    }

    async fn run(n: u32) -> u64 {
        let scoped = pin!(Scope::new(Mutex::new(0)));

        for i in 0..n {
            scoped.as_ref().spawn(Ex(i));
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
