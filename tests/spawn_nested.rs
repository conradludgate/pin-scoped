#![feature(async_closure)]
#![cfg(not(pin_scoped_loom))]

use std::{
    pin::{pin, Pin},
    sync::Mutex,
    task::Context,
};

use futures_util::{task::noop_waker_ref, Future};

use pin_scoped::{spawner::StateWithSpawner, Scope, ScopeState};

enum Task {
    ExtraSlow(u64),
}

type State = ScopeState<StateWithSpawner<Mutex<u64>, Task>>;

async fn run(n: u64) -> u64 {
    let mut scoped = pin!(Scope::new(StateWithSpawner::new(Mutex::new(0))));

    for i in 0..n {
        scoped.as_ref().spawn(async move |state: Pin<&State>| {
            // we can spawn internally through some channel abstraction
            state.spawn(Task::ExtraSlow(i)).await;

            tokio::time::sleep(tokio::time::Duration::from_millis(10 * i)).await;
            *state.state.lock().unwrap() += 1;
            tokio::time::sleep(tokio::time::Duration::from_millis(10 * i)).await;
        });
    }

    while let Some(task) = scoped.as_mut().pop_task().await {
        match task {
            Task::ExtraSlow(i) => {
                scoped.as_ref().spawn(async move |state: Pin<&State>| {
                    tokio::time::sleep(tokio::time::Duration::from_millis(20 * i)).await;
                    *state.state.lock().unwrap() += 1;
                    tokio::time::sleep(tokio::time::Duration::from_millis(20 * i)).await;
                });
            }
        }
    }

    scoped.await.state.into_inner().unwrap()
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
