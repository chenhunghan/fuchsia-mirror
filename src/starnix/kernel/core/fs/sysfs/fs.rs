// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fs::sysfs::{build_kernel_directory, build_power_directory};
use crate::task::{CurrentTask, Kernel};
use crate::vfs::pseudo::simple_directory::SimpleDirectoryMutator;
use crate::vfs::pseudo::simple_file::BytesFile;
use crate::vfs::{
    CacheMode, DirEntry, FileSystem, FileSystemHandle, FileSystemOps, FileSystemOptions,
    FsNodeInfo, FsStr,
};
use starnix_logging::{Level, track_stub_log};
use starnix_types::vfs::default_statfs;
use starnix_uapi::auth::FsCred;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::{SYSFS_MAGIC, errno, statfs};

struct SysFs;
impl FileSystemOps for SysFs {
    fn statfs(&self, _fs: &FileSystem, _current_task: &CurrentTask) -> Result<statfs, Errno> {
        Ok(default_statfs(SYSFS_MAGIC))
    }
    fn name(&self) -> &'static FsStr {
        "sysfs".into()
    }
}

/// Reports lookups of missing `/sys` entries via `track_stub_log!`, to surface sysfs nodes that
/// userspace expects but which Starnix does not (yet) provide.
///
/// Each distinct missing path is logged once, and counted in the kernel's Inspect `stubs` node.
fn sysfs_not_found_handler(entry: &DirEntry, name: &FsStr) -> Errno {
    let message = format!("Looking for {name} in {entry:?}");
    track_stub_log!(Level::Warn, TODO("https://fxbug.dev/493488790"), &message);
    errno!(ENOENT, message)
}

fn add_devicetree_property(
    fs: &FileSystemHandle,
    dir: &SimpleDirectoryMutator,
    name: &str,
    value: Vec<u8>,
) {
    // Security-prefixed devicetree properties are root-only with zero reported size.
    let is_security = name.starts_with("security-");
    let file_mode = if is_security { mode!(IFREG, 0o400) } else { mode!(IFREG, 0o444) };
    let mut info = FsNodeInfo::new(file_mode, FsCred::root());
    if !is_security {
        info.size = value.len();
    }
    let node = fs.create_node_and_allocate_node_id(BytesFile::new_node(value), info);
    dir.node(name.into(), node);
}

fn populate_devicetree_node(
    fs: &FileSystemHandle,
    dir: &SimpleDirectoryMutator,
    node: &devicetree::types::Node,
) {
    let dir_mode = 0o755;
    // FDT v17+ omits explicit `name` properties, which sysfs synthesizes from the node name
    // without any `@unit-address` suffix.
    if node.get_property("name").is_none() {
        let base_name = node.name.split_once('@').map_or(node.name.as_str(), |(base, _)| base);
        let mut name_bytes = Vec::with_capacity(base_name.len() + 1);
        name_bytes.extend_from_slice(base_name.as_bytes());
        name_bytes.push(0);
        add_devicetree_property(fs, dir, "name", name_bytes);
    }
    for property in &node.properties {
        add_devicetree_property(fs, dir, &property.name, property.value.clone());
    }
    for child in &node.children {
        dir.subdir(&child.name, dir_mode, |dir| {
            populate_devicetree_node(fs, dir, child);
        });
    }
}

