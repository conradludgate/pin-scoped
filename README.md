# pin-scoped

Abusing Pin for a safe, minimally compromised scoped async-task API

## Usage

```rust
use pin_scoped::{ScopeState, TokioScope};
use std::{pin::pin, sync::Mutex};
use tokio::runtime::Handle;

let n = 8_u64;
let scope = pin!(TokioScope::new(Mutex::new(0_u64)));

// spawn n tasks, with a shared reference to the pinned-state only.
for i in 0..n {
    scope.as_ref().spawn(async move |state: ScopeState<'_, Mutex<u64>, Handle>| {
        tokio::time::sleep(tokio::time::Duration::from_millis(10 * i)).await;
        *state.lock().unwrap() += 1;
        tokio::time::sleep(tokio::time::Duration::from_millis(10 * i)).await;
    });
}

// take back ownership of the scope.
let value = scope.await.into_inner().unwrap();
assert_eq!(value, n);
```
