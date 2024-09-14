#![feature(async_closure)]

use std::{pin::pin, sync::Mutex};

use pin_scoped::Scope;

#[tokio::main]
async fn main() {
    let mut scoped = pin!(Scope::new(0));

    let handle = scoped.as_mut().spawn(async move |state: &u64| state);

    let mut state = scoped.await;
    let state2 = handle.await.unwrap();

    state += 1;
    println!("{state2}")
}
