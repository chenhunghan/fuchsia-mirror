// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crypt_policy as _;
use fake_keymint::FakeKeymint;
use fidl_fuchsia_security_keymint as fkeymint;
use fshost_test_fixture::disk_builder::DataSpec;
use futures::FutureExt as _;

pub mod config;

use config::{blob_fs_type, data_fs_spec, data_fs_type, new_builder, volumes_spec};

fn keymint_data_fs_spec() -> DataSpec {
    DataSpec { format: Some("fxfs"), zxcrypt: false, crypt_policy: crypt_policy::Policy::Keymint }
}

#[fuchsia::test]
async fn blobfs_and_data_mounted_with_keymint() {
    let mut builder = new_builder().with_crypt_policy(crypt_policy::Policy::Keymint);
    let data_spec =
        DataSpec { zxcrypt: false, crypt_policy: crypt_policy::Policy::Keymint, ..data_fs_spec() };
    builder.with_disk().format_volumes(volumes_spec()).format_data(data_spec);
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    fixture.check_test_data_file().await;
    fixture.check_test_blob().await;
    fixture.tear_down().await;
}

#[fuchsia::test]
async fn data_formatted_keymint() {
    let mut builder = new_builder().with_crypt_policy(crypt_policy::Policy::Keymint);
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    fixture.check_test_blob().await;

    fixture.tear_down().await;
}

/// Tests the early-boot recovery mechanism when an `old_blob` is found in the KeyMint
/// persistence file. This indicates a previous upgrade was interrupted by a power failure
/// before the old key could be deleted from the hardware (or a failure of the 'delete_key'
/// method).
///
/// We must ensure fshost deletes the old key from the TEE and cleans up the persistent state
/// during early boot to prevent the hardware slot from leaking. Hardware slots may be limited,
/// so failing to clean them up across multiple interrupted upgrades could eventually brick a
/// device.
#[fuchsia::test]
async fn data_formatted_keymint_upgrade_recovery() {
    let mut builder = new_builder().with_crypt_policy(crypt_policy::Policy::Keymint);

    // Provide a valid "upgraded" mock blob format that FakeKeymint tracks, so it will
    // actually delete it.
    let old_blob = {
        let km = FakeKeymint::default();
        km.bump_epoch();
        km.generate_static_sealing_key(b"fuchsia")
    };
    let shared_keymint = builder.keymint();

    // We must forcibly inject the simulated old_blob into the keymint hardware state so the
    // test has something to actually scrub.
    shared_keymint.insert_sealing_key(b"fuchsia", vec![old_blob.clone()]);

    builder
        .with_disk()
        .with_keymint_instance(shared_keymint.clone())
        .with_keymint_old_blob(old_blob.clone())
        .format_volumes(volumes_spec());
    let fixture = builder.build().await;

    // Confirm that fshost mounted the data volume smoothly despite the interrupted upgrade
    // state.
    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    fixture.check_test_blob().await;

    // Verify the successful hardware cleanup path!
    assert!(!shared_keymint.has_key_blob(&old_blob));

    fixture.tear_down().await;
}

