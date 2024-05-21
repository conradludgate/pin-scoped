#![feature(async_closure)]

use std::{pin::pin, sync::Mutex};
use tokio::task::yield_now;

use pin_scoped::Scoped;

#[tokio::main]
async fn main()  {
    let mut scoped = pin!(Scoped::new(Mutex::new(0)));

    let non_scoped = Mutex::new(0);

    for _ in 0..64 {
        scoped.as_mut().spawn(async |state: &Mutex<u64>| {
            yield_now().await;
            *state.lock().unwrap() += 1;
            *non_scoped.lock().unwrap() += 1;
            yield_now().await;
        });
    }

    scoped.await;
}
