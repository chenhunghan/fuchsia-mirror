// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::device::DeviceMode;
use crate::device::kobject::{Bus, Class, Device, DeviceMetadata, Subsystem};
use crate::fs::sysfs::{build_device_directory, get_sysfs, register_cpu_devices};
use crate::task::Kernel;
use crate::vfs::pseudo::simple_directory::{SimpleDirectory, SimpleDirectoryMutator};
use crate::vfs::{FileSystemHandle, FsStr, FsString};
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;
use std::sync::{Arc, OnceLock};

/// Owner of all the KObjects in sysfs.
///
/// Holds strong references to the KObjects that are visible in sysfs. These
/// objects are organized into hierarchies that make it easier to implement sysfs.
pub struct KObjectStore {
    /// Root of the sysfs hierarchy.
    pub root: Arc<SimpleDirectory>,

    /// Sysfs filesystem in which the KObjects are stored.
    fs: OnceLock<FileSystemHandle>,

    /// Root `/sys/devices/virtual` device.
    virtual_device: OnceLock<Device>,

    /// Root `/sys/devices/platform` device.
    platform_device: OnceLock<Device>,

    /// Root `/sys/devices/system` device.
    system_device: OnceLock<Device>,

    /// Root `/sys/devices/platform/soc` bus device.
    soc_device: OnceLock<Device>,
}

impl KObjectStore {
    pub fn init(&self, kernel: &Kernel) {
        self.fs.set(get_sysfs(kernel)).unwrap();
        self.register_initial_devices(kernel);
    }

    fn register_initial_devices(&self, kernel: &Kernel) {
        // Generic Linux device and class registrations.
        register_cpu_devices(kernel);
        let _ = self.bdi_class();
        let _ = self.net_class();
        let _ = self.power_supply_class();
    }

    pub(super) fn fs(&self) -> &FileSystemHandle {
        self.fs.get().expect("sysfs should be initialized")
    }

    /// Device bus used for platform devices (`/sys/bus/platform`).
    pub fn platform_bus(&self) -> Bus {
        self.get_or_create_bus("platform".into())
    }

    /// Root device used for virtual devices (`/sys/devices/virtual`).
    pub fn virtual_device(&self) -> Device {
        self.virtual_device
            .get_or_init(|| {
                self.create_device(
                    "virtual".into(),
                    /* parent = */ None,
                    /* subsystem = */ None,
                    /* metadata = */ None,
                    |_, _| {},
                )
                .expect("can create virtual root device")
            })
            .clone()
    }

    /// Root device used for platform devices (`/sys/devices/platform`).
    pub fn platform_device(&self) -> Device {
        self.platform_device
            .get_or_init(|| {
                self.create_device(
                    "platform".into(),
                    /* parent = */ None,
                    /* subsystem = */ None,
                    /* metadata = */ None,
                    build_device_directory,
                )
                .expect("can create platform root device")
            })
            .clone()
    }

    /// Root device used for system devices (`/sys/devices/system`).
    ///
    /// Subsystems such as `cpu` place their root device beneath this device.
    pub fn system_device(&self) -> Device {
        self.system_device
            .get_or_init(|| {
                self.create_device(
                    "system".into(),
                    /* parent = */ None,
                    /* subsystem = */ None,
                    /* metadata = */ None,
                    |_, _| {},
                )
                .expect("can create system root device")
            })
            .clone()
    }

    /// Bus device used for SoC peripheral devices (`/sys/devices/platform/soc`).
    pub fn soc_device(&self) -> Device {
        // TODO(https://fxbug.dev/452096300): Replace hardcoded SoC bus device with dynamic board
        // configuration.
        self.soc_device
            .get_or_init(|| {
                self.create_device(
                    "soc".into(),
                    Some(self.platform_device()),
                    Some(self.platform_bus().into()),
                    /* metadata = */ None,
                    build_device_directory,
                )
                .expect("can create soc device")
            })
            .clone()
    }

    /// Device class used for backing device info (`bdi`) devices (`/sys/class/bdi`).
    pub fn bdi_class(&self) -> Class {
        self.get_or_create_class("bdi".into())
    }

    /// Device class used for block devices.
    pub fn block_class(&self) -> Class {
        self.get_or_create_class("block".into())
    }

