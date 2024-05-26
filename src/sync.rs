#[cfg(loom)]
pub use loom::sync::{Condvar, Mutex};
#[cfg(not(loom))]
pub use std::sync::{Condvar, Mutex};

#[cfg(not(loom))]
use std::marker::PhantomData;
use std::mem::ManuallyDrop;

#[cfg(loom)]
use loom::cell::UnsafeCell;
#[cfg(not(loom))]
use std::cell::UnsafeCell;

pub struct ManuallyDropCell<T> {
    cell: UnsafeCell<ManuallyDrop<T>>,
}

impl<T> ManuallyDropCell<T> {
    pub fn new(t: T) -> Self {
        ManuallyDropCell {
            cell: UnsafeCell::new(ManuallyDrop::new(t)),
        }
    }

    pub unsafe fn take(&self) -> T {
        #[cfg(loom)]
        let state = unsafe { self.cell.with_mut(|s| ManuallyDrop::take(&mut *s)) };
        #[cfg(not(loom))]
        let state = unsafe { ManuallyDrop::take(&mut *self.cell.get()) };

        state
    }

    pub unsafe fn drop(&self) {
        #[cfg(loom)]
        unsafe {
            self.cell.with_mut(|s| ManuallyDrop::drop(&mut *s))
        };
        #[cfg(not(loom))]
        unsafe {
            ManuallyDrop::drop(&mut *self.cell.get())
        };
    }

    pub unsafe fn borrow<'a>(&self) -> (&'a T, ConstPtr<ManuallyDrop<T>>) {
        let ptr = self.cell.get();

        #[cfg(loom)]
        let ret = (ptr.with(|p| unsafe { &**p }), ptr);
        #[cfg(not(loom))]
        let ret = (unsafe { &**ptr }, ConstPtr(PhantomData));

        ret
    }
}

#[cfg(loom)]
pub use loom::cell::ConstPtr;

#[cfg(not(loom))]
pub struct ConstPtr<T>(PhantomData<*const T>);
