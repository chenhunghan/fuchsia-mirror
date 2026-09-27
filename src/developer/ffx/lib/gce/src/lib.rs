// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod client;
pub mod context;
pub mod error;
pub mod image;
pub mod models;
pub mod tunnel;

pub use client::{Api, GceClient};
pub use context::{GceContext, get_serial_endpoint, resolve_config_string};
pub use discovery::gce_watcher::{GceInstance, GceInstanceData, GceWatcher};
pub use error::{BoxedError, GceError, Result};
pub use image::{
    BUNDLE_HASH_HEX_LEN, DEFAULT_GCE_DISK_SIZE, GceVmShape, VbmetaKeys, compute_bundle_hash,
    gce_vm_shape, generate_instance_name, generate_serial_number, package_gce_tar_gz,
    prepare_gce_disk_archive, product_bundle_architecture,
};
pub use models::{
    AccessConfig, AttachedDisk, AttachedDiskInitializeParams, FirewallAllowed, FirewallRule,
    GuestOsFeature, Image, Instance, InstanceList, Metadata, MetadataItem, NetworkInterface,
    Operation, RawDisk, SerialPortOutput, StartResult, StopResult,
};
pub use tunnel::{GceTunnel, GceTunnelConfig, read_gce_ssh_pubkey, read_gce_ssh_pubkeys};
