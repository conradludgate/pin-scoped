# pin-scoped

Abusing Pin for a safe, minimally compromised scoped async-task API

## Usage

```rust
use pin_scoped::TokioScope;

let mut scope = pin!(TokioScope::new(Mutex::new(0)));

// spawn n tasks, with a shared reference to the pinned-state only.
for i in 0..n {
    scope.as_mut().spawn(async move |state: &Mutex<u64>| {
        tokio::time::sleep(tokio::time::Duration::from_millis(10 * i)).await;
        *state.lock().unwrap() += 1;
        tokio::time::sleep(tokio::time::Duration::from_millis(10 * i)).await;
    });
}

// take back ownership of the scope.
scope.await.into_inner().unwrap()
```
