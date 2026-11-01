#![cfg(not(pin_scoped_loom))]

use std::{
    pin::pin,
    sync::{atomic::AtomicU64, Mutex},
    task::Context,
};

use futures_util::{task::noop_waker_ref, Future};

use pin_scoped::{Scope, Stack};
use tokio::time::{sleep, Duration};

async fn run(n: u64) -> u64 {
    let scoped = pin!(Scope::new(Mutex::new(0)));

    for i in 0..n {
        scoped
            .as_ref()
            .spawn(async move |state: Stack![Mutex<u64>]| {
                let nested = pin!(state.nest(AtomicU64::new(0)));

                // we can spawn new scopes internally
                nested
                    .as_ref()
                    .spawn(async move |state: Stack![AtomicU64, Mutex<u64>]| {
                        *state.parent_scope().lock().unwrap() += 1;

                        sleep(Duration::from_millis(20 * i)).await;
                        state.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    });

                sleep(Duration::from_millis(10 * i)).await;

                *state.lock().unwrap() += nested.await.into_inner();
            });
    }

    scoped.await.into_inner().unwrap()
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .worker_threads(1)
        .build()
        .unwrap()
}

#[test]
fn scoped() {
    let res = rt().block_on(run(32));
    assert_eq!(res, 64);
}

#[test]
fn dropped() {
    let rt = rt();
    let _guard = rt.enter();

    let mut task = pin!(run(32));
    assert!(task
        .as_mut()
        .poll(&mut Context::from_waker(noop_waker_ref()))
        .is_pending());
}