    /// Device class used for thermal devices.
    pub fn thermal_class(&self) -> Class {
        self.get_or_create_class("thermal".into())
    }

    /// Device class used for powercap devices.
    pub fn powercap_class(&self) -> Class {
        self.get_or_create_class("powercap".into())
    }

    /// Device class used for power-supply devices (`/sys/class/power_supply`).
    pub fn power_supply_class(&self) -> Class {
        self.get_or_create_class("power_supply".into())
    }

    /// Device class used for graphics devices.
    pub fn graphics_class(&self) -> Class {
        self.get_or_create_class("graphics".into())
    }

    /// Device class used for input devices.
    pub fn input_class(&self) -> Class {
        self.get_or_create_class("input".into())
    }

    /// Device class used for mem devices.
    pub fn mem_class(&self) -> Class {
        self.get_or_create_class("mem".into())
    }

    /// Device class used for net devices.
    pub fn net_class(&self) -> Class {
        self.get_or_create_class("net".into())
    }

    /// Device class used for misc devices.
    pub fn misc_class(&self) -> Class {
        self.get_or_create_class("misc".into())
    }

    /// Device class used for tty devices.
    pub fn tty_class(&self) -> Class {
        self.get_or_create_class("tty".into())
    }

    /// Device class used for dma_heap devices.
    pub fn dma_heap_class(&self) -> Class {
        self.get_or_create_class("dma_heap".into())
    }

    /// Incorrect device class.
    ///
    /// This class exposes the name "starnix" to userspace, which is incorrect. Instead, devices
    /// should use a class that represents their usage rather than their implementation.
    ///
    /// This class exists because a number of devices incorrectly use this class. We should fix
    /// those devices to report their proper class.
    pub fn starnix_class(&self) -> Class {
        self.get_or_create_class("starnix".into())
    }

    /// Real-time clock class.
    ///
    /// Becomes `/sys/class/rtc/...`.
    pub fn rtc_class(&self) -> Class {
        self.get_or_create_class("rtc".into())
    }

    fn ensure_dir(&self, path: &[&FsStr]) -> Arc<SimpleDirectory> {
        let fs = self.fs();
        let mut dir = self.root.clone();
        for component in path {
            dir = dir.subdir(fs, component, 0o755);
        }
        dir
    }

    fn lookup_dir(&self, path: &[&FsStr]) -> Option<Arc<SimpleDirectory>> {
        let mut dir = self.root.clone();
        for component in path {
            dir = dir.get_dir(component)?;
        }
        Some(dir)
    }

    /// Get a bus by name.
    ///
    /// If the bus does not exist, this function will create it.
    pub fn get_or_create_bus(&self, name: &FsStr) -> Bus {
        let name = name.to_owned();
        let devices = self.ensure_dir(&[b"bus".into(), name.as_ref(), b"devices".into()]);
        Bus::new(name, devices)
    }

    /// Get a class by name.
    ///
    /// If the class does not exist, this function will create it.
    pub fn get_or_create_class(&self, name: &FsStr) -> Class {
        let name = name.to_owned();
        let devices = self.ensure_dir(&[b"class".into(), name.as_ref()]);
        Class::new(name, devices)
    }

    pub fn class_with_dir(
        &self,
        name: &FsStr,
        build_directory: impl FnOnce(&SimpleDirectoryMutator),
    ) -> Class {
        let class = self.get_or_create_class(name);
        class.devices().edit(self.fs(), build_directory);
        class
    }

