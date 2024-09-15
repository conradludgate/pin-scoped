#![feature(async_closure)]
#![cfg(not(pin_scoped_loom))]

use std::{future::poll_fn, pin::pin, sync::Mutex, task::Context};

use diatomic_waker::DiatomicWaker;
use futures_util::{task::noop_waker_ref, Future};
use tokio::{sync::Semaphore, task::yield_now};

use pin_scoped::Scope;

enum Task {
    ExtraSlow(u64),
}

struct State {
    count: Mutex<u64>,
    spawn_queue: Semaphore,
    next_task: Mutex<Option<Task>>,
    waker: DiatomicWaker,
}

impl State {
    async fn spawn(&self, t: Task) {
        self.spawn_queue.acquire().await.unwrap().forget();
        *self.next_task.lock().unwrap() = Some(t);
        self.waker.notify();
    }
}

async fn run(n: u64) -> u64 {
    let mut scoped = pin!(Scope::new(State {
        count: Mutex::new(0),
        spawn_queue: Semaphore::const_new(1),
        next_task: Mutex::new(None),
        waker: DiatomicWaker::new(),
    }));

    for i in 0..n {
        scoped.as_mut().spawn(async move |state: &State| {
            // we can spawn internally through some channel abstraction
            state.spawn(Task::ExtraSlow(i)).await;

            tokio::time::sleep(tokio::time::Duration::from_millis(10 * i)).await;
            *state.count.lock().unwrap() += 1;
            tokio::time::sleep(tokio::time::Duration::from_millis(10 * i)).await;
        });
    }

    loop {
        let state = scoped.as_ref().get();
        let get_task = || state.next_task.lock().unwrap().take();

        // SAFETY: only called by the scope owner task. Not concurrently
        let task = unsafe { state.waker.wait_until(get_task) };
        let empty = poll_fn(|cx| scoped.as_ref().poll_until_empty(cx));

        tokio::select! {
            _ = empty => break,
            task = task => {
                state.spawn_queue.add_permits(1);

                match task {
                    Task::ExtraSlow(i) => {
                        scoped.as_mut().spawn(async move |state: &State| {
                            tokio::time::sleep(tokio::time::Duration::from_millis(20 * i)).await;
                            *state.count.lock().unwrap() += 1;
                            tokio::time::sleep(tokio::time::Duration::from_millis(20 * i)).await;
                        });
                    }
                }
            }
        }
    }

    scoped.await.count.into_inner().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn scoped() {
    assert_eq!(run(32).await, 64);
}

#[tokio::test(flavor = "multi_thread")]
async fn dropped() {
    let mut task = pin!(run(32));
    assert!(task
        .as_mut()
        .poll(&mut Context::from_waker(noop_waker_ref()))
        .is_pending());
    yield_now().await;
}
