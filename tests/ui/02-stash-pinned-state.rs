#![feature(async_closure)]

use std::{pin::pin, sync::Mutex};

use pin_scoped::TokioScope;

static STASH: Mutex<&'static u64> = Mutex::new(&0);

#[tokio::main]
async fn main()  {
    let mut scoped = pin!(TokioScope::new(0));

    for _ in 0..64 {
        scoped.as_mut().spawn(async move |state: &u64| {
            *STASH.lock().unwrap() = state;
        });
    }

    scoped.await;
}