impl SysFs {
    fn new_fs(kernel: &Kernel, options: FileSystemOptions) -> FileSystemHandle {
        let fs =
            FileSystem::new(kernel, CacheMode::Cached(kernel.fs_cache_config()), SysFs, options)
                .expect("sysfs constructed with valid options");

        fn empty_dir(_: &SimpleDirectoryMutator) {}

        let registry = &kernel.device_registry;
        let root = &registry.objects.root;
        fs.create_root(fs.allocate_ino(), root.clone());
        if kernel.features.log_sysfs_lookup_misses {
            root.set_not_found_handler(sysfs_not_found_handler);
        }
        let dir = SimpleDirectoryMutator::new(fs.clone(), root.clone());

        let dir_mode = 0o755;
        dir.subdir("fs", dir_mode, |dir| {
            dir.subdir("selinux", dir_mode, empty_dir);
            dir.subdir("bpf", dir_mode, empty_dir);
            dir.subdir("cgroup", dir_mode, empty_dir);
            dir.subdir("fuse", dir_mode, |dir| {
                dir.subdir("connections", dir_mode, empty_dir);
            });
            dir.subdir("pstore", dir_mode, empty_dir);
        });

        dir.subdir("block", dir_mode, empty_dir);
        dir.subdir("bus", dir_mode, empty_dir);

        dir.subdir("dev", dir_mode, |dir| {
            dir.subdir("char", dir_mode, empty_dir);
            dir.subdir("block", dir_mode, empty_dir);
        });
        dir.subdir("firmware", dir_mode, |dir| {
            if let Some(device_tree) = &kernel.device_tree {
                dir.subdir("devicetree", dir_mode, |dir| {
                    dir.subdir("base", dir_mode, |dir| {
                        populate_devicetree_node(&fs, dir, &device_tree.root_node);
                    });
                });
            }
        });

        dir.subdir("kernel", dir_mode, |dir| {
            build_kernel_directory(kernel, dir);
        });

        dir.subdir("power", 0o755, |dir| {
            build_power_directory(kernel, dir);
        });

        dir.subdir("module", dir_mode, empty_dir);
        dir.subdir("devices", dir_mode, empty_dir);

        fs
    }
}

struct SysFsHandle(FileSystemHandle);

pub fn sys_fs(
    current_task: &CurrentTask,
    _options: FileSystemOptions,
) -> Result<FileSystemHandle, Errno> {
    Ok(get_sysfs(current_task.kernel()))
}

