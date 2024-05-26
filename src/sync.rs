#[cfg(loom)]
pub use loom::sync::{Condvar, Mutex};
#[cfg(not(loom))]
pub use std::sync::{Condvar, Mutex};

#[cfg(not(loom))]
use std::marker::PhantomData;
use std::{marker::PhantomPinned, mem::ManuallyDrop, pin::Pin};

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

    pub unsafe fn borrow<'a>(&self) -> (&'a T, GuardPtr<ManuallyDrop<T>>) {
        let ptr = self.cell.get();

        #[cfg(loom)]
        let ret = (ptr.with(|p| unsafe { &**p }), GuardPtr(Some(ptr)));
        #[cfg(not(loom))]
        let ret = (unsafe { &**ptr }, GuardPtr(PhantomData));

        ret
    }
}

#[cfg(not(loom))]
pub struct GuardPtr<T>(PhantomData<*const T>);

#[cfg(loom)]
pub struct GuardPtr<T>(Option<loom::cell::ConstPtr<T>>);

// Safety:
// we never read/write the pointer value, it's only for tracking in loom.
unsafe impl<T> Send for GuardPtr<T> {}

impl<T> GuardPtr<T> {
    pub fn release(&mut self) {
        #[cfg(loom)]
        {
            self.0 = None;
        }
    }
}

/// An unboxed aliasable value.
#[derive(Default)]
pub struct Aliasable<T> {
    val: T,
    _pinned: PhantomPinned,
}

impl<T> Aliasable<T> {
    /// Create a new `Aliasable` that stores `val`.
    #[must_use]
    #[inline]
    pub fn new(val: T) -> Self {
        Self {
            val,
            _pinned: PhantomPinned,
        }
    }

    /// Get a shared reference to the value inside the `Aliasable`.
    ///
    /// This method takes [`Pin`]`<&Self>` instead of `&self` to enforce that all parent containers
    /// are `!`[`Unpin`], and thus won't be annotated with `noalias`.
    ///
    /// This crate intentionally does not provide a method to get an `&mut T`, because the value
    /// may be shared. To obtain an `&mut T` you should use an interior mutable container such as a
    /// mutex or [`UnsafeCell`](core::cell::UnsafeCell).
    #[must_use]
    #[inline]
    pub fn get(self: Pin<&Self>) -> &T {
        &self.get_ref().val
    }

    /// Get a shared reference to the value inside the `Aliasable` with an extended lifetime.
    ///
    /// # Safety
    ///
    /// The reference must not be held for longer than the `Aliasable` exists.
    #[must_use]
    #[inline]
    pub unsafe fn get_extended<'a>(self: Pin<&Self>) -> &'a T {
        unsafe { &*(self.get() as *const T) }
    }
}
