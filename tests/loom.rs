#![cfg(pin_scoped_loom)]

use futures_util::task::noop_waker_ref;
use loom::sync::Mutex;
use pin_scoped::loom_rt::Handle;
use pin_scoped::{NestedStack, Scope, ScopeState};
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
    impl pin_scoped::async_fn::AsyncFnOnceRef<Mutex<u64>, (), Handle> for Ex {
        async fn call(self, state: ScopeState<'_, Mutex<u64>, Handle>) {
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

#[test]
fn nested_scopes() {
    type InnerState = NestedStack<Mutex<u64>, (), Handle>;

    struct Outer;
    impl pin_scoped::async_fn::AsyncFnOnceRef<Mutex<u64>, (), Handle> for Outer {
        async fn call(self, state: ScopeState<'_, Mutex<u64>, Handle>) {
            let nested = Box::pin(state.nest(()));
            nested.as_ref().spawn(Inner);
            nested.await;
        }
    }

    struct Inner;
    impl pin_scoped::async_fn::AsyncFnOnceRef<InnerState, (), Handle> for Inner {
        async fn call(self, state: ScopeState<'_, InnerState, Handle>) {
            *state.parent_scope().lock().unwrap() += 1;
        }
    }

    loom::model(|| {
        let rt = Handle::current();
        let worker = rt.spawn_worker();
        let scope = Box::pin(Scope::with_runtime(Mutex::new(0), rt.clone()));
        scope.as_ref().spawn(Outer);
        assert_eq!(loom::future::block_on(scope).into_inner().unwrap(), 1);
        rt.shutdown();
        worker.join().unwrap();
    });
}
