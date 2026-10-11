// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::{
    ChannelDispatcher, Dispatcher, ExceptionDispatcher, Exceptionate, HandleValue, JobDispatcher,
    ProcessDispatcher, ThreadDispatcher,
};
use debug::ltracef;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{
    ZX_EXCEPTION_CHANNEL_DEBUGGER, ZX_POL_NEW_CHANNEL, ZX_RIGHT_DUPLICATE, ZX_RIGHT_ENUMERATE,
    ZX_RIGHT_INSPECT, ZX_RIGHT_MANAGE_THREAD, ZX_RIGHT_READ, ZX_RIGHT_TRANSFER, ZX_RIGHT_WAIT,
};

const LOCAL_TRACE: u32 = 0;

// zx_status_t zx_task_create_exception_channel
#[syscall]
pub fn sys_task_create_exception_channel(
    handle: HandleValue,
    options: u32,
    out: &mut HandleValue,
) -> Result<(), Status> {
    ltracef!("handle {:#x}, options {:#x}\n", handle.raw_value(), options);

    if (options & !ZX_EXCEPTION_CHANNEL_DEBUGGER) != 0 {
        return Err(Status::INVALID_ARGS);
    }

    let up = ProcessDispatcher::get_current();
    up.enforce_basic_policy(ZX_POL_NEW_CHANNEL)?;

    // Required rights to receive exceptions:
    //   INSPECT: provides non-trivial task information
    //   DUPLICATE: can create new thread and process handles
    //   TRANSFER: exceptions or their channels can be transferred
    //   MANAGE_THREAD: can keep thread paused during exception
    //   ENUMERATE (job/process): can access child thread (enforced below)
    //
    // In the future we may want to support some smarter behavior here e.g.
    // allowing for exceptions but no task handles if these rights don't exist,
    // but to start with we'll keep it simple until we know we want this.
    let (task, task_rights) =
        up.handle_table().get_dispatcher_with_rights_and_actual::<Dispatcher>(
            &up,
            handle,
            ZX_RIGHT_INSPECT | ZX_RIGHT_DUPLICATE | ZX_RIGHT_TRANSFER | ZX_RIGHT_MANAGE_THREAD,
        )?;

    // The task handles provided over this exception channel use the rights on
    // `handle` so we are sure not to grant any additional rights the caller
    // didn't already have.
    //
    // TODO(https://fxbug.dev/42108165): thread/process/job rights don't always map 1:1.
    let mut process_rights = task_rights;
    let thread_rights = task_rights;

    // Only one of `exceptionate` and `job_to_create_debug_exceptionate` will be `Some`.
    //
    // When creating debugger exception channel on a job, `exceptionate` will be `None` and
    // `job_to_create_debug_exceptionate` is used instead, because
    // `JobDispatcher::create_debug_exceptionate` requires dynamic allocation. For other types,
    // only `exceptionate` will be used.
    let mut exceptionate: Option<&Exceptionate> = None;
    let mut job_to_create_debug_exceptionate: Option<&JobDispatcher> = None;

    let mut job_or_process = false;

    // Use `downcast()` on the borrowed `task` reference to avoid moving the
    // `RefPtr` out, as we still need to retain the `RefPtr` to keep our extracted
    // `&Exceptionate` alive.
    if let Some(job) = task.downcast::<JobDispatcher>() {
        if (options & ZX_EXCEPTION_CHANNEL_DEBUGGER) != 0 {
            job_to_create_debug_exceptionate = Some(job);
        } else {
            exceptionate = Some(job.exceptionate());
        }
        job_or_process = true;
    } else if let Some(process) = task.downcast::<ProcessDispatcher>() {
        if (options & ZX_EXCEPTION_CHANNEL_DEBUGGER) != 0 {
            exceptionate = Some(process.debug_exceptionate());
        } else {
            exceptionate = Some(process.exceptionate());
        }
        job_or_process = true;
    } else if let Some(thread) = task.downcast::<ThreadDispatcher>() {
        if (options & ZX_EXCEPTION_CHANNEL_DEBUGGER) != 0 {
            return Err(Status::INVALID_ARGS);
        }

        // We don't provide access up the task chain, so don't send the process
        // handle when we're registering on a thread.
        process_rights = 0;
        exceptionate = Some(thread.exceptionate());
    } else {
        return Err(Status::WRONG_TYPE);
    }

    // For job and process handlers, we require the handle be able to enumerate
    // as proof that the caller is allowed to get to the thread handle.
    if job_or_process && (task_rights & ZX_RIGHT_ENUMERATE) == 0 {
        return Err(Status::ACCESS_DENIED);
    }

    let (kernel_handle, user_handle, rights) = ChannelDispatcher::create()?;

    if let Some(job) = job_to_create_debug_exceptionate {
        job.create_debug_exceptionate(kernel_handle, thread_rights, process_rights)?;
    } else {
        exceptionate.unwrap().set_channel(kernel_handle, thread_rights, process_rights)?;
    }

    // Strip unwanted rights from the user endpoint, exception channels are
    // read-only from userspace.
    //
    // We don't need to remove the task channel if this fails. Exception
    // channels are built to handle the userspace peer closing so it will just
    // follow that path if we fail to copy the userspace endpoint out.
    *out = up.make_and_add_handle(
        user_handle,
        rights & (ZX_RIGHT_TRANSFER | ZX_RIGHT_WAIT | ZX_RIGHT_READ),
    )?;
    Ok(())
}

// zx_status_t zx_exception_get_thread
#[syscall]
pub fn sys_exception_get_thread(
    handle: HandleValue,
    thread: &mut HandleValue,
) -> Result<(), Status> {
    let up = ProcessDispatcher::get_current();
    let exception = up.handle_table().get_dispatcher::<ExceptionDispatcher>(&up, handle)?;
    let thread_handle = exception.make_thread_handle()?;
    *thread = up.handle_table().map_handle_to_value(thread_handle.as_ref());
    up.handle_table().add_handle(thread_handle);
    Ok(())
}

// zx_status_t zx_exception_get_process
#[syscall]
pub fn sys_exception_get_process(
    handle: HandleValue,
    process: &mut HandleValue,
) -> Result<(), Status> {
    let up = ProcessDispatcher::get_current();
    let exception = up.handle_table().get_dispatcher::<ExceptionDispatcher>(&up, handle)?;
    let process_handle = exception.make_process_handle()?;
    *process = up.handle_table().map_handle_to_value(process_handle.as_ref());
    up.handle_table().add_handle(process_handle);
    Ok(())
}
