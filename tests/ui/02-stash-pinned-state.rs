#![feature(async_closure)]

use std::{pin::pin, sync::Mutex};

use pin_scoped::Scope;

static STASH: Mutex<&'static u64> = Mutex::new(&0);

#[tokio::main]
async fn main()  {
    let scoped = pin!(Scope::new(0));

    for _ in 0..64 {
        scoped.as_ref().spawn(async move |state: &u64| {
            *STASH.lock().unwrap() = state;
        });
    }

    scoped.await;
}
