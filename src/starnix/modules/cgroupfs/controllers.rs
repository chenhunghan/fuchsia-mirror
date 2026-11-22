// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implementation of `cgroup.controllers` and `cgroup.subtree_control` files.
//!
//! `cgroup.controllers` is a read-only file listing the space-separated controllers available to a
//! cgroup. `cgroup.subtree_control` lists the controllers enabled for the cgroup's immediate
//! children and accepts space-separated `+controller` and `-controller` operations.
//!
//! Full details at https://docs.kernel.org/admin-guide/cgroup-v2.html#controlling-controllers

use starnix_core::task::{CgroupOps, ControllerType, CurrentTask};
use starnix_core::vfs::FsNodeOps;
use starnix_core::vfs::pseudo::simple_file::{BytesFile, BytesFileOps};
use starnix_uapi::errors::Errno;
use starnix_uapi::{errno, error};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::{Arc, Weak};

fn format_controllers(controllers: &BTreeSet<ControllerType>) -> Vec<u8> {
    let names: Vec<&str> = controllers.iter().filter_map(ControllerType::v2_name).collect();
    if names.is_empty() { Vec::new() } else { format!("{}\n", names.join(" ")).into_bytes() }
}

/// Read-only `cgroup.controllers` interface file reporting controllers available to the cgroup.
pub struct ControllersFile {
    cgroup: Weak<dyn CgroupOps>,
}

impl ControllersFile {
    pub fn new_node(cgroup: Weak<dyn CgroupOps>) -> impl FsNodeOps {
        BytesFile::new_node(Self { cgroup })
    }

    fn cgroup(&self) -> Result<Arc<dyn CgroupOps>, Errno> {
        self.cgroup.upgrade().ok_or_else(|| errno!(ENODEV))
    }
}

impl BytesFileOps for ControllersFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let cgroup = self.cgroup()?;
        let controllers = cgroup.get_available_controllers()?;
        Ok(format_controllers(&controllers).into())
    }
}

/// Read-write `cgroup.subtree_control` interface file managing controllers enabled for immediate
/// child cgroups.
pub struct SubtreeControlFile {
    cgroup: Weak<dyn CgroupOps>,
}

impl SubtreeControlFile {
    pub fn new_node(cgroup: Weak<dyn CgroupOps>) -> impl FsNodeOps {
        BytesFile::new_node(Self { cgroup })
    }

    fn cgroup(&self) -> Result<Arc<dyn CgroupOps>, Errno> {
        self.cgroup.upgrade().ok_or_else(|| errno!(ENODEV))
    }
}

impl BytesFileOps for SubtreeControlFile {
    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let input = std::str::from_utf8(&data).map_err(|_| errno!(EINVAL))?;
        let mut enable = BTreeSet::new();
        let mut disable = BTreeSet::new();

        // Per https://docs.kernel.org/admin-guide/cgroup-v2.html#controlling-controllers:
        // "Space-separated list of controllers prefixed with '+' or '-' can be written to enable
        // or disable controllers... If a controller appears more than once on the list, the last
        // one is effective. When multiple enable and disable operations are specified, either all
        // succeed or all fail."
        for token in input.split_ascii_whitespace() {
            if let Some(name) = token.strip_prefix('+') {
                let controller = ControllerType::from_v2_str(name).map_err(|_| errno!(EINVAL))?;
                disable.remove(&controller);
                enable.insert(controller);
            } else if let Some(name) = token.strip_prefix('-') {
                let controller = ControllerType::from_v2_str(name).map_err(|_| errno!(EINVAL))?;
                enable.remove(&controller);
                disable.insert(controller);
            } else {
                return error!(EINVAL);
            }
        }

        let cgroup = self.cgroup()?;
        cgroup.update_subtree_control(&enable, &disable)
    }

    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let cgroup = self.cgroup()?;
        let controllers = cgroup.get_subtree_control()?;
        Ok(format_controllers(&controllers).into())
    }
}
