#![cfg(pin_scoped_loom)]

use futures_util::task::noop_waker_ref;
use loom::sync::Mutex;
use pin_scoped::loom_rt::Handle;
use pin_scoped::Scope;
use std::{
    future::{poll_fn, Future},
    pin::pin,
    task::Context,
};

#[test]
fn scoped() {
    loom::model(|| {
        let rt = Handle::current();

        let worker_thread = rt.spawn_worker();

        let val = loom::future::block_on(run(2, rt.clone()));
        assert_eq!(val, 2);

        rt.shutdown();
        worker_thread.join().unwrap();
    });
}

#[test]
fn dropped() {
    loom::model(|| {
        let rt = Handle::current();

        let worker_thread = rt.spawn_worker();

        {
            let mut task = pin!(run(2, rt.clone()));
            let _ = task
                .as_mut()
                .poll(&mut Context::from_waker(noop_waker_ref()));
        }

        rt.shutdown();
        worker_thread.join().unwrap();
    });
}

async fn run(n: u32, rt: Handle) -> u64 {
    let scoped = pin!(Scope::with_runtime(Mutex::new(0), rt));
    struct Ex;
    impl pin_scoped::async_fn::AsyncFnOnceRef<Mutex<u64>, ()> for Ex {
        async fn call(self, state: &Mutex<u64>) {
            yield_now().await;
            *state.lock().unwrap() += 1;
            yield_now().await;
        }
    }

    for _ in 0..n {
        scoped.as_ref().spawn(Ex);
    }

    scoped.await.into_inner().unwrap()
}

async fn yield_now() {
    poll_fn(|cx| {
        cx.waker().wake_by_ref();
        std::task::Poll::Ready(())
    })
    .await
}