    /// Creates a device in `sysfs` and links it into its subsystem and `/sys/dev` views.
    ///
    /// Returns `EEXIST` if the device directory or any of its subsystem, `/sys/block`, or
    /// `/sys/dev` symlinks already exists, or `ENOENT` if `parent` has already been removed from
    /// `sysfs`.
    ///
    /// Rather than calling this method directly, add devices through
    /// [`DeviceRegistry`](crate::device::DeviceRegistry) so device numbers and uevent notifications
    /// are coordinated with `sysfs` creation.
    pub(super) fn create_device(
        &self,
        name: &FsStr,
        parent: Option<Device>,
        subsystem: Option<Subsystem>,
        metadata: Option<DeviceMetadata>,
        build_directory: impl FnOnce(&Device, &SimpleDirectoryMutator),
    ) -> Result<Device, Errno> {
        let parent = match (parent, &subsystem) {
            (None, Some(Subsystem::Class(_))) => Some(self.virtual_device()),
            (parent, _) => parent,
        };
        let parent_dir = match parent.as_ref() {
            Some(parent) => parent.dir().ok_or_else(|| errno!(ENOENT))?,
            None => self.ensure_dir(&[b"devices".into()]),
        };
        let dir = if let Some(glue_dir) =
            Device::glue_dir_name_for(parent.as_ref(), subsystem.as_ref())
        {
            parent_dir.create_nested_subdir(self.fs(), glue_dir, 0o755, name, 0o755)?
        } else {
            parent_dir.create_subdir(self.fs(), name, 0o755)?
        };
        let device = Device::new(name.to_owned(), parent, subsystem, metadata, dir.clone());
        let mut rollback =
            scopeguard::guard(Vec::<(Arc<SimpleDirectory>, FsString)>::new(), |links| {
                for (link_dir, link_name) in links.into_iter().rev() {
                    link_dir.remove(link_name.as_ref());
                }
                self.remove_device_dir(&device);
            });
        dir.edit(self.fs(), |mutator| {
            if let Some(target) = device.subsystem_symlink_target() {
                mutator.symlink("subsystem".into(), target.as_ref());
            }
            build_directory(&device, mutator);
        });
        for (link_dir, link_name, target) in
            self.device_symlinks(&device, /* create_dirs = */ true)
        {
            link_dir.create_symlink(self.fs(), link_name.as_ref(), target.as_ref())?;
            rollback.push((link_dir, link_name));
        }
        scopeguard::ScopeGuard::into_inner(rollback);
        Ok(device)
    }

    fn symlink_dir(&self, path: &[&FsStr], create: bool) -> Option<Arc<SimpleDirectory>> {
        if create { Some(self.ensure_dir(path)) } else { self.lookup_dir(path) }
    }

    /// Returns `(link_dir, link_name, target)` entries for `device` in the order they should be
    /// created (and reverse of the order they should be removed).
    fn device_symlinks(
        &self,
        device: &Device,
        create_dirs: bool,
    ) -> Vec<(Arc<SimpleDirectory>, FsString, FsString)> {
        let mut links = Vec::with_capacity(3);
        let name = device.name().to_owned();
        match device.subsystem() {
            Some(Subsystem::Bus(bus)) => {
                links.push((bus.devices().clone(), name.clone(), device.path_from_depth(3)));
            }
            Some(Subsystem::Class(class)) => {
                links.push((class.devices().clone(), name.clone(), device.path_from_depth(2)));
            }
            None => {}
        }
        if let Some(metadata) = device.metadata() {
            let dev_subdir: &FsStr = match metadata.mode {
                DeviceMode::Block => {
                    if let Some(dir) = self.symlink_dir(&[b"block".into()], create_dirs) {
                        links.push((dir, name, device.path_from_depth(1)));
                    }
                    b"block".into()
                }
                DeviceMode::Char => b"char".into(),
            };
            if let Some(dir) = self.symlink_dir(&[b"dev".into(), dev_subdir], create_dirs) {
                links.push((dir, metadata.devt.to_string().into(), device.path_from_depth(2)));
            }
        }
        links
    }

    fn remove_device_dir(&self, device: &Device) {
        let name = device.name();
        let parent_dir = match device.parent() {
            Some(parent) => parent.dir(),
            None => self.lookup_dir(&[b"devices".into()]),
        };
        let Some(parent_dir) = parent_dir else {
            return;
        };
        if let Some(glue_dir) = device.glue_dir_name() {
            parent_dir.remove_from_subdir_if_empty(glue_dir, name);
        } else {
            parent_dir.remove(name);
        }
    }

    /// Destroy a device.
    ///
    /// This function removes the KObject for the device from the store.
    ///
    /// Most clients hold weak references to KObjects, which means those references will become
    /// invalid shortly after this function is called.
    pub(super) fn remove(&self, device: &Device) {
        for (link_dir, link_name, _) in
            self.device_symlinks(device, /* create_dirs = */ false).into_iter().rev()
        {
            link_dir.remove(link_name.as_ref());
        }
        self.remove_device_dir(device);
    }
}

