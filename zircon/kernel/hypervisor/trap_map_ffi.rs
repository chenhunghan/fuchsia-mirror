// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use fbl::RefPtr;
use pin_init::PinInit;
use zx_status::Status;
use zx_types::{ZX_OK, zx_gpaddr_t, zx_port_packet_t, zx_status_t};

use crate::object::PortDispatcher;

use super::trap_map::{Trap, TrapMap};

/// Queues `packet` on a `Trap`'s port for C++ callers.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub extern "C" fn rust_trap_queue(trap: &Trap, packet: &zx_port_packet_t) -> zx_status_t {
    Status::result_into_raw(trap.queue(packet, None))
}

/// Returns `true` if `trap` has an associated port.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub extern "C" fn rust_trap_has_port(trap: &Trap) -> bool {
    trap.has_port()
}

/// Returns the kind (`ZX_GUEST_TRAP_*`) of `trap`.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub extern "C" fn rust_trap_kind(trap: &Trap) -> u32 {
    trap.kind()
}

/// Returns the port packet key of `trap`.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub extern "C" fn rust_trap_key(trap: &Trap) -> u64 {
    trap.key()
}

/// Initializes a `TrapMap` in place for C++ callers.
///
/// # Safety
///
/// `trap_map` must be a valid, uninitialized, properly aligned pointer to a `TrapMap` that remains
/// pinned until `rust_trap_map_destroy`.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_trap_map_new(trap_map: *mut TrapMap) {
    // SAFETY: Guaranteed by caller.
    let _ = unsafe { TrapMap::new().__pinned_init(trap_map) };
}

/// Destroys a `TrapMap` in place for C++ callers.
///
/// # Safety
///
/// `trap_map` must be a valid initialized `TrapMap`.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_trap_map_destroy(trap_map: *mut TrapMap) {
    // SAFETY: Guaranteed by caller.
    unsafe {
        core::ptr::drop_in_place(trap_map);
    }
}

/// Inserts a trap into `trap_map` for C++ callers.
///
/// # Safety
///
/// `port` must be either null or a raw pointer exported from `fbl::RefPtr<PortDispatcher>`.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_trap_map_insert_trap(
    trap_map: &TrapMap,
    kind: u32,
    addr: zx_gpaddr_t,
    len: usize,
    port: *mut PortDispatcher,
    key: u64,
) -> zx_status_t {
    // SAFETY: `port` is either null or exported from `fbl::RefPtr<PortDispatcher>`.
    let port = unsafe { RefPtr::try_from_raw(port) };
    Status::result_into_raw(trap_map.insert_trap(kind, addr, len, port, key))
}

/// Finds a trap matching `(kind, addr)` in `trap_map` for C++ callers.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub extern "C" fn rust_trap_map_find_trap(
    trap_map: &TrapMap,
    kind: u32,
    addr: zx_gpaddr_t,
    out: &mut *mut Trap,
) -> zx_status_t {
    match trap_map.find_trap(kind, addr) {
        Ok(trap) => {
            *out = trap.as_mut_ptr();
            ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}
