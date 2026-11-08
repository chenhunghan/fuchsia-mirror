// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::{
    NonPrivateFsNode, PolicyCapSupport, build_permission_check, check_permission,
    current_task_state, fs_node_ensure_class, policycap_support, set_cached_sid, superblock,
};

use crate::task::CurrentTask;
use crate::vfs::FileHandle;
use selinux::{InitialSid, PolicyCap, SecurityPermission, SecurityServer};
use starnix_logging::{log_info, log_warn};
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;
use std::sync::atomic::Ordering;
use strum::VariantArray as _;

pub(in crate::security) fn selinuxfs_init_null(
    current_task: &CurrentTask,
    null_file_handle: &FileHandle,
) {
    // Apply the "devnull" initial SID to the node.
    let null_node = NonPrivateFsNode::new(null_file_handle.node())
        .expect("selinuxfs null file must not be kernel-private");
    fs_node_ensure_class(null_node).unwrap();
    set_cached_sid(null_node, InitialSid::Devnull.into());

    let kernel_state = current_task
        .kernel()
        .security_state
        .state
        .as_ref()
        .expect("selinux kernel state exists when selinux is enabled");

    kernel_state
        .selinuxfs_null
        .set(null_file_handle.clone())
        .expect("selinuxfs null file initialized at most once");
}

/// Called by the "selinuxfs" to load a binary SELinux policy into the `SecurityServer` and
/// complete policy-dependent kernel initialization.
pub(in crate::security) fn selinuxfs_load_policy(
    current_task: &CurrentTask,
    policy_data: Vec<u8>,
) -> Result<(), Errno> {
    let kernel_state = current_task
        .kernel()
        .security_state
        .state
        .as_ref()
        .expect("selinux kernel state exists when selinux is enabled");
    let security_server = &kernel_state.server;
    log_info!("Loading {} byte policy", policy_data.len());
    security_server.load_policy(policy_data).map_err(|error| errno!(EINVAL, error))?;

    // Compare the policy capabilities against this kernel's support level, and emit warnings for
    // each mismatch.
    for capability in PolicyCap::VARIANTS {
        let is_enabled = security_server.is_policycap_enabled(*capability);
        log_info!("SELinux:  policy capability {}={}", capability.name(), is_enabled as u8);
        match policycap_support(*capability) {
            PolicyCapSupport::AlwaysOn(bug) => {
                if !is_enabled {
                    log_warn!(
                        "SELinux:  policy capability {} cannot be disabled bug={bug}",
                        capability.name()
                    );
                }
            }
            PolicyCapSupport::AlwaysOff(bug) => {
                if is_enabled {
                    log_warn!(
                        "SELinux:  policy capability {} is not supported bug={bug}",
                        capability.name()
                    );
                }
            }
            PolicyCapSupport::Configurable | PolicyCapSupport::NotImplemented => (),
        }
    }

    // Invoke `file_system_resolve_security()` on all pre-existing `FileSystem`s.
    // No new `FileSystem`s should be added to `pending_file_systems` after policy load.
    let pending_file_systems = std::mem::take(&mut *kernel_state.pending_file_systems.lock());
    for file_system in pending_file_systems {
        if let Some(file_system) = file_system.0.upgrade() {
            superblock::file_system_resolve_security(security_server, current_task, &file_system)
                .unwrap_or_else(|e| {
                    panic!("Failed to resolve {} FileSystem label: {:?}", file_system.name(), e)
                });
        }
    }

    kernel_state.has_policy.store(true, Ordering::Release);
    Ok(())
}

/// Used by the "selinuxfs" module to perform checks on SELinux API file accesses.
pub(in crate::security) fn selinuxfs_check_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    permission: SecurityPermission,
) -> Result<(), Errno> {
    let source_sid = current_task_state(current_task).current_sid;
    let target_sid = InitialSid::Security.into();
    let permission_check = build_permission_check(current_task, security_server);
    check_permission(
        &permission_check,
        current_task,
        source_sid,
        target_sid,
        permission,
        current_task.into(),
    )
}

#[cfg(test)]
mod tests {
    use super::super::testing::HOOKS_TESTS_BINARY_POLICY;
    use crate::security;
    use crate::testing::spawn_kernel_with_selinux_and_run;
    use starnix_uapi::error;

    #[fuchsia::test]
    async fn selinuxfs_load_policy_rejects_invalid_policy() {
        spawn_kernel_with_selinux_and_run(async |current_task, security_server| {
            let kernel_state = current_task
                .kernel()
                .security_state
                .state
                .as_ref()
                .expect("selinux kernel state exists when selinux is enabled");
            let pending_file_systems_count = kernel_state.pending_file_systems.lock().len();
            assert_ne!(pending_file_systems_count, 0);
            let queued_fs = kernel_state
                .pending_file_systems
                .lock()
                .iter()
                .next()
                .and_then(|fs| fs.0.upgrade())
                .expect("queued file system");
            assert!(queued_fs.security_state.state.label().is_none());

            // A policy that the `SecurityServer` rejects leaves the kernel's SELinux state
            // unchanged.
            assert_eq!(
                security::selinuxfs_load_policy(current_task, b"not a policy".to_vec()),
                error!(EINVAL)
            );
            assert!(!security_server.has_policy());
            assert!(!kernel_state.has_policy());
            assert_eq!(kernel_state.pending_file_systems.lock().len(), pending_file_systems_count);
            assert!(queued_fs.security_state.state.label().is_none());

            // A valid policy can then still be loaded, resolving the pending file systems' labels.
            security::selinuxfs_load_policy(current_task, HOOKS_TESTS_BINARY_POLICY.to_vec())
                .expect("load policy");
            assert!(security_server.has_policy());
            assert!(kernel_state.has_policy());
            assert!(kernel_state.pending_file_systems.lock().is_empty());
            assert!(queued_fs.security_state.state.label().is_some());

            // A policy rejected on reload leaves the previously loaded policy active.
            assert_eq!(
                security::selinuxfs_load_policy(current_task, b"not a policy".to_vec()),
                error!(EINVAL)
            );
            assert!(kernel_state.has_policy());
            assert_eq!(security_server.binary_policy_size(), Some(HOOKS_TESTS_BINARY_POLICY.len()));
        })
        .await;
    }
}
