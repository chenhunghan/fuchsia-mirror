// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::dispatcher::{PeeredDispatcher, StateLockGuard};
use crate::user_copy::{UserInPtr, UserOutPtr};
use zx_status::Status;
use zx_types::{ZX_FIFO_READABLE, ZX_FIFO_WRITABLE};

impl FifoDispatcher {
    /// Writes `count` elements of `elem_size` bytes from `ptr` into the peer.
    ///
    /// Returns ZX_ERR_OUT_OF_RANGE if `elem_size` does not match the fifo.
    pub fn write_from_user(
        &self,
        elem_size: usize,
        ptr: UserInPtr<u8>,
        count: usize,
    ) -> Result<usize, Status> {
        self.canary.assert();

        ksync::lock!(let guard = self.lock());
        let Some(peer) = self.peer(&guard) else {
            return Err(Status::BAD_STATE);
        };
        peer.write_self_locked(&guard, elem_size, ptr, count)
    }

    fn write_self_locked(
        &self,
        guard: &StateLockGuard<'_>,
        elem_size: usize,
        mut ptr: UserInPtr<u8>,
        mut count: usize,
    ) -> Result<usize, Status> {
        self.canary.assert();

        if elem_size != self.elem_size as usize {
            return Err(Status::OUT_OF_RANGE);
        }
        if count == 0 {
            return Err(Status::OUT_OF_RANGE);
        }

        let old_head = guard.head();

        // Total number of available empty slots in the fifo.
        let avail = (self.elem_count - guard.head().wrapping_sub(guard.tail())) as usize;

        if avail == 0 {
            return Err(Status::SHOULD_WAIT);
        }

        let was_empty = guard.head() == guard.tail();

        if count > avail {
            count = avail;
        }

        while count > 0 {
            let offset = guard.head() % self.elem_count;

            // Number of slots from target to end of fifo.
            let mut n = (self.elem_count - offset) as usize;
            if n > count {
                n = count;
            }

            ptr.copy_array_from_user(self.data_mut(guard, offset, n))?;

            guard.set_head(guard.head().wrapping_add(n as u32));
            count -= n;
            ptr = ptr.byte_offset(n * self.elem_size as usize);
        }

        if was_empty {
            self.update_state_locked(guard, 0, ZX_FIFO_READABLE);
        }

        if self.is_full_locked(guard) {
            self.update_state_locked(guard, ZX_FIFO_WRITABLE, 0);
        }

        Ok(guard.head().wrapping_sub(old_head) as usize)
    }

    /// Reads up to `count` elements of `elem_size` bytes into `ptr`.
    pub fn read_to_user(
        &self,
        elem_size: usize,
        mut ptr: UserOutPtr<u8>,
        mut count: usize,
    ) -> Result<usize, Status> {
        self.canary.assert();

        ksync::lock!(let guard = self.lock());

        if elem_size != self.elem_size as usize {
            return Err(Status::OUT_OF_RANGE);
        }
        if count == 0 {
            return Err(Status::OUT_OF_RANGE);
        }

        // Total number of available entries to read from the fifo.
        let avail = guard.head().wrapping_sub(guard.tail()) as usize;

        if avail == 0 {
            return Err(if self.peer(&guard).is_some() {
                Status::SHOULD_WAIT
            } else {
                Status::PEER_CLOSED
            });
        }

        let was_full = self.is_full_locked(&guard);

        if count > avail {
            count = avail;
        }

        let old_tail = guard.tail();
        while count > 0 {
            let offset = guard.tail() % self.elem_count;

            // Number of slots from target to end of fifo.
            let mut n = (self.elem_count - offset) as usize;
            if n > count {
                n = count;
            }

            if ptr.copy_array_to_user(self.data(&guard, offset, n)).is_err() {
                // Roll back, in case this is the second copy.
                guard.set_tail(old_tail);
                return Err(Status::INVALID_ARGS);
            }

            guard.set_tail(guard.tail().wrapping_add(n as u32));
            count -= n;
            ptr = ptr.byte_offset(n * self.elem_size as usize);
        }

        // If the fifo was full, it is now writable.
        if was_full {
            if let Some(peer) = self.peer(&guard) {
                peer.update_state_locked(&guard, 0, ZX_FIFO_WRITABLE);
            }
        }

        Ok(guard.tail().wrapping_sub(old_tail) as usize)
    }

    /// Returns true if the fifo has no room for another element.
    fn is_full_locked(&self, guard: &StateLockGuard<'_>) -> bool {
        guard.head().wrapping_sub(guard.tail()) == self.elem_count
    }
}
