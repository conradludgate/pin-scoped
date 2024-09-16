#![feature(async_closure)]
#![cfg(not(pin_scoped_loom))]

use std::{pin::pin, sync::Mutex, task::Context};

use futures_util::{task::noop_waker_ref, Future};

use pin_scoped::Scope;

async fn run(n: u64) -> u64 {
    let mut scoped = pin!(Scope::new(Mutex::new(0)));

    for i in 0..n {
        scoped.as_mut().spawn(async move |state: &Mutex<u64>| {
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
    let res = rt().block_on(run(64));
    assert_eq!(res, 64);
}

#[test]
fn dropped() {
    let rt = rt();
    let _guard = rt.enter();

    let mut task = pin!(run(64));
    assert!(task
        .as_mut()
        .poll(&mut Context::from_waker(noop_waker_ref()))
        .is_pending());
}
