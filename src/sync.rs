#[cfg(pin_scoped_loom)]
pub use loom::sync::{Condvar, Mutex};

// on std, use condvar and mutex
// on no_std, use spin::Mutex and a fake condvar spinloop
#[cfg(not(pin_scoped_loom))]
pub use std::sync::{Condvar, Mutex};

#[cfg(not(pin_scoped_loom))]
use core::marker::PhantomData;
use core::mem::ManuallyDrop;

#[cfg(not(pin_scoped_loom))]
use core::cell::UnsafeCell;
#[cfg(pin_scoped_loom)]
use loom::cell::UnsafeCell;

pub struct ManuallyDropCell<T> {
    cell: UnsafeCell<ManuallyDrop<T>>,
}

impl<T> ManuallyDropCell<T> {
    pub fn new(t: T) -> Self {
        Self {
            cell: UnsafeCell::new(ManuallyDrop::new(t)),
        }
    }

    pub unsafe fn take(&self) -> T {
        #[cfg(pin_scoped_loom)]
        let state = unsafe { self.cell.with_mut(|s| ManuallyDrop::take(&mut *s)) };
        #[cfg(not(pin_scoped_loom))]
        let state = unsafe { ManuallyDrop::take(&mut *self.cell.get()) };

        state
    }

    pub unsafe fn drop(&self) {
        #[cfg(pin_scoped_loom)]
        unsafe {
            self.cell.with_mut(|s| ManuallyDrop::drop(&mut *s))
        };
        #[cfg(not(pin_scoped_loom))]
        unsafe {
            ManuallyDrop::drop(&mut *self.cell.get());
        };
    }

    pub unsafe fn borrow<'a>(&self) -> (&'a T, GuardPtr<ManuallyDrop<T>>) {
        let ptr = self.cell.get();

        #[cfg(pin_scoped_loom)]
        let ret = (ptr.with(|p| unsafe { &**p }), GuardPtr(Some(ptr)));
        #[cfg(not(pin_scoped_loom))]
        let ret = (unsafe { &**ptr }, GuardPtr(PhantomData));

        ret
    }
}

#[cfg(not(pin_scoped_loom))]
pub struct GuardPtr<T>(PhantomData<*const T>);

#[cfg(pin_scoped_loom)]
pub struct GuardPtr<T>(Option<loom::cell::ConstPtr<T>>);

// Safety:
// we never read/write the pointer value, it's only for tracking in loom.
unsafe impl<T> Send for GuardPtr<T> {}

impl<T> GuardPtr<T> {
    #[cfg_attr(not(pin_scoped_loom), allow(clippy::unused_self))]
    pub fn release(&mut self) {
        #[cfg(pin_scoped_loom)]
        {
            self.0 = None;
        }
    }
}
