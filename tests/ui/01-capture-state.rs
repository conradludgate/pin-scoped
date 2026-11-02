
use std::{pin::pin, sync::Mutex};
use tokio::{runtime::Handle, task::yield_now};

use pin_scoped::{Scope, ScopeState};

#[tokio::main]
async fn main() {
    let scoped = pin!(Scope::new(Mutex::new(0)));

    let non_scoped = Mutex::new(0);

    for _ in 0..64 {
        scoped
            .as_ref()
            .spawn(async |state: ScopeState<Mutex<u64>, Handle>| {
                yield_now().await;
                *state.lock().unwrap() += 1;
                *non_scoped.lock().unwrap() += 1;
                yield_now().await;
            });
    }

    scoped.await;
}
