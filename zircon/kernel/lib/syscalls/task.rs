// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::{
    Dispatcher, HandleValue, JobDispatcher, ProcessDispatcher, SuspendTokenDispatcher,
    ThreadDispatcher,
};
use crate::user_copy::UserInPtr;
use core::mem::MaybeUninit;
use debug::ltracef;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{ZX_MAX_NAME_LEN, ZX_RIGHT_DESTROY, ZX_RIGHT_WRITE, ZX_TASK_RETCODE_SYSCALL_KILL};

const LOCAL_TRACE: u32 = 0;

/// Copies a required task name from user memory into `buf`, rejecting null pointers.
///
/// Unlike [`UserInPtr::copy_user_string`], which allows `(nullptr, 0)` for syscalls with optional
/// names (such as `zx_resource_create`), task-creation syscalls require a non-null `name_ptr` even
/// when `name_len == 0`.
#[inline]
pub(super) fn copy_user_name(
    name_ptr: UserInPtr<u8>,
    name_len: usize,
    buf: &mut [MaybeUninit<u8>; ZX_MAX_NAME_LEN],
) -> Result<&[u8], Status> {
    if name_ptr.is_null() {
        return Err(Status::INVALID_ARGS);
    }
    name_ptr.copy_user_string(name_len, buf)
}

#[syscall]
pub fn sys_task_kill(handle: HandleValue) -> Result<(), Status> {
    ltracef!("handle {:#x}\n", handle.raw_value());

    let dispatcher = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<Dispatcher>(up, handle, ZX_RIGHT_DESTROY)
    })?;

    if let Some(job) = dispatcher.downcast::<JobDispatcher>() {
        job.kill(ZX_TASK_RETCODE_SYSCALL_KILL);
        Ok(())
    } else if let Some(process) = dispatcher.downcast::<ProcessDispatcher>() {
        process.kill(ZX_TASK_RETCODE_SYSCALL_KILL);
        Ok(())
    } else if dispatcher.downcast::<ThreadDispatcher>().is_some() {
        Err(Status::NOT_SUPPORTED)
    } else {
        Err(Status::WRONG_TYPE)
    }
}

#[syscall]
pub fn sys_task_suspend(handle: HandleValue, token: &mut HandleValue) -> Result<(), Status> {
    ltracef!("handle {:#x}\n", handle.raw_value());

    let task = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<Dispatcher>(up, handle, ZX_RIGHT_WRITE)
    })?;
    let (new_token, rights) = SuspendTokenDispatcher::create(task)?;
    *token = ProcessDispatcher::with_current(|up| up.make_and_add_handle(new_token, rights))?;
    Ok(())
}
