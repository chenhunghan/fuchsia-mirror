// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::file::file_receive;
use super::{build_permission_check, check_permission, current_task_state};
use crate::task::CurrentTask;
use crate::vfs::FileObject;
use selinux::{BinderPermission, SecurityServer};
use starnix_uapi::auth::Credentials;
use starnix_uapi::errors::Errno;

/// Returns the serialized, NUL-terminated Security Context associated with the given credentials.
pub(in crate::security) fn binder_get_context(
    security_server: &SecurityServer,
    source_creds: &Credentials,
) -> Vec<u8> {
    // `selinux_hooks` are only invoked once a policy is loaded, at which point every SID resolves
    // to a Security Context (falling back to the `unlabeled` context if invalidated).
    security_server
        .sid_to_security_context_with_nul(source_creds.security_state.current_sid)
        .expect("SELinux policy is loaded")
}

/// Checks whether the given `current_task` can become the binder context manager.
pub(in crate::security) fn binder_set_context_mgr(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    context_mgr_creds: &Credentials,
) -> Result<(), Errno> {
    let audit_context = current_task.into();
    let current_sid = current_task_state(current_task).current_sid;
    let context_mgr_sid = context_mgr_creds.security_state.current_sid;
    check_permission(
        &build_permission_check(current_task, security_server),
        current_task,
        current_sid,
        context_mgr_sid,
        BinderPermission::SetContextMgr,
        audit_context,
    )
}

/// Checks whether the given `current_task` has permission to send a binder transaction
/// from a process with `source_creds` to a process with `target_creds`.
pub(in crate::security) fn binder_transaction(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    source_creds: &Credentials,
    target_creds: &Credentials,
) -> Result<(), Errno> {
    let audit_context = current_task.into();
    let current_sid = current_task_state(current_task).current_sid;
    let source_sid = source_creds.security_state.current_sid;
    let target_sid = target_creds.security_state.current_sid;
    if current_sid != source_sid {
        check_permission(
            &build_permission_check(current_task, security_server),
            current_task,
            current_sid,
            source_sid,
            BinderPermission::Impersonate,
            audit_context,
        )?;
    }
    check_permission(
        &build_permission_check(current_task, security_server),
        current_task,
        source_sid,
        target_sid,
        BinderPermission::Call,
        audit_context,
    )?;
    Ok(())
}

/// Checks whether the given `current_task` has permission to transfer Binder objects
/// from a process with `source_creds` to a process with `target_creds`.
pub(in crate::security) fn binder_transfer_binder(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    source_creds: &Credentials,
    target_creds: &Credentials,
) -> Result<(), Errno> {
    let audit_context = current_task.into();
    let source_sid = source_creds.security_state.current_sid;
    let target_sid = target_creds.security_state.current_sid;
    check_permission(
        &build_permission_check(current_task, security_server),
        current_task,
        source_sid,
        target_sid,
        BinderPermission::Transfer,
        audit_context,
    )
}

/// Checks whether the target process with `target_creds` has permission to receive `file` in a Binder transaction.
pub(in crate::security) fn binder_transfer_file(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    target_creds: &Credentials,
    file: &FileObject,
) -> Result<(), Errno> {
    let receiving_sid = target_creds.security_state.current_sid;
    file_receive(security_server, current_task, receiving_sid, file)
}
