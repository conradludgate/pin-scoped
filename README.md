# pin-scoped

Abusing Pin for a safe, minimally compromised async-scoped API

## Usage

```rust
let mut scoped = pin!(Scoped::new(Mutex::new(0)));

for i in 0..n {
    scoped.as_mut().spawn(move |state: &Mutex<u64>| {
        async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(10 * i)).await;
            *state.lock().unwrap() += 1;
            tokio::time::sleep(tokio::time::Duration::from_millis(10 * i)).await;
        }
        .boxed()
    });
}

scoped.await.into_inner().unwrap()
```
