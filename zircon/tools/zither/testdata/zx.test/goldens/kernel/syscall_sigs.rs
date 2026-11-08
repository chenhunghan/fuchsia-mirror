// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// DO NOT EDIT. Generated from FIDL library zx by zither, a Fuchsia platform tool.

#[allow(unused_imports)]
use crate::object::HandleValue;
#[allow(unused_imports)]
use crate::user_copy::{UserInOutPtr, UserInPtr, UserOutPtr};
#[allow(unused_imports)]
use zx_status::Status;
#[allow(unused_imports)]
use zx_types::*;

#[allow(non_camel_case_types)]
pub type channel_create =
    extern "C" fn(u32, &mut HandleValue, &mut HandleValue) -> Result<(), Status>;

#[allow(non_camel_case_types)]
pub type channel_read = extern "C" fn(
    HandleValue,
    u32,
    UserOutPtr<u8>,
    UserOutPtr<zx_handle_t>,
    u32,
    u32,
    UserOutPtr<u32>,
    UserOutPtr<u32>,
) -> Result<(), Status>;

#[allow(non_camel_case_types)]
pub type channel_write = extern "C" fn(
    HandleValue,
    u32,
    UserInPtr<u8>,
    u32,
    UserInPtr<zx_handle_t>,
    u32,
) -> Result<(), Status>;

#[allow(non_camel_case_types)]
pub type clock_get_monotonic = extern "C" fn() -> zx_time_t;

#[allow(non_camel_case_types)]
pub type clock_get_monotonic_via_kernel = extern "C" fn() -> zx_time_t;

#[allow(non_camel_case_types)]
pub type handle_close_many = extern "C" fn(UserInPtr<zx_handle_t>, usize) -> Result<(), Status>;

#[allow(non_camel_case_types)]
pub type ktrace_control =
    extern "C" fn(HandleValue, u32, u32, UserInOutPtr<u8>) -> Result<(), Status>;

#[allow(non_camel_case_types)]
pub type nanosleep = extern "C" fn(zx_time_t) -> Result<(), Status>;

#[allow(non_camel_case_types)]
pub type process_exit = extern "C" fn(i64) -> ();

#[allow(non_camel_case_types)]
pub type syscall_next = extern "C" fn() -> ();

#[allow(non_camel_case_types)]
pub type syscall_test0 = extern "C" fn() -> ();

#[allow(non_camel_case_types)]
pub type syscall_test1 = extern "C" fn() -> ();

#[allow(non_camel_case_types)]
pub type syscall_test2 = extern "C" fn() -> ();

#[allow(non_camel_case_types)]
pub type system_get_page_size = extern "C" fn() -> u32;
