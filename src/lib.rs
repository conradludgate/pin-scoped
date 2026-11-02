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
        // SAFETY: the pinned owner cannot reclaim its storage while a registration
        // exists. Register before exposing the extended borrow to caller code;
        // the registration stays with the future through its destruction.
        let shared_state = unsafe { &*(&*self as *const SharedState<State>) };
        // SAFETY: group is structurally pinned within shared_state.
        let inner = ScopeGuard::new(unsafe { Pin::new_unchecked(&shared_state.group) });
        let task_key = inner.task_key;
        // SAFETY: inner keeps the initialized state borrowed until the future is
        // destroyed, including when constructing or spawning it unwinds.
        let (_, ptr_guard) = unsafe { self.state.borrow() };
        // SAFETY: shared_state is already pinned and inner protects its lifetime.
        let future = f.call(ScopeState(unsafe { Pin::new_unchecked(shared_state) }));

        let handle = self.runtime.spawn(RuntimeFuture::new(ScopedFuture {
            inner,
            future: ManuallyDrop::new(future),
            ptr_guard,
        }));

        // Closing transfers the map to the owner. A spawn that finishes
        // installing after that transfer must request its own cancellation.
        let cancel = {
            let mut lock = self.group.lock.lock().unwrap();
            let tasks = lock.tasks;
            if let Some(cancel_list) = lock.cancel_list.as_mut() {
                debug_assert_eq!(tasks >> 3, cancel_list.len());
                if let Some(handle_slot) = cancel_list.get_mut(task_key) {
                    *handle_slot = Some(handle.abort_handle());
                }
                false
            } else {
                true
            }
        };
        if cancel {
            handle.abort();
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
    /// Creates a nested scope registered with this scope.
    ///
    /// # Panics
    ///
    /// Panics if the parent scope has begun closing.
    pub fn nest<State2>(self, state: State2) -> NestedScope<'parent, State, State2> {
        // SAFETY: the parent is pinned. Its owner cannot reclaim its storage
        // while a registration exists; acquire one before returning a nested
        // scope, and retain it in the nested state until that state is released.
        let shared_state =
            unsafe { Pin::new_unchecked(&*(self.0.get_ref() as *const SharedState<State>)) };
        // SAFETY: group is structurally pinned within the parent. Keep its
        // registration in the nested state, after that state's user payload, so
        // the parent remains borrowed until all nested users have been released.
        let parent = ScopeGuard::new(unsafe { Pin::new_unchecked(&shared_state.get_ref().group) });

        let scope = Scope {
            aliased: SharedState {
                group: LockGroup {
                    lock: Mutex::new(LockState {
                        waker: MaybeUninit::uninit(),
                        tasks: 0,
                        cancel_list: Some(SlotMap::new()),
                    }),
                    condvar: Condvar::new(),
                    _pinned: PhantomPinned,
                },
                state: ManuallyDropCell::new(NestedStack(shared_state, state, parent)),
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
    /// Registration count in the high bits, WAKER/PARKED/CLOSED in the low bits.
    tasks: usize,

    /// Set when [`WAKER`] bit of `tasks` is set.
    waker: MaybeUninit<Waker>,

    /// Active registrations, until cancellation takes ownership of the map.
    /// Once taken, tasks still counts registrations until their guards drop.
    /// Nested scopes have no abort handle of their own.
    cancel_list: Option<SlotMap<DefaultKey, Option<AbortHandle>>>,
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
            let waker = waker.clone();
            self.waker.write(waker);
            // Publish initialization only after cloning succeeds.
            self.tasks |= WAKER;
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
    ///
    /// Early cancellation uses [`tokio::task::block_in_place`] and requires a
    /// context supported by that function. If cancellation cleanup unwinds
    /// before all registrations are released, the process aborts. Panics from
    /// the owned state's destructor can unwind after cleanup has completed.
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
            // Scope storage must remain live until all registered users have
            // finished. Do not allow an incomplete cleanup to unwind past it.
            let abort_on_unwind = AbortOnDrop;

            // lock and drop the state.
            let mut lock = aliased.group.lock.lock().unwrap();

            lock.tasks |= CLOSED; // mark as closed.

            if lock.tasks & SHARED_MASK != 0 {
                // Move the existing allocation out before cancellation. Guards
                // continue releasing registrations through tasks, and late spawns
                // cancel themselves when they find that the map has been taken.
                let cancel_list = lock.cancel_list.take().unwrap();
                drop(lock);
                for (_, handle) in cancel_list {
                    if let Some(handle) = handle {
                        handle.abort();
                    }
                }
                lock = aliased.group.lock.lock().unwrap();
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

            debug_assert!(lock.cancel_list.as_ref().map_or(true, SlotMap::is_empty));
            *this.removed = true;
            std::mem::forget(abort_on_unwind);

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

            debug_assert!(lock.cancel_list.as_ref().map_or(true, SlotMap::is_empty));
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

pub struct NestedStack<Parent: 'static, State>(
    Pin<&'static SharedState<Parent>>,
    State,
    // Drop after State, whose destructor may still use the enclosing scope.
    #[allow(dead_code)] ScopeGuard,
);

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
    ///
    /// Cancellation has the same blocking-context and abort-on-unwind behavior
    /// as [`Scope`]. A leaked nested scope keeps its parent registered; awaiting
    /// or dropping that parent may then wait indefinitely.
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
                        cancel_list: Some(SlotMap::new()),
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
    /// Returns the number of active task and nested-scope registrations.
    ///
    /// A nested scope keeps one registration until its state is released, even
    /// when it has no tasks of its own.
    pub fn num_tasks(self: Pin<&Self>) -> usize {
        let group = &self.aliased.group.lock;
        group.lock().unwrap().tasks >> 3
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

        debug_assert!(lock.cancel_list.as_ref().map_or(true, SlotMap::is_empty));

        drop(lock);

        Poll::Ready(())
    }
}

// The owner waits for every registration before releasing shared state. This
// guard must outlive the future or nested state using that borrow.
struct ScopeGuard {
    group: Pin<&'static LockGroup>,
    task_key: DefaultKey,
}

struct AbortOnDrop;

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        std::process::abort();
    }
}