impl Default for KObjectStore {
    fn default() -> Self {
        Self {
            root: SimpleDirectory::new(),
            fs: OnceLock::new(),
            virtual_device: OnceLock::new(),
            platform_device: OnceLock::new(),
            system_device: OnceLock::new(),
            soc_device: OnceLock::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::spawn_kernel_and_run;
    use crate::vfs::{LookupContext, NamespaceNode, ResolveBase, SymlinkTarget};
    use starnix_uapi::device_id::DeviceId;

    fn create_test_device(
        store: &KObjectStore,
        name: &str,
        parent: Option<Device>,
        subsystem: Option<Subsystem>,
        metadata: Option<DeviceMetadata>,
    ) -> Result<Device, Errno> {
        store.create_device(name.into(), parent, subsystem, metadata, build_device_directory)
    }

    fn test_metadata(name: &str, major: u32, minor: u32, mode: DeviceMode) -> DeviceMetadata {
        DeviceMetadata::new(name.into(), DeviceId::new(major, minor), mode)
    }

    #[::fuchsia::test]
    async fn create_device_rejects_duplicate_device_dir() {
        spawn_kernel_and_run(async |current_task| {
            let store = &current_task.kernel().device_registry.objects;

            create_test_device(store, "root_dev", None, None, None).expect("create root_dev");
            assert_eq!(
                create_test_device(store, "root_dev", None, None, None).unwrap_err(),
                errno!(EEXIST)
            );
            assert!(store.root.lookup("devices/root_dev/uevent".into()).is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn create_device_rolls_back_on_subsystem_symlink_conflict() {
        spawn_kernel_and_run(async |current_task| {
            let store = &current_task.kernel().device_registry.objects;
            let root = &store.root;
            let root_dev =
                create_test_device(store, "root_dev", None, None, None).expect("create root_dev");

            let bus = store.get_or_create_bus("test_bus".into());
            create_test_device(store, "bus_dev", None, Some(bus.clone().into()), None)
                .expect("create bus_dev");
            assert_eq!(
                create_test_device(
                    store,
                    "bus_dev",
                    Some(root_dev.clone()),
                    Some(bus.into()),
                    None
                )
                .unwrap_err(),
                errno!(EEXIST)
            );
            assert!(root.lookup("devices/root_dev/bus_dev".into()).is_none());
            assert!(root.lookup("devices/bus_dev/uevent".into()).is_some());
            assert!(root.lookup("bus/test_bus/devices/bus_dev".into()).is_some());

            // Class symlink collision when parent has no other children in `<class>` prunes the
            // empty `<class>` glue directory under `root_dev`.
            let class = store.get_or_create_class("test_class".into());
            create_test_device(store, "class_dev", None, Some(class.clone().into()), None)
                .expect("create class_dev");
            assert_eq!(
                create_test_device(
                    store,
                    "class_dev",
                    Some(root_dev.clone()),
                    Some(class.clone().into()),
                    None,
                )
                .unwrap_err(),
                errno!(EEXIST)
            );
            assert!(root.lookup("devices/root_dev/test_class".into()).is_none());
            assert!(root.lookup("devices/virtual/test_class/class_dev/uevent".into()).is_some());
            assert!(root.lookup("class/test_class/class_dev".into()).is_some());

            // Class symlink collision when `<class>` glue directory already holds another child
            // preserves that sibling while rolling back the failed entry.
            create_test_device(
                store,
                "sibling_dev",
                Some(root_dev.clone()),
                Some(class.clone().into()),
                None,
            )
            .expect("create sibling_dev");
            assert_eq!(
                create_test_device(store, "class_dev", Some(root_dev), Some(class.into()), None)
                    .unwrap_err(),
                errno!(EEXIST)
            );
            assert!(root.lookup("devices/root_dev/test_class/class_dev".into()).is_none());
            assert!(root.lookup("devices/root_dev/test_class/sibling_dev/uevent".into()).is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn create_device_rolls_back_on_block_symlink_conflict() {
        spawn_kernel_and_run(async |current_task| {
            let store = &current_task.kernel().device_registry.objects;
            let root = &store.root;
            let root_dev =
                create_test_device(store, "root_dev", None, None, None).expect("create root_dev");
            let bus = store.get_or_create_bus("test_bus".into());

            create_test_device(
                store,
                "blk_collide",
                None,
                Some(store.block_class().into()),
                Some(test_metadata("blk_collide", 250, 0, DeviceMode::Block)),
            )
            .expect("create blk1");
            assert_eq!(
                create_test_device(
                    store,
                    "blk_collide",
                    Some(root_dev),
                    Some(bus.into()),
                    Some(test_metadata("blk_collide", 250, 1, DeviceMode::Block)),
                )
                .unwrap_err(),
                errno!(EEXIST)
            );
            assert!(root.lookup("devices/root_dev/blk_collide".into()).is_none());
            assert!(root.lookup("bus/test_bus/devices/blk_collide".into()).is_none());
            assert!(root.lookup("dev/block/250:1".into()).is_none());
            assert!(root.lookup("block/blk_collide".into()).is_some());
            assert!(root.lookup("class/block/blk_collide".into()).is_some());
            assert!(root.lookup("dev/block/250:0".into()).is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn create_device_rolls_back_on_dev_symlink_conflict() {
        spawn_kernel_and_run(async |current_task| {
            let store = &current_task.kernel().device_registry.objects;
            let root = &store.root;
            let root_dev =
                create_test_device(store, "root_dev", None, None, None).expect("create root_dev");
            let class = store.get_or_create_class("test_class".into());

            create_test_device(
                store,
                "chr1",
                None,
                Some(class.clone().into()),
                Some(test_metadata("chr1", 251, 0, DeviceMode::Char)),
            )
            .expect("create chr1");
            assert_eq!(
                create_test_device(
                    store,
                    "chr2",
                    Some(root_dev.clone()),
                    Some(class.clone().into()),
                    Some(test_metadata("chr2", 251, 0, DeviceMode::Char)),
                )
                .unwrap_err(),
                errno!(EEXIST)
            );
            assert!(root.lookup("devices/root_dev/test_class/chr2".into()).is_none());
            assert!(root.lookup("class/test_class/chr2".into()).is_none());
            assert!(root.lookup("dev/char/251:0".into()).is_some());

            create_test_device(
                store,
                "blk1",
                None,
                Some(store.block_class().into()),
                Some(test_metadata("blk1", 250, 0, DeviceMode::Block)),
            )
            .expect("create blk1");
            assert_eq!(
                create_test_device(
                    store,
                    "blk_dup_devt",
                    Some(root_dev),
                    Some(store.block_class().into()),
                    Some(test_metadata("blk_dup_devt", 250, 0, DeviceMode::Block)),
                )
                .unwrap_err(),
                errno!(EEXIST)
            );
            assert!(root.lookup("devices/root_dev/block".into()).is_none());
            assert!(root.lookup("class/block/blk_dup_devt".into()).is_none());
            assert!(root.lookup("block/blk_dup_devt".into()).is_none());
            assert!(root.lookup("dev/block/250:0".into()).is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn create_device_rejects_removed_parent_even_when_cached() {
        spawn_kernel_and_run(async |current_task| {
            let store = &current_task.kernel().device_registry.objects;
            let root = &store.root;
            let root_dev =
                create_test_device(store, "root_dev", None, None, None).expect("create root_dev");
            let class = store.get_or_create_class("test_class".into());
            let sibling_dev = create_test_device(
                store,
                "sibling_dev",
                Some(root_dev.clone()),
                Some(class.clone().into()),
                None,
            )
            .expect("create sibling_dev");

            // Populate VFS `DirEntry` caches for `root_dev` and `sibling_dev` without flushing
            // `DelayedReleaser` after removal.
            let sysfs_root = NamespaceNode::new_anonymous(store.fs().root().clone());
            let mut context = LookupContext::default();
            let _cached_root_dev = current_task
                .lookup_path(&mut context, sysfs_root.clone(), "devices/root_dev".into())
                .expect("lookup devices/root_dev in sysfs VFS");
            let mut context = LookupContext::default();
            let _cached_sibling_dev = current_task
                .lookup_path(
                    &mut context,
                    sysfs_root,
                    "devices/root_dev/test_class/sibling_dev".into(),
                )
                .expect("lookup sibling_dev in sysfs VFS");

            store.remove(&sibling_dev);
            store.remove(&root_dev);
            assert!(root_dev.raw_dir().is_some());
            assert!(root_dev.dir().is_none());
            assert!(sibling_dev.raw_dir().is_some());
            assert!(sibling_dev.dir().is_none());

            assert_eq!(
                create_test_device(store, "orphan_direct", Some(root_dev.clone()), None, None)
                    .unwrap_err(),
                errno!(ENOENT)
            );
            assert_eq!(
                create_test_device(
                    store,
                    "orphan_class",
                    Some(root_dev),
                    Some(class.clone().into()),
                    None,
                )
                .unwrap_err(),
                errno!(ENOENT)
            );
            assert_eq!(
                create_test_device(
                    store,
                    "orphan_under_class",
                    Some(sibling_dev),
                    Some(class.into()),
                    None,
                )
                .unwrap_err(),
                errno!(ENOENT)
            );
            assert!(root.lookup("class/test_class/orphan_class".into()).is_none());
            assert!(root.lookup("class/test_class/orphan_under_class".into()).is_none());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn create_device_populates_subsystem_symlinks() {
        spawn_kernel_and_run(async |current_task| {
            let store = &current_task.kernel().device_registry.objects;
            let root = &store.root;
            let bus = store.get_or_create_bus("test_bus".into());
            let class = store.get_or_create_class("test_class".into());

            let root_dev =
                create_test_device(store, "root_dev", None, None, None).expect("create root_dev");
            assert!(root.lookup("devices/root_dev/subsystem".into()).is_none());

            create_test_device(store, "root_bus_dev", None, Some(bus.clone().into()), None)
                .expect("create root_bus_dev");
            let nested_bus_dev =
                create_test_device(store, "nested_bus_dev", Some(root_dev), Some(bus.into()), None)
                    .expect("create nested_bus_dev");
            create_test_device(store, "virt_class_dev", None, Some(class.clone().into()), None)
                .expect("create virt_class_dev");
            let nested_class_dev = create_test_device(
                store,
                "nested_class_dev",
                Some(nested_bus_dev),
                Some(class.clone().into()),
                None,
            )
            .expect("create nested_class_dev");
            create_test_device(
                store,
                "class_child_dev",
                Some(nested_class_dev),
                Some(class.into()),
                None,
            )
            .expect("create class_child_dev");

            let sysfs_root = NamespaceNode::new_anonymous(store.fs().root().clone());
            let lookup = |path: &str| {
                let mut context = LookupContext {
                    resolve_base: ResolveBase::Beneath(sysfs_root.clone()),
                    ..LookupContext::default()
                };
                current_task
                    .lookup_path(&mut context, sysfs_root.clone(), path.into())
                    .unwrap_or_else(|e| panic!("failed to lookup {path}: {e:?}"))
            };
            let bus_dir = lookup("bus/test_bus");
            let class_dir = lookup("class/test_class");

            for (symlink_path, expected_readlink, expected_target) in [
                ("devices/root_bus_dev/subsystem", "../../bus/test_bus", &bus_dir),
                ("devices/root_dev/nested_bus_dev/subsystem", "../../../bus/test_bus", &bus_dir),
                (
                    "devices/virtual/test_class/virt_class_dev/subsystem",
                    "../../../../class/test_class",
                    &class_dir,
                ),
                (
                    "devices/root_dev/nested_bus_dev/test_class/nested_class_dev/subsystem",
                    "../../../../../class/test_class",
                    &class_dir,
                ),
                (
                    "devices/root_dev/nested_bus_dev/test_class/nested_class_dev/class_child_dev/subsystem",
                    "../../../../../../class/test_class",
                    &class_dir,
                ),
            ] {
                let link_node = root
                    .lookup(symlink_path.into())
                    .unwrap_or_else(|| panic!("missing symlink {symlink_path}"));
                match link_node.readlink(current_task).expect("readlink") {
                    SymlinkTarget::Path(target) => {
                        assert_eq!(
                            target, expected_readlink,
                            "unexpected readlink target for {symlink_path}"
                        );
                    }
                    other => panic!("expected SymlinkTarget::Path for {symlink_path}, got {other:?}"),
                }
                let resolved = lookup(symlink_path);
                assert!(
                    Arc::ptr_eq(&resolved.entry, &expected_target.entry),
                    "unexpected target for {symlink_path}"
                );
            }
        })
        .await;
    }
}
