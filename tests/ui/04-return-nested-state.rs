#![feature(async_closure)]

use std::pin::pin;

use pin_scoped::{Scope, ScopeState};

#[tokio::main]
async fn main() {
    let scoped = pin!(Scope::new(0));

    let handle = scoped
        .as_ref()
        .spawn(async move |state: ScopeState<u64>| state.nest(1));

    let mut state = scoped.await;
    let nested_scope = handle.await.unwrap();

    state += 1;
    println!("{state}");

    drop(nested_scope);
}