impl ScopeGuard {
    fn new(group: Pin<&'static LockGroup>) -> Self {
        let task_key = {
            let mut lock = group.lock.lock().unwrap();
            if lock.tasks & CLOSED != 0 {
                drop(lock);
                panic!("scope is in a closing state.");
            }
            let Some(tasks) = lock.tasks.checked_add(SHARED) else {
                drop(lock);
                panic!("number of active registrations exceeded maximum");
            };
            let cancel_list = lock.cancel_list.as_mut().unwrap();
            debug_assert_eq!((tasks >> 3) - 1, cancel_list.len());
            let key = cancel_list.insert(None);
            lock.tasks = tasks;
            key
        };
        Self { group, task_key }
    }
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        let (waker, token) = {
            let mut lock = self.group.lock.lock().unwrap();
            let token = lock
                .cancel_list
                .as_mut()
                .and_then(|cancel_list| cancel_list.remove(self.task_key));

            lock.tasks -= SHARED;
            let waker = if lock.tasks & SHARED_MASK == 0 {
                if lock.tasks & PARKED == PARKED {
                    self.group.condvar.notify_one();
                }
                lock.take_waker()
            } else {
                None
            };
            (waker, token)
        };

        if let Some(waker) = waker {
            waker.wake();
        }
        drop(token);
    }
}

// Keep reference-bearing task state behind an initialization boundary when
// ownership crosses executor code. The executor only pins/polls/drops this
// wrapper; access to the initialized inner future stays local to these methods.
pin_project! {
    struct RuntimeFuture<F> {
        #[pin]
        inner: MaybeUninit<F>,
    }

    impl<F> PinnedDrop for RuntimeFuture<F> {
        fn drop(this: Pin<&mut Self>) {
            // SAFETY: new initializes inner once. Polling only borrows it, and
            // this private wrapper never extracts or replaces it. Destruction
            // occurs in place; MaybeUninit does not subsequently drop F again.
            unsafe { this.project().inner.get_unchecked_mut().assume_init_drop() };
        }
    }
}

impl<F> RuntimeFuture<F> {
    fn new(future: F) -> Self {
        Self {
            inner: MaybeUninit::new(future),
        }
    }
}

impl<F: Future> Future for RuntimeFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: inner is initialized by new and stays initialized until
        // PinnedDrop. It is structurally pinned, never moved after pinning, and
        // destroyed in place, so projecting to the initialized F preserves Pin.
        unsafe {
            self.project()
                .inner
                .map_unchecked_mut(|inner| inner.assume_init_mut())
        }
        .poll(cx)
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
