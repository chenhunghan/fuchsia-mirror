// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::crypto::global_prng;
use crate::user_copy::{UserInPtr, UserOutPtr};
use core::ops::{Deref, DerefMut};
use explicit_memory::mandatory_memset;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{ZX_CPRNG_ADD_ENTROPY_MAX_LEN, ZX_CPRNG_DRAW_MAX_LEN};

const MAX_CPRNG_DRAW: usize = ZX_CPRNG_DRAW_MAX_LEN;
const MAX_CPRNG_SEED: usize = ZX_CPRNG_ADD_ENTROPY_MAX_LEN;

struct ZeroOnDrop<'a, T>(&'a mut [T]);

impl<'a, T> Deref for ZeroOnDrop<'a, T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl<'a, T> DerefMut for ZeroOnDrop<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
    }
}

impl<'a, T> Drop for ZeroOnDrop<'a, T> {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid slice of `size_of_val(self.0)` bytes.
        unsafe {
            mandatory_memset(self.0.as_mut_ptr() as *mut u8, 0, core::mem::size_of_val(self.0));
        }
    }
}

#[syscall]
pub fn sys_cprng_draw_once(buffer: UserOutPtr<u8>, len: usize) -> Result<(), Status> {
    if len > MAX_CPRNG_DRAW {
        return Err(Status::INVALID_ARGS);
    }

    let mut storage = [0u8; MAX_CPRNG_DRAW];
    // Ensure we get rid of the stack copy of the random data as this function returns.
    let mut kernel_buf = ZeroOnDrop(&mut storage);

    let prng = global_prng::get_instance();
    assert!(prng.is_thread_safe());
    prng.draw(&mut kernel_buf[..len]);

    buffer.copy_slice_to_user(&kernel_buf[..len]).map_err(|_| Status::INVALID_ARGS)?;
    Ok(())
}

#[syscall]
pub fn sys_cprng_add_entropy(buffer: UserInPtr<u8>, buffer_size: usize) -> Result<(), Status> {
    if buffer_size > MAX_CPRNG_SEED {
        return Err(Status::INVALID_ARGS);
    }

    let mut storage = [core::mem::MaybeUninit::<u8>::uninit(); MAX_CPRNG_SEED];
    // Ensure we get rid of the stack copy of the entropy as this function returns.
    let mut kernel_buf = ZeroOnDrop(&mut storage);

    let slice = buffer
        .copy_slice_from_user(&mut kernel_buf[..buffer_size])
        .map_err(|_| Status::INVALID_ARGS)?;

    global_prng::add_entropy(slice);
    Ok(())
}
