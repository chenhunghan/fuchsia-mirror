// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::TargetEvent;
use crate::error::Error;
use crate::events::TargetHandle;
use crate::instance_watcher::{InstanceSource, InstanceWatcher};
use emulator_instance::EmulatorInstances;
use futures::channel::mpsc::UnboundedSender;
use std::path::{Path, PathBuf};

/// Reads emulator instances from the watched instance root, falling back to `backup_roots` for
/// instances created by older versions of ffx.
///
/// Only the instance root is watched for changes. Instances in `backup_roots` are reported when
/// the watcher starts, but later additions or removals there don't generate events. The backup
/// roots only hold legacy instances, which current versions of ffx never create.
struct EmulatorSource {
    backup_roots: Vec<PathBuf>,
}

impl InstanceSource for EmulatorSource {
    fn recursive(&self) -> bool {
        true
    }

    fn get_all_targets(&self, root: &Path) -> Vec<(String, TargetHandle)> {
        let roots = std::iter::once(root.to_path_buf()).chain(self.backup_roots.iter().cloned());
        let emu_instances = EmulatorInstances::from(roots.collect::<Vec<_>>());
        emulator_instance::get_all_targets(&emu_instances)
            .unwrap_or_default()
            .into_iter()
            .map(|t| {
                let name = t.nodename.clone();
                let handle = TargetHandle::from(t);
                (name, handle)
            })
            .collect()
    }

    fn instance_id_from_path(&self, root: &Path, path: &Path) -> Option<String> {
        emulator_instance::instance_name_from_path(root, path)
    }

    fn read_target_handle(&self, root: &Path, path: &Path) -> Option<TargetHandle> {
        let name = self.instance_id_from_path(root, path)?;
        let emu_instances = EmulatorInstances::new(root.to_path_buf());
        let info = emulator_instance::get_target(&emu_instances, &name).ok()??;
        Some(TargetHandle::from(info))
    }
}

pub struct EmulatorWatcher {
    _watcher: InstanceWatcher,
}

impl EmulatorWatcher {
    pub fn new(
        instance_roots: Vec<PathBuf>,
        sender: UnboundedSender<TargetEvent>,
    ) -> Result<Self, Error> {
        let mut roots = instance_roots.into_iter();
        let instance_root = roots.next().unwrap_or_default();
        let watcher = InstanceWatcher::new(
            instance_root,
            sender,
            EmulatorSource { backup_roots: roots.collect() },
            |path, err| Error::EmulatorWatcher { path, err },
        )?;
        Ok(Self { _watcher: watcher })
    }
}
