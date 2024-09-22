#![feature(async_closure)]

use std::{pin::pin, sync::OnceLock};

use pin_scoped::{Scope, ScopeState};

static STASH: OnceLock<ScopeState<'static, u64>> = OnceLock::new();

#[tokio::main]
async fn main() {
    let scoped = pin!(Scope::new(0));

    for _ in 0..64 {
        scoped.as_ref().spawn(async move |state: ScopeState<u64>| {
            STASH.set(state);
        });
    }

    scoped.await;
}
