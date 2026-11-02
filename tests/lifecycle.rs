#![cfg(not(pin_scoped_loom))]

use std::{
    future::{ready, Future},
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use pin_scoped::{async_fn::AsyncFnOnceRef, NestedStack, Scope, ScopeState};
use tokio::sync::oneshot;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

#[test]
fn nested_registration_is_held_until_its_state_is_released() {
    runtime().block_on(async {
        let scope = Box::pin(Scope::new(()));
        let (created, wait_created) = oneshot::channel();
        let (release, wait_release) = oneshot::channel();
        let (completed, wait_completed) = oneshot::channel();
        let (finish, wait_finish) = oneshot::channel();

        scope
            .as_ref()
            .spawn(async move |state: ScopeState<'_, ()>| {
                let nested = Box::pin(state.nest(String::from("nested state")));
                created.send(()).unwrap();
                wait_release.await.unwrap();
                assert_eq!(nested.await, "nested state");
                completed.send(()).unwrap();
                wait_finish.await.unwrap();
            });

        wait_created.await.unwrap();
        assert_eq!(scope.as_ref().num_tasks(), 2);
        release.send(()).unwrap();
        wait_completed.await.unwrap();
        assert_eq!(scope.as_ref().num_tasks(), 1);
        finish.send(()).unwrap();
        scope.await;
    });
}

#[test]
fn nested_registrations_are_counted_without_spawning_tasks() {
    runtime().block_on(async {
        let scope = Box::pin(Scope::new(()));
        let (created, wait_created) = oneshot::channel();
        let (release, wait_release) = oneshot::channel();

        scope
            .as_ref()
            .spawn(async move |state: ScopeState<'_, ()>| {
                let nested: Vec<_> = (0..64).map(|_| state.nest(())).collect();
                created.send(()).unwrap();
                wait_release.await.unwrap();
                drop(nested);
            });

        wait_created.await.unwrap();
        // One task plus 64 nested scopes, each retaining its parent registration.
        assert_eq!(scope.as_ref().num_tasks(), 65);
        release.send(()).unwrap();
        scope.await;
    });
}

#[test]
fn setup_panic_releases_its_registration() {
    struct PanicDuringSetup;

    impl AsyncFnOnceRef<(), ()> for PanicDuringSetup {
        fn call(self, _: ScopeState<'_, ()>) -> impl Send + Future<Output = ()> {
            if std::hint::black_box(true) {
                panic!("setup failed");
            }
            ready(())
        }
    }

    runtime().block_on(async {
        let scope = Box::pin(Scope::new(()));
        assert!(catch_unwind(AssertUnwindSafe(|| {
            scope.as_ref().spawn(PanicDuringSetup);
        }))
        .is_err());
        assert_eq!(scope.as_ref().num_tasks(), 0);
        scope.await;
    });
}

#[test]
fn nested_state_destructor_panic_releases_the_parent_registration() {
    struct PanicOnDrop;
    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("state destructor failed");
        }
    }

    runtime().block_on(async {
        let scope = Box::pin(Scope::new(()));
        let handle = scope.as_ref().spawn(async |state: ScopeState<'_, ()>| {
            let nested = Box::pin(state.nest(PanicOnDrop));
            assert!(catch_unwind(AssertUnwindSafe(|| drop(nested))).is_err());
        });
        handle.await.unwrap();
        assert_eq!(scope.as_ref().num_tasks(), 0);
        scope.await;
    });
}

#[test]
fn multiple_nested_levels_can_access_their_ancestors() {
    type First = NestedStack<AtomicUsize, ()>;
    type Second = NestedStack<First, ()>;

    runtime().block_on(async {
        let scope = Box::pin(Scope::new(AtomicUsize::new(0)));
        scope
            .as_ref()
            .spawn(async |state: ScopeState<'_, AtomicUsize>| {
                let first = Box::pin(state.nest(()));
                first.as_ref().spawn(async |state: ScopeState<'_, First>| {
                    let second = Box::pin(state.nest(()));
                    second
                        .as_ref()
                        .spawn(async |state: ScopeState<'_, Second>| {
                            tokio::task::yield_now().await;
                            state
                                .parent_scope()
                                .parent_scope()
                                .fetch_add(1, Ordering::SeqCst);
                        });
                    second.await;
                });
                first.await;
            });
        assert_eq!(scope.await.into_inner(), 1);
    });
}

#[test]
fn multithreaded_cancellation_releases_nested_users_before_their_owners() {
    struct RecordDrop {
        sequence: Arc<AtomicUsize>,
        position: Arc<AtomicUsize>,
    }

    impl Drop for RecordDrop {
        fn drop(&mut self) {
            let position = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
            self.position.store(position, Ordering::SeqCst);
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .unwrap();
    let sequence = Arc::new(AtomicUsize::new(0));
    let owner_position = Arc::new(AtomicUsize::new(0));
    let nested_position = Arc::new(AtomicUsize::new(0));
    let task_position = Arc::new(AtomicUsize::new(0));
    let owner = RecordDrop {
        sequence: sequence.clone(),
        position: owner_position.clone(),
    };
    let nested = RecordDrop {
        sequence: sequence.clone(),
        position: nested_position.clone(),
    };
    let task = RecordDrop {
        sequence,
        position: task_position.clone(),
    };
    let scope = Box::pin(Scope::with_runtime(owner, runtime.handle().clone()));
    let (started, wait_started) = oneshot::channel();
    scope
        .as_ref()
        .spawn(async move |state: ScopeState<'_, RecordDrop>| {
            let nested = Box::pin(state.nest(nested));
            nested.as_ref().spawn(
                async move |state: ScopeState<'_, NestedStack<RecordDrop, RecordDrop>>| {
                    let task = task;
                    started.send(()).unwrap();
                    std::future::pending::<()>().await;
                    std::hint::black_box((state, task));
                },
            );
            nested.await;
        });

    runtime.block_on(wait_started).unwrap();
    drop(scope);
    assert_eq!(task_position.load(Ordering::SeqCst), 1);
    assert_eq!(nested_position.load(Ordering::SeqCst), 2);
    assert_eq!(owner_position.load(Ordering::SeqCst), 3);
}