/// Tests the fallback behavior of the early-boot recovery mechanism when the hardware deletion
/// of the `old_blob` fails. This simulates a scenario where a power failure interrupted an
/// upgrade, and upon reboot, the KeyMint refuses to delete the old key (e.g., due to an
/// internal fault).
///
/// In this case, fshost must swallow the deletion error, remove the `old_blob` from the
/// persistence file, and proceed with mounting the filesystem. While this leaks a single
/// hardware key slot, it prevents an infinite bootloop that could permanently brick the device.
/// We prioritize device availability while tolerating an unrecoverable hardware fault.
#[fuchsia::test]
async fn data_formatted_keymint_upgrade_deletion_failure_recovery() {
    let mut builder = new_builder().with_crypt_policy(crypt_policy::Policy::Keymint);

    builder
        .keymint()
        .set_delete_hook(|_| async { Some(fkeymint::DeleteError::FailedDelete) }.boxed());

    let old_blob = vec![0x11, 0x22, 0x33, 0x44];
    let shared_keymint = builder.keymint();
    builder
        .with_disk()
        .with_keymint_instance(shared_keymint.clone())
        .with_keymint_old_blob(old_blob)
        .format_volumes(volumes_spec())
        .format_data(keymint_data_fs_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("data", data_fs_type()).await;

    fixture.tear_down().await;
}

/// Tests the primary inline key upgrade path. During a normal filesystem mount, KeyMint may
/// return `KeyRequiresUpgrade` during `Unseal`. fshost must successfully re-seal the key with
/// the upgraded material, persist the new blob, delete the old blob from hardware, and continue
/// mounting.
///
/// This test validates the happy path of the upgrade process, where both the persistent file
/// update and the hardware key deletion complete successfully without interruption or errors.
#[fuchsia::test]
async fn data_formatted_keymint_upgrade_on_unseal() {
    let mut builder = new_builder().with_crypt_policy(crypt_policy::Policy::Keymint);
    let shared_keymint = builder.keymint();
    let initial_blob = shared_keymint.generate_static_sealing_key(b"fuchsia");
    let disk = {
        builder.with_disk().format_volumes(volumes_spec()).format_data(keymint_data_fs_spec());
        let fixture = builder.build().await;
        fixture.tear_down().await.unwrap()
    };

    shared_keymint.bump_epoch();
    let fixture = new_builder()
        .with_crypt_policy(crypt_policy::Policy::Keymint)
        .with_disk_from(disk)
        .with_keymint_instance(shared_keymint.clone())
        .build()
        .await;

    fixture.check_fs_type("data", data_fs_type()).await;

    let upgraded_blob = shared_keymint.generate_static_sealing_key(b"fuchsia");
    assert!(shared_keymint.has_key_blob(&upgraded_blob));
    assert!(!shared_keymint.has_key_blob(&initial_blob));

    fixture.tear_down().await;
}

/// Tests the resilience of the inline upgrade path when the hardware deletion of the old key
/// fails synchronously during the upgrade operation.
///
/// If `DeleteSealingKey` fails immediately after we've successfully persisted the newly
/// upgraded blob, fshost must log the error but still complete the mount successfully. This
/// leaks a key slot in the TEE but ensures the device remains usable. This validates the
/// primary path's resilience, distinct from the early-boot fallback cleanup mechanism tested
/// elsewhere.
///
/// Note that the failed deletion gets one more attempt at next load time, so a single failed
/// deletion does not necessarily leak anything. A leak will only occur if the deletion fails
/// multiple times in a row.
#[fuchsia::test]
async fn data_formatted_keymint_upgrade_on_unseal_with_deletion_failure() {
    let mut builder = new_builder().with_crypt_policy(crypt_policy::Policy::Keymint);
    let shared_keymint = builder.keymint();
    let initial_blob = shared_keymint.generate_static_sealing_key(b"fuchsia");
    let disk = {
        builder.with_disk().format_volumes(volumes_spec()).format_data(keymint_data_fs_spec());
        let fixture = builder.build().await;
        fixture.tear_down().await.unwrap()
    };

    shared_keymint.bump_epoch();
    shared_keymint.set_delete_hook(|_| async { Some(fkeymint::DeleteError::FailedDelete) }.boxed());

    let fixture = new_builder()
        .with_crypt_policy(crypt_policy::Policy::Keymint)
        .with_disk_from(disk)
        .with_keymint_instance(shared_keymint.clone())
        .build()
        .await;

    fixture.check_fs_type("data", data_fs_type()).await;

    // We failed the deletion, so the old key MUST leak into the TEE slot.
    let upgraded_blob = shared_keymint.generate_static_sealing_key(b"fuchsia");
    assert!(shared_keymint.has_key_blob(&upgraded_blob));
    assert!(shared_keymint.has_key_blob(&initial_blob));

    fixture.tear_down().await;
}

/// Tests the scenario where multiple keys (e.g., both `data.data` and `data.metadata`) require
/// an upgrade during the same mount cycle.
///
/// We intentionally fail `Unseal` with `KeyRequiresUpgrade` multiple times to force both keys
/// to traverse the upgrade path. This ensures that the fshost upgrade logic correctly handles
/// multiple sequential upgrades without corrupting the persistence file or panicking, and
/// successfully mounts the filesystem after all keys have been upgraded.
#[fuchsia::test]
async fn data_formatted_keymint_double_upgrade() {
    let mut builder = new_builder().with_crypt_policy(crypt_policy::Policy::Keymint);
    let shared_keymint = builder.keymint();
    let initial_blob = shared_keymint.generate_static_sealing_key(b"fuchsia");
    let disk = {
        builder.with_disk().format_volumes(volumes_spec()).format_data(keymint_data_fs_spec());
        let fixture = builder.build().await;
        fixture.tear_down().await.unwrap()
    };

    shared_keymint.bump_epoch();
    let fixture = new_builder()
        .with_crypt_policy(crypt_policy::Policy::Keymint)
        .with_disk_from(disk)
        .with_keymint_instance(shared_keymint.clone())
        .build()
        .await;

    fixture.check_fs_type("data", data_fs_type()).await;

    // Both data.data and data.metadata trigger an upgrade on the same `fuchsia` key payload.
    // Ensure the old shared key blob gets properly erased.
    let upgraded_blob = shared_keymint.generate_static_sealing_key(b"fuchsia");
    assert!(shared_keymint.has_key_blob(&upgraded_blob));
    assert!(!shared_keymint.has_key_blob(&initial_blob));

    fixture.tear_down().await;
}
