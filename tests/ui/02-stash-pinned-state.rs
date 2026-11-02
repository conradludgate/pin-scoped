
use std::{pin::pin, sync::OnceLock};

use pin_scoped::{Scope, ScopeState};
use tokio::runtime::Handle;

static STASH: OnceLock<ScopeState<'static, u64, Handle>> = OnceLock::new();

#[tokio::main]
async fn main() {
    let scoped = pin!(Scope::new(0));

    for _ in 0..64 {
        scoped.as_ref().spawn(async move |state: ScopeState<u64, Handle>| {
            STASH.set(state);
        });
    }

    scoped.await;
}
