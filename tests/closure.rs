#![feature(async_closure)]

use std::{pin::pin, sync::Mutex, task::Context};

use futures_util::{task::noop_waker_ref, Future};
use tokio::task::yield_now;

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

#[tokio::test(flavor = "multi_thread")]
async fn scoped() {
    assert_eq!(run(64).await, 64);
}

#[tokio::test(flavor = "multi_thread")]
async fn dropped() {
    let mut task = pin!(run(64));
    assert!(task
        .as_mut()
        .poll(&mut Context::from_waker(noop_waker_ref()))
        .is_pending());
    yield_now().await;
}