pub fn get_sysfs(kernel: &Kernel) -> FileSystemHandle {
    kernel
        .expando
        .get_or_init(|| SysFsHandle(SysFs::new_fs(kernel, FileSystemOptions::default())))
        .0
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::KernelFeatures;
    use crate::testing::{spawn_kernel_and_run, spawn_kernel_with_features_and_run};
    use crate::vfs::buffers::VecOutputBuffer;
    use crate::vfs::{LookupContext, MountInfo, NamespaceNode};
    use starnix_uapi::open_flags::OpenFlags;
    use std::sync::Arc;

    #[::fuchsia::test]
    async fn soc_device_lazily_registers_platform_and_soc_devices() {
        spawn_kernel_and_run(async |current_task| {
            let objects = &current_task.kernel().device_registry.objects;
            let root = &objects.root;

            assert!(root.lookup("devices/platform".into()).is_none());
            assert!(root.lookup("devices/platform/soc".into()).is_none());
            assert!(root.lookup("bus/platform/devices/soc".into()).is_none());

            let _soc = objects.soc_device();
            let soc_uevent = root
                .lookup("devices/platform/soc/uevent".into())
                .expect("soc uevent node should exist after soc_device()");
            // Calling soc_device() again should return the cached Device without replacing its
            // sysfs entries.
            let _soc = objects.soc_device();
            let soc_uevent_after = root
                .lookup("devices/platform/soc/uevent".into())
                .expect("soc uevent node should still exist");
            assert!(Arc::ptr_eq(&soc_uevent, &soc_uevent_after));

            assert!(root.lookup("devices/platform/uevent".into()).is_some());
            assert!(root.lookup("bus/platform/devices/soc".into()).is_some());

            // SoC identification device is only registered when the `soc` module is enabled.
            assert!(root.lookup("bus/soc/devices/soc0".into()).is_none());
            assert!(root.lookup("devices/soc0".into()).is_none());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn sysfs_populates_devicetree_nodes() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let root = &kernel.device_registry.objects.root;

            // Without a ZBI devicetree, `/sys/firmware` exists but `devicetree/base` is absent.
            assert!(root.lookup("firmware".into()).is_some());
            assert!(root.lookup("firmware/devicetree".into()).is_none());

            // Populating a synthetic devicetree node recursively creates directories and properties.
            let dt_node = devicetree::types::Node {
                name: String::new(),
                properties: vec![
                    devicetree::types::Property {
                        name: "model".into(),
                        value: b"test-board\0".to_vec(),
                    },
                    devicetree::types::Property {
                        name: "security-password".into(),
                        value: b"secret\0".to_vec(),
                    },
                ],
                children: vec![
                    devicetree::types::Node {
                        name: "chosen".into(),
                        properties: vec![devicetree::types::Property {
                            name: "bootargs".into(),
                            value: b"console=ttyS0\0".to_vec(),
                        }],
                        children: vec![],
                    },
                    devicetree::types::Node {
                        name: "memory@80000000".into(),
                        properties: vec![],
                        children: vec![],
                    },
                    devicetree::types::Node {
                        name: "custom-node".into(),
                        properties: vec![devicetree::types::Property {
                            name: "name".into(),
                            value: b"explicit\0".to_vec(),
                        }],
                        children: vec![],
                    },
                ],
            };
            let sysfs = get_sysfs(kernel);
            root.edit(&sysfs, |dir| {
                dir.subdir("firmware", 0o755, |dir| {
                    dir.subdir("devicetree", 0o755, |dir| {
                        dir.subdir("base", 0o755, |dir| {
                            populate_devicetree_node(&sysfs, dir, &dt_node);
                        });
                    });
                });
            });

            let sysfs_root = NamespaceNode::new_anonymous(sysfs.root().clone());
            let read_prop = |path: &str| -> (Vec<u8>, FsNodeInfo) {
                let node = current_task
                    .lookup_path(&mut LookupContext::default(), sysfs_root.clone(), path.into())
                    .unwrap_or_else(|e| panic!("lookup {path}: {e:?}"));
                let info = node.entry.node.info().clone();
                let file = node.open(current_task, OpenFlags::RDONLY).expect("open prop");
                let mut buf = VecOutputBuffer::new(64);
                let n = file.read(current_task, &mut buf).expect("read prop");
                let mut bytes: Vec<u8> = buf.into();
                bytes.truncate(n);
                (bytes, info)
            };

            let (root_name, root_name_info) = read_prop("firmware/devicetree/base/name");
            assert_eq!(root_name, b"\0");
            assert_eq!(root_name_info.size, 1);
            assert_eq!(root_name_info.mode, mode!(IFREG, 0o444));

            let (model, model_info) = read_prop("firmware/devicetree/base/model");
            assert_eq!(model, b"test-board\0");
            assert_eq!(model_info.size, 11);

            let (_secret, secret_info) = read_prop("firmware/devicetree/base/security-password");
            assert_eq!(secret_info.size, 0);
            assert_eq!(secret_info.mode, mode!(IFREG, 0o400));

            let (chosen_name, _) = read_prop("firmware/devicetree/base/chosen/name");
            assert_eq!(chosen_name, b"chosen\0");
            assert!(root.lookup("firmware/devicetree/base/chosen/bootargs".into()).is_some());

            let (memory_name, memory_name_info) =
                read_prop("firmware/devicetree/base/memory@80000000/name");
            assert_eq!(memory_name, b"memory\0");
            assert_eq!(memory_name_info.size, 7);

            let (explicit_name, explicit_name_info) =
                read_prop("firmware/devicetree/base/custom-node/name");
            assert_eq!(explicit_name, b"explicit\0");
            assert_eq!(explicit_name_info.size, 9);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_log_sysfs_lookup_misses() {
        for (log_sysfs_lookup_misses, name) in
            [(false, "missing_when_disabled"), (true, "missing_when_enabled")]
        {
            spawn_kernel_with_features_and_run(
                async move |current_task| {
                    let sysfs = get_sysfs(current_task.kernel());
                    let res = sysfs.root().component_lookup(
                        current_task,
                        &MountInfo::detached(),
                        name.into(),
                    );
                    assert_eq!(res.unwrap_err(), errno!(ENOENT));
                },
                KernelFeatures { log_sysfs_lookup_misses, ..Default::default() },
            )
            .await;

            let inspector =
                starnix_logging::track_stub_lazy_node_callback().await.expect("lazy node callback");
            let hierarchy = fuchsia_inspect::reader::read(&inspector).await.expect("read inspect");
            let expected =
                format!(r#"Looking for {name} in DirEntry {{ fs: "sysfs", path: "/" }}"#);
            assert_eq!(hierarchy.get_child(&expected).is_some(), log_sysfs_lookup_misses);
        }
    }
}
