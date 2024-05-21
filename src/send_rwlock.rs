// Copyright 2016 Amanieu d'Antras
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use core::{
    cell::Cell,
    sync::atomic::{AtomicUsize, Ordering},
};
use parking_lot_core::{
    self, FilterOp, ParkResult, ParkToken, SpinWait, UnparkResult, UnparkToken,
};
use std::process::abort;

// UnparkToken used to indicate that that the target thread should attempt to
// lock the mutex again as soon as it is unparked.
pub(crate) const TOKEN_NORMAL: UnparkToken = UnparkToken(0);

// UnparkToken used to indicate that the mutex is being handed off to the target
// thread directly without unlocking it.
pub(crate) const TOKEN_HANDOFF: UnparkToken = UnparkToken(1);

// This reader-writer lock implementation is based on Boost's upgrade_mutex:
// https://github.com/boostorg/thread/blob/fc08c1fe2840baeeee143440fba31ef9e9a813c8/include/boost/thread/v2/shared_mutex.hpp#L432
//
// This implementation uses 2 wait queues, one at key [addr] and one at key
// [addr + 1]. The primary queue is used for all new waiting threads, and the
// secondary queue is used by the thread which has acquired WRITER_BIT but is
// waiting for the remaining readers to exit the lock.
//
// This implementation is fair between readers and writers since it uses the
// order in which threads first started queuing to alternate between read phases
// and write phases. In particular is it not vulnerable to write starvation
// since readers will block if there is a pending writer.

// There is at least one thread in the main queue.
const PARKED_BIT: usize = 0b0001;
// There is a parked thread holding WRITER_BIT. WRITER_BIT must be set.
const WRITER_PARKED_BIT: usize = 0b0010;
// If the reader count is zero: a writer is currently holding an exclusive lock.
// Otherwise: a writer is waiting for the remaining readers to exit the lock.
const WRITER_BIT: usize = 0b100;
// Mask of bits used to count readers.
const READERS_MASK: usize = !0b111;
// Base unit for counting readers.
const ONE_READER: usize = 0b1000;

// Token indicating what type of lock a queued thread is trying to acquire
const TOKEN_SHARED: ParkToken = ParkToken(ONE_READER);
const TOKEN_EXCLUSIVE: ParkToken = ParkToken(WRITER_BIT);

/// Raw reader-writer lock type backed by the parking lot.
pub struct RawRwLock {
    state: AtomicUsize,
}

