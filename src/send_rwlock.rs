// Copyright 2016 Amanieu d'Antras
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use core::sync::atomic::{AtomicUsize, Ordering};
use parking_lot_core::{self, ParkResult, ParkToken, SpinWait, UnparkResult, UnparkToken};
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
            self.lock_exclusive_slow()
        }
    }

    #[inline]
    pub fn try_lock_exclusive(&self) -> bool {
        self.state
            .compare_exchange(0, WRITER_BIT, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    #[inline]
    pub fn lock_shared(&self) {
        let mut state = self.state.load(Ordering::Relaxed);
        loop {
            if state & WRITER_BIT != 0 {
                panic!("inconsistent state. writer bit cannot be set")
            }
            match self.state.compare_exchange_weak(
                state,
                state.checked_add(ONE_READER).expect("task count overflow"),
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(x) => state = x,
            }
        }
    }

    #[inline]
    pub unsafe fn unlock_shared(&self) {
        let state = self.state.fetch_sub(ONE_READER, Ordering::Release);
        if state & (READERS_MASK | WRITER_PARKED_BIT) == (ONE_READER | WRITER_PARKED_BIT) {
            self.unlock_shared_slow();
        }
    }
}

impl RawRwLock {
    #[cold]
    fn lock_exclusive_slow(&self) {
        // Step 1: grab exclusive ownership of WRITER_BIT
        let mut spinwait = SpinWait::new();
        let mut state = self.state.load(Ordering::Relaxed);
        loop {
            // Attempt to grab the lock
            if state & WRITER_BIT == 0 {
                // Grab WRITER_BIT if it isn't set, even if there are parked threads.
                match self.state.compare_exchange_weak(
                    state,
                    state | WRITER_BIT,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(x) => {
                        state = x;
                        continue;
                    }
                }
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
                state & PARKED_BIT != 0 && (state & WRITER_BIT != 0)
            };
            let before_sleep = || {};
            let timed_out = |_, _| {};

            // SAFETY:
            // * `addr` is an address we control.
            // * `validate`/`timed_out` does not panic or call into any function of `parking_lot`.
            // * `before_sleep` does not call `park`, nor does it panic.
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
                // The thread that unparked us passed the lock on to us
                // directly without unlocking it.
                ParkResult::Unparked(TOKEN_HANDOFF) => break,

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

        // Step 2: wait for all remaining readers to exit the lock.
        self.wait_for_readers();
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

    // Common code for waiting for readers to exit the lock after acquiring
    // WRITER_BIT.
    #[inline]
    fn wait_for_readers(&self) {
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
                ParkResult::TimedOut => abort(),
            }
        }
    }
}
