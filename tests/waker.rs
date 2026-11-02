#![cfg(not(pin_scoped_loom))]

use std::{
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Wake, Waker},
};

use pin_scoped::{ScopeState, TokioScope};

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn completion_wakes_the_latest_registered_waker() {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let mut scope = Box::pin(TokioScope::new(()));
            let (release, wait_release) = tokio::sync::oneshot::channel();
            let task = scope
                .as_ref()
                .spawn(async move |state: ScopeState<'_, ()>| {
                    wait_release.await.unwrap();
                    std::hint::black_box(state);
                });

            let first = Arc::new(WakeCount::default());
            let first_waker = Waker::from(first.clone());
            let mut cx = Context::from_waker(&first_waker);
            assert!(scope.as_mut().poll(&mut cx).is_pending());
            assert!(scope.as_mut().poll(&mut cx).is_pending());
            // The scope retains one reference, even after repeated polls.
            assert_eq!(Arc::strong_count(&first), 3);

            let latest = Arc::new(WakeCount::default());
            let latest_waker = Waker::from(latest.clone());
            let mut cx = Context::from_waker(&latest_waker);
            assert!(scope.as_mut().poll(&mut cx).is_pending());
            assert_eq!(Arc::strong_count(&first), 2);
            assert_eq!(Arc::strong_count(&latest), 3);

            release.send(()).unwrap();
            task.await.unwrap();
            assert_eq!(first.0.load(Ordering::SeqCst), 0);
            assert_eq!(latest.0.load(Ordering::SeqCst), 1);
            assert_eq!(Arc::strong_count(&latest), 2);
            scope.await;
        });
}
