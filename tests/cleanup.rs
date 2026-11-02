#![cfg(all(feature = "tokio", not(pin_scoped_loom), not(miri)))]

use std::process::Command;

use pin_scoped::{Scope, ScopeState};
use tokio::runtime::Handle;

#[test]
fn incomplete_cleanup_aborts() {
    const CHILD: &str = "PIN_SCOPED_CLEANUP_CHILD";
    if std::env::var_os(CHILD).is_some() {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async {
                let scope = Box::pin(Scope::new(()));
                scope.as_ref().spawn(async |_: ScopeState<'_, (), Handle>| {
                    std::future::pending::<()>().await;
                });
                drop(scope);
            });
        return;
    }

    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "incomplete_cleanup_aborts", "--nocapture"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(output.status.signal(), Some(6), "{output:?}");
    }
    #[cfg(not(unix))]
    assert!(!output.status.success(), "{output:?}");
}

#[test]
fn retained_nested_registration_keeps_owner_pending() {
    const CHILD: &str = "PIN_SCOPED_REGISTRATION_CHILD";
    if std::env::var_os(CHILD).is_some() {
        use std::{future::Future, task::Context};

        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async {
                let mut scope = Box::pin(Scope::new(()));
                scope
                    .as_ref()
                    .spawn(async |state: ScopeState<'_, (), Handle>| {
                        std::mem::forget(state.nest(()));
                    })
                    .await
                    .unwrap();
                assert_eq!(scope.as_ref().num_tasks(), 1);
                let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
                assert!(scope.as_mut().poll(&mut cx).is_pending());
                // Process isolation avoids blocking suite teardown on an
                // intentionally retained registration.
                std::process::exit(0);
            });
        return;
    }

    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "retained_nested_registration_keeps_owner_pending",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}