impl RawRwLock {
    pub fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
        }
    }

    #[inline]
    pub fn lock_exclusive(&self) {
        if self
            .state
            .compare_exchange_weak(0, WRITER_BIT, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            let result = self.lock_exclusive_slow();
            debug_assert!(result);
        }
    }

    #[inline]
    pub fn try_lock_exclusive(&self) -> bool {
        self.state
            .compare_exchange(0, WRITER_BIT, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    #[inline]
    pub unsafe fn unlock_exclusive(&self) {
        if self
            .state
            .compare_exchange(WRITER_BIT, 0, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
        self.unlock_exclusive_slow(false);
    }

    #[inline]
    pub fn lock_shared(&self) {
        if !self.try_lock_shared_fast(false) {
            let result = self.lock_shared_slow(false);
            debug_assert!(result);
        }
    }

    #[inline]
    pub fn try_lock_shared(&self) -> bool {
        if self.try_lock_shared_fast(false) {
            true
        } else {
            self.try_lock_shared_slow(false)
        }
    }

    #[inline]
    pub unsafe fn unlock_shared(&self) {
        let state = self.state.fetch_sub(ONE_READER, Ordering::Release);
        if state & (READERS_MASK | WRITER_PARKED_BIT) == (ONE_READER | WRITER_PARKED_BIT) {
            self.unlock_shared_slow();
        }
    }

    #[inline]
    pub fn is_locked(&self) -> bool {
        let state = self.state.load(Ordering::Relaxed);
        state & (WRITER_BIT | READERS_MASK) != 0
    }

    #[inline]
    pub fn is_locked_exclusive(&self) -> bool {
        let state = self.state.load(Ordering::Relaxed);
        state & (WRITER_BIT) != 0
    }
}

impl RawRwLock {
    #[inline(always)]
    fn try_lock_shared_fast(&self, recursive: bool) -> bool {
        let state = self.state.load(Ordering::Relaxed);

        // We can't allow grabbing a shared lock if there is a writer, even if
        // the writer is still waiting for the remaining readers to exit.
        if state & WRITER_BIT != 0 {
            // To allow recursive locks, we make an exception and allow readers
            // to skip ahead of a pending writer to avoid deadlocking, at the
            // cost of breaking the fairness guarantees.
            if !recursive || state & READERS_MASK == 0 {
                return false;
            }
        }

        if let Some(new_state) = state.checked_add(ONE_READER) {
            self.state
                .compare_exchange_weak(state, new_state, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        } else {
            false
        }
    }

    #[cold]
    fn try_lock_shared_slow(&self, recursive: bool) -> bool {
        let mut state = self.state.load(Ordering::Relaxed);
        loop {
            // This mirrors the condition in try_lock_shared_fast
            #[allow(clippy::collapsible_if)]
            if state & WRITER_BIT != 0 {
                if !recursive || state & READERS_MASK == 0 {
                    return false;
                }
            }
            match self.state.compare_exchange_weak(
                state,
                state
                    .checked_add(ONE_READER)
                    .expect("RwLock reader count overflow"),
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(x) => state = x,
            }
        }
    }

    #[cold]
    fn lock_exclusive_slow(&self) -> bool {
        let try_lock = |state: &mut usize| {
            loop {
                if *state & WRITER_BIT != 0 {
                    return false;
                }

                // Grab WRITER_BIT if it isn't set, even if there are parked threads.
                match self.state.compare_exchange_weak(
                    *state,
                    *state | WRITER_BIT,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return true,
                    Err(x) => *state = x,
                }
            }
        };

        // Step 1: grab exclusive ownership of WRITER_BIT
        let timed_out = !self.lock_common(TOKEN_EXCLUSIVE, try_lock, WRITER_BIT);
        if timed_out {
            return false;
        }

        // Step 2: wait for all remaining readers to exit the lock.
        self.wait_for_readers(0)
    }

    #[cold]
    fn unlock_exclusive_slow(&self, force_fair: bool) {
        // There are threads to unpark. Try to unpark as many as we can.
        let callback = |mut new_state, result: UnparkResult| {
            // If we are using a fair unlock then we should keep the
            // rwlock locked and hand it off to the unparked threads.
            if result.unparked_threads != 0 && (force_fair || result.be_fair) {
                if result.have_more_threads {
                    new_state |= PARKED_BIT;
                }
                self.state.store(new_state, Ordering::Release);
                TOKEN_HANDOFF
            } else {
                // Clear the parked bit if there are no more parked threads.
                if result.have_more_threads {
                    self.state.store(PARKED_BIT, Ordering::Release);
                } else {
                    self.state.store(0, Ordering::Release);
                }
                TOKEN_NORMAL
            }
        };
        // SAFETY: `callback` does not panic or call into any function of `parking_lot`.
        unsafe {
            self.wake_parked_threads(0, callback);
        }
    }

    #[cold]
    fn lock_shared_slow(&self, recursive: bool) -> bool {
        let try_lock = |state: &mut usize| {
            let mut spinwait_shared = SpinWait::new();
            loop {
                // This is the same condition as try_lock_shared_fast
                #[allow(clippy::collapsible_if)]
                if *state & WRITER_BIT != 0 {
                    if !recursive || *state & READERS_MASK == 0 {
                        return false;
                    }
                }

                if self
                    .state
                    .compare_exchange_weak(
                        *state,
                        state
                            .checked_add(ONE_READER)
                            .expect("RwLock reader count overflow"),
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    return true;
                }

                // If there is high contention on the reader count then we want
                // to leave some time between attempts to acquire the lock to
                // let other threads make progress.
                spinwait_shared.spin_no_yield();
                *state = self.state.load(Ordering::Relaxed);
            }
        };
        self.lock_common(TOKEN_SHARED, try_lock, WRITER_BIT)
    }

    #[cold]
    fn unlock_shared_slow(&self) {
        // At this point WRITER_PARKED_BIT is set and READER_MASK is empty. We
        // just need to wake up a potentially sleeping pending writer.
        // Using the 2nd key at addr + 1
        let addr = self as *const _ as usize + 1;
        let callback = |_result: UnparkResult| {
            // Clear the WRITER_PARKED_BIT here since there can only be one
            // parked writer thread.
            self.state.fetch_and(!WRITER_PARKED_BIT, Ordering::Relaxed);
            TOKEN_NORMAL
        };
        // SAFETY:
        //   * `addr` is an address we control.
        //   * `callback` does not panic or call into any function of `parking_lot`.
        unsafe {
            parking_lot_core::unpark_one(addr, callback);
        }
    }

    /// Common code for waking up parked threads after releasing `WRITER_BIT`.
    ///
    /// # Safety
    ///
    /// `callback` must uphold the requirements of the `callback` parameter to
    /// `parking_lot_core::unpark_filter`. Meaning no panics or calls into any function in
    /// `parking_lot`.
    #[inline]
    unsafe fn wake_parked_threads(
        &self,
        new_state: usize,
        callback: impl FnOnce(usize, UnparkResult) -> UnparkToken,
    ) {
        // We must wake up at least one upgrader or writer if there is one,
        // otherwise they may end up parked indefinitely since unlock_shared
        // does not call wake_parked_threads.
        let new_state = Cell::new(new_state);
        let addr = self as *const _ as usize;
        let filter = |ParkToken(token)| {
            let s = new_state.get();

            // If we are waking up a writer, don't wake anything else.
            if s & WRITER_BIT != 0 {
                return FilterOp::Stop;
            }

            // Otherwise wake *all* readers and one writer.
            if token & WRITER_BIT != 0 {
                // Skip writers and upgradable readers if we already have
                // a writer/upgradable reader.
                FilterOp::Skip
            } else {
                new_state.set(s + token);
                FilterOp::Unpark
            }
        };
        let callback = |result| callback(new_state.get(), result);
        // SAFETY:
        // * `addr` is an address we control.
        // * `filter` does not panic or call into any function of `parking_lot`.
        // * `callback` safety responsibility is on caller
        parking_lot_core::unpark_filter(addr, filter, callback);
    }

    // Common code for waiting for readers to exit the lock after acquiring
    // WRITER_BIT.
    #[inline]
    fn wait_for_readers(&self, prev_value: usize) -> bool {
        // At this point WRITER_BIT is already set, we just need to wait for the
        // remaining readers to exit the lock.
        let mut spinwait = SpinWait::new();
        let mut state = self.state.load(Ordering::Acquire);
        while state & READERS_MASK != 0 {
            // Spin a few times to wait for readers to exit
            if spinwait.spin() {
                state = self.state.load(Ordering::Acquire);
                continue;
            }

            // Set the parked bit
            if state & WRITER_PARKED_BIT == 0 {
                if let Err(x) = self.state.compare_exchange_weak(
                    state,
                    state | WRITER_PARKED_BIT,
                    Ordering::Acquire,
                    Ordering::Acquire,
                ) {
                    state = x;
                    continue;
                }
            }

            // Park our thread until we are woken up by an unlock
            // Using the 2nd key at addr + 1
            let addr = self as *const _ as usize + 1;
            let validate = || {
                let state = self.state.load(Ordering::Relaxed);
                state & READERS_MASK != 0 && state & WRITER_PARKED_BIT != 0
            };
            let before_sleep = || {};
            let timed_out = |_, _| {};
            // SAFETY:
            //   * `addr` is an address we control.
            //   * `validate`/`timed_out` does not panic or call into any function of `parking_lot`.
            //   * `before_sleep` does not call `park`, nor does it panic.
            let park_result = unsafe {
                parking_lot_core::park(
                    addr,
                    validate,
                    before_sleep,
                    timed_out,
                    TOKEN_EXCLUSIVE,
                    None,
                )
            };
            match park_result {
                // We still need to re-check the state if we are unparked
                // since a previous writer timing-out could have allowed
                // another reader to sneak in before we parked.
                ParkResult::Unparked(_) | ParkResult::Invalid => {
                    state = self.state.load(Ordering::Acquire);
                    continue;
                }

                // Timeout expired
                ParkResult::TimedOut => {
                    // We need to release WRITER_BIT and revert back to
                    // our previous value. We also wake up any threads that
                    // might be waiting on WRITER_BIT.
                    let state = self.state.fetch_add(
                        prev_value.wrapping_sub(WRITER_BIT | WRITER_PARKED_BIT),
                        Ordering::Relaxed,
                    );
                    if state & PARKED_BIT != 0 {
                        let callback = |_, result: UnparkResult| {
                            // Clear the parked bit if there no more parked threads
                            if !result.have_more_threads {
                                self.state.fetch_and(!PARKED_BIT, Ordering::Relaxed);
                            }
                            TOKEN_NORMAL
                        };
                        // SAFETY: `callback` does not panic or call any function of `parking_lot`.
                        unsafe {
                            self.wake_parked_threads(ONE_READER, callback);
                        }
                    }
                    return false;
                }
            }
        }
        true
    }

    /// Common code for acquiring a lock
    #[inline]
    fn lock_common(
        &self,
        token: ParkToken,
        mut try_lock: impl FnMut(&mut usize) -> bool,
        validate_flags: usize,
    ) -> bool {
        let mut spinwait = SpinWait::new();
        let mut state = self.state.load(Ordering::Relaxed);
        loop {
            // Attempt to grab the lock
            if try_lock(&mut state) {
                return true;
            }

            // If there are no parked threads, try spinning a few times.
            if state & (PARKED_BIT | WRITER_PARKED_BIT) == 0 && spinwait.spin() {
                state = self.state.load(Ordering::Relaxed);
                continue;
            }

            // Set the parked bit
            if state & PARKED_BIT == 0 {
                if let Err(x) = self.state.compare_exchange_weak(
                    state,
                    state | PARKED_BIT,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    state = x;
                    continue;
                }
            }

            // Park our thread until we are woken up by an unlock
            let addr = self as *const _ as usize;
            let validate = || {
                let state = self.state.load(Ordering::Relaxed);
                state & PARKED_BIT != 0 && (state & validate_flags != 0)
            };
            let before_sleep = || {};
            let timed_out = |_, _| {};

            // SAFETY:
            // * `addr` is an address we control.
            // * `validate`/`timed_out` does not panic or call into any function of `parking_lot`.
            // * `before_sleep` does not call `park`, nor does it panic.
            let park_result = unsafe {
                parking_lot_core::park(addr, validate, before_sleep, timed_out, token, None)
            };
            match park_result {
                // The thread that unparked us passed the lock on to us
                // directly without unlocking it.
                ParkResult::Unparked(TOKEN_HANDOFF) => return true,

                // We were unparked normally, try acquiring the lock again
                ParkResult::Unparked(_) => (),

                // The validation function failed, try locking again
                ParkResult::Invalid => (),

                // Timeout expired
                ParkResult::TimedOut => abort(),
            }

            // Loop back and try locking again
            spinwait.reset();
            state = self.state.load(Ordering::Relaxed);
        }
    }
}
