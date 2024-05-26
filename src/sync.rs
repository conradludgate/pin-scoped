#[cfg(loom)]
pub use loom::sync::{Condvar, Mutex};
#[cfg(not(loom))]
pub use std::sync::{Condvar, Mutex};

#[cfg(loom)]
pub use loom::cell::UnsafeCell;
#[cfg(not(loom))]
pub use std::cell::UnsafeCell;
