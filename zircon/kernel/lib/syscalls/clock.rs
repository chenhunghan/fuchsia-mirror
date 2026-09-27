// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::counters::define_kcounter;
use crate::object::{ClockDispatcher, Dispatcher, HandleValue};
use crate::user_copy::{UserInPtr, UserOutPtr};
use debug::ltracef;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{
    ZX_CLOCK_ARGS_VERSION_MASK, ZX_CLOCK_ARGS_VERSION_SHIFT, ZX_CLOCK_UPDATE_MAX_RATE_ADJUST,
    ZX_CLOCK_UPDATE_MIN_RATE_ADJUST, ZX_CLOCK_UPDATE_OPTION_RATE_ADJUST_VALID,
    ZX_CLOCK_UPDATE_OPTIONS_ALL, ZX_RIGHT_READ, ZX_RIGHT_WRITE, zx_clock_args_version,
    zx_clock_create_args_v1_t, zx_clock_details_v1_t, zx_clock_update_args_v1_t,
    zx_clock_update_args_v2_t, zx_instant_boot_t, zx_instant_mono_t, zx_time_t,
};

const LOCAL_TRACE: u32 = 0;

const fn get_args_version(options: u64) -> u64 {
    (options & ZX_CLOCK_ARGS_VERSION_MASK) >> ZX_CLOCK_ARGS_VERSION_SHIFT
}

define_kcounter!(SYSCALLS_ZX_CLOCK_GET_MONOTONIC, "syscalls.zx_clock_get_monotonic", Sum);
define_kcounter!(SYSCALLS_ZX_CLOCK_GET_BOOT, "syscalls.zx_clock_get_boot", Sum);

#[syscall]
pub fn sys_clock_get_monotonic_via_kernel() -> zx_instant_mono_t {
    SYSCALLS_ZX_CLOCK_GET_MONOTONIC.add(1);
    crate::platform_rs::timer::current_mono_time().0
}

#[syscall]
pub fn sys_clock_get_boot_via_kernel() -> zx_instant_boot_t {
    SYSCALLS_ZX_CLOCK_GET_BOOT.add(1);
    crate::platform_rs::timer::current_boot_time().0
}

#[syscall]
pub fn sys_clock_create(
    options: u64,
    user_args: UserInPtr<u8>,
    out: &mut HandleValue,
) -> Result<(), Status> {
    ltracef!("options 0x{:x}, user_args {:?}\n", options, user_args);

    let mut args = zx_clock_create_args_v1_t::default();

    // Extract the creation arguments based on the version signalled in options.
    match get_args_version(options) {
        // v0 implies "just use the defaults". No args structure should have been
        // passed. Just set our local v1 args structure to the default backstop
        // time of 0.
        0 => {
            if !user_args.is_null() {
                return Err(Status::INVALID_ARGS);
            }
            args.backstop_time = 0;
        }

        // Extract the user args from the v1 structure. They will be sanity checked
        // during the dispatcher's static Create.
        1 => {
            args = user_args.reinterpret::<zx_clock_create_args_v1_t>().read()?;
        }

        _ => return Err(Status::INVALID_ARGS),
    }

    let (kernel_handle, rights) = ClockDispatcher::create(options, &args)?;
    let user_handle = kernel_handle.make_and_add_handle(rights)?;
    *out = user_handle;
    Ok(())
}

#[syscall]
pub fn sys_clock_read(
    clock_handle: HandleValue,
    user_now: UserOutPtr<zx_time_t>,
) -> Result<(), Status> {
    ltracef!("clock_handle 0x{:x}, user_now {:?}\n", clock_handle.raw_value(), user_now);

    let clock = Dispatcher::get_with_rights::<ClockDispatcher>(clock_handle, ZX_RIGHT_READ)?;
    let now = clock.read()?;
    user_now.write(now)?;
    Ok(())
}

#[syscall]
pub fn sys_clock_get_details(
    clock_handle: HandleValue,
    options: u64,
    user_details: UserOutPtr<u8>,
) -> Result<(), Status> {
    ltracef!(
        "clock_handle 0x{:x}, options 0x{:x}, user_details {:?}\n",
        clock_handle.raw_value(),
        options,
        user_details
    );

    // Currently, the only version of the details structure defined is V1. If the
    // user failed to provide a buffer, or signaled a different version of the
    // structure, then it is an error.
    if (options != zx_clock_args_version(1)) || user_details.is_null() {
        return Err(Status::INVALID_ARGS);
    }

    let clock = Dispatcher::get_with_rights::<ClockDispatcher>(clock_handle, ZX_RIGHT_READ)?;
    let details = clock.get_details()?;
    user_details.reinterpret::<zx_clock_details_v1_t>().write(details)?;
    Ok(())
}

#[syscall]
pub fn sys_clock_update(
    clock_handle: HandleValue,
    mut options: u64,
    user_args: UserInPtr<u8>,
) -> Result<(), Status> {
    ltracef!(
        "clock_handle 0x{:x}, options 0x{:x}, user_args {:?}\n",
        clock_handle.raw_value(),
        options,
        user_args
    );

    // Currently, there are only 2 versions of the update structure defined; V1
    // and V2. If the user failed to provide a buffer, or signaled any other
    // version of the structure, then it is an error.
    if user_args.is_null() {
        return Err(Status::INVALID_ARGS);
    }

    enum UpdateArgs {
        V1(zx_clock_update_args_v1_t),
        V2(zx_clock_update_args_v2_t),
    }

    let version = get_args_version(options);
    let args = match version {
        1 => UpdateArgs::V1(user_args.reinterpret::<zx_clock_update_args_v1_t>().read()?),
        2 => UpdateArgs::V2(user_args.reinterpret::<zx_clock_update_args_v2_t>().read()?),
        _ => return Err(Status::INVALID_ARGS),
    };

    let rate_adjust = match &args {
        UpdateArgs::V1(v1) => v1.rate_adjust,
        UpdateArgs::V2(v2) => v2.rate_adjust,
    };

    // Before going further, perform basic sanity checks of the update arguments.
    //
    // Only the defined options may be present in the request, and at least one of
    // them must be specified.
    options &= !ZX_CLOCK_ARGS_VERSION_MASK;
    if (options & !ZX_CLOCK_UPDATE_OPTIONS_ALL) != 0 || (options & ZX_CLOCK_UPDATE_OPTIONS_ALL) == 0
    {
        return Err(Status::INVALID_ARGS);
    }

    // The PPM adjustment must be within the legal range.
    if (options & ZX_CLOCK_UPDATE_OPTION_RATE_ADJUST_VALID) != 0
        && !(ZX_CLOCK_UPDATE_MIN_RATE_ADJUST..=ZX_CLOCK_UPDATE_MAX_RATE_ADJUST)
            .contains(&rate_adjust)
    {
        return Err(Status::INVALID_ARGS);
    }

    let clock = Dispatcher::get_with_rights::<ClockDispatcher>(clock_handle, ZX_RIGHT_WRITE)?;

    match &args {
        UpdateArgs::V1(v1) => clock.update(options, v1),
        UpdateArgs::V2(v2) => clock.update(options, v2),
    }
}
