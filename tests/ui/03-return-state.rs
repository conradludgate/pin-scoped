#![feature(async_closure)]

use std::pin::pin;

use pin_scoped::Scope;

#[tokio::main]
async fn main() {
    let scoped = pin!(Scope::new(0));

    let handle = scoped.as_ref().spawn(async move |state: &u64| state);

    let mut state = scoped.await;
    let state2 = handle.await.unwrap();

    state += 1;
    println!("{state} {state2}")
}
