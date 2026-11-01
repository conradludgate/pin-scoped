#![cfg(not(pin_scoped_loom))]

use std::{pin::pin, sync::Mutex, task::Context};

use futures_util::{task::noop_waker_ref, Future};

use pin_scoped::{Scope, ScopeState};

type State<'a> = ScopeState<'a, Mutex<u64>>;

async fn run(n: u64) -> u64 {
    let scoped = pin!(Scope::new(Mutex::new(0)));

    for i in 0..n {
        scoped.as_ref().spawn(async move |state: State| {
            // we can spawn siblings internally
            state.spawn(async move |state: State| {
                tokio::time::sleep(tokio::time::Duration::from_millis(20 * i)).await;
                *state.lock().unwrap() += 1;
                tokio::time::sleep(tokio::time::Duration::from_millis(20 * i)).await;
            });

            tokio::time::sleep(tokio::time::Duration::from_millis(10 * i)).await;
            *state.lock().unwrap() += 1;
            tokio::time::sleep(tokio::time::Duration::from_millis(10 * i)).await;
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
