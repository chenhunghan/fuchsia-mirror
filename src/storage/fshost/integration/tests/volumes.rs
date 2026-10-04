// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use blob_writer::BlobWriter;
use crypt_policy as _;
use delivery_blob::{CompressionMode, Type1Blob};
use fidl_fuchsia_fs_startup::VolumeMarker as FsStartupVolumeMarker;
use fidl_fuchsia_fxfs::BlobCreatorProxy;
use fidl_fuchsia_io as fio;
use fshost_test_fixture::disk_builder::{BLOBFS_MAX_BYTES, VolumesSpec};
use fshost_test_fixture::{TestFixture, round_down};
use fuchsia_component::client::connect_to_named_protocol_at_dir_root;
use regex::Regex;

pub mod config;

use config::{
    blob_fs_type, data_fs_spec, data_fs_type, data_fs_zxcrypt, data_max_bytes, fvm_slice_size,
    new_builder, volumes_spec,
};

#[fuchsia::test]
async fn data_formatted_with_small_initial_volume() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).data_volume_size(fvm_slice_size());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn data_formatted_with_small_initial_volume_big_target() {
    let mut builder = new_builder();
    // The formatting uses the max bytes argument as the initial target to resize to. If this
    // target is larger than the disk, the resize should still succeed.
    builder.fshost().set_config_value("data_max_bytes", data_max_bytes() * 2);
    builder.with_disk().format_volumes(volumes_spec()).data_volume_size(fvm_slice_size());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn set_volume_limit() {
    let mut builder = new_builder();
    builder
        .fshost()
        .set_config_value("data_max_bytes", data_max_bytes())
        .set_config_value("blob_max_bytes", BLOBFS_MAX_BYTES);
    builder.with_disk().format_volumes(volumes_spec()).format_data(data_fs_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    let volumes_dir = fixture.dir("volumes", fio::PERM_READABLE);
    let blob_volume_name = if cfg!(feature = "fxblob") { "blob" } else { "blobfs" };
    let blob_volume_proxy = connect_to_named_protocol_at_dir_root::<FsStartupVolumeMarker>(
        &volumes_dir,
        blob_volume_name,
    )
    .unwrap();
    let blobfs_limit =
        blob_volume_proxy.get_limit().await.unwrap().map_err(zx::Status::err_from_raw).unwrap();
    let expected_blobfs_limit = if cfg!(feature = "fxblob") {
        BLOBFS_MAX_BYTES
    } else {
        // The fvm component rounds the max bytes down to the nearest slice size.
        round_down(BLOBFS_MAX_BYTES, fvm_slice_size())
    };
    assert_eq!(blobfs_limit, expected_blobfs_limit);
    let data_volume_proxy =
        connect_to_named_protocol_at_dir_root::<FsStartupVolumeMarker>(&volumes_dir, "data")
            .unwrap();
    let data_limit =
        data_volume_proxy.get_limit().await.unwrap().map_err(zx::Status::err_from_raw).unwrap();
    let expected_data_limit = if cfg!(feature = "fxblob") {
        data_max_bytes()
    } else if data_fs_zxcrypt() {
        // The fvm component rounds the max bytes down to the nearest slice size, and fshost adds
        // an additional slice to account for the zxcrypt metadata.
        round_down(data_max_bytes(), fvm_slice_size()) + fvm_slice_size()
    } else {
        // The fvm component rounds the max bytes down to the nearest slice size.
        round_down(data_max_bytes(), fvm_slice_size())
    };
    assert_eq!(data_limit, expected_data_limit);

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn set_data_and_blob_max_bytes_zero() {
    let mut builder = new_builder();
    builder.fshost().set_config_value("data_max_bytes", 0).set_config_value("blob_max_bytes", 0);
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    let flags = fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE;

    let data_root = fixture.dir("data", flags);
    let file = fuchsia_fs::directory::open_file(&data_root, "file", flags).await.unwrap();
    fuchsia_fs::file::write(&file, "file contents!").await.unwrap();

    let blob_contents = vec![0; 8192];
    let hash = fuchsia_merkle::root_from_slice(&blob_contents);
    let compressed_data: Vec<u8> = Type1Blob::generate(&blob_contents, CompressionMode::Always);

    let blob_proxy: BlobCreatorProxy = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir()
        .expect("connect_to_protocol_at_exposed_dir failed");

    let writer_client_end = blob_proxy
        .create(&hash.into(), false)
        .await
        .expect("transport error on BlobCreator.Create")
        .expect("failed to create blob");
    let writer = writer_client_end.into_proxy();
    let mut blob_writer = BlobWriter::create(writer, compressed_data.len() as u64)
        .await
        .expect("failed to create BlobWriter");
    blob_writer.write(&compressed_data).await.unwrap();

    fixture.tear_down().await;
}

async fn assert_volumes_are_expected(fixture: &TestFixture) {
    let (volumes_dir, expected) = if cfg!(feature = "fxfs") {
        (fixture.dir("volumes", fio::PERM_READABLE), vec![r"^blob$", r"^data$", r"^unencrypted$"])
    } else {
        (fixture.dir("volumes", fio::PERM_READABLE), vec![r"^blobfs$", r"^data$"])
    };

    let mut expected: Vec<_> = expected.into_iter().map(|r| Regex::new(r).unwrap()).collect();

    // Ensure that the account and virtualization volumes were successfully destroyed. The volumes
    // are removed from devfs asynchronously, so use a timeout.
    let start_time = std::time::Instant::now();
    let mut dir_entries =
        fuchsia_fs::directory::readdir(&volumes_dir).await.expect("Failed to readdir the volumes");
    while dir_entries
        .iter()
        .find(|x| x.name.contains("account") || x.name.contains("virtualization"))
        .is_some()
    {
        let elapsed = start_time.elapsed().as_secs() as u64;
        if elapsed >= 30 {
            panic!("The account or virtualization partition still exists in devfs after 30 secs");
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        dir_entries = fuchsia_fs::directory::readdir(&volumes_dir)
            .await
            .expect("Failed to readdir the fvm DirectoryProxy");
    }
    for entry in dir_entries {
        let name = entry.name;
        let position = expected
            .iter()
            .position(|r| r.is_match(&name))
            .unwrap_or_else(|| panic!("Unexpected entry name: {name}"));
        expected.swap_remove(position);
    }
    assert!(expected.is_empty(), "Missing {expected:?}");
}

#[fuchsia::test]
async fn reset_volumes() {
    let mut builder = new_builder();
    builder
        .with_disk()
        .format_volumes(volumes_spec())
        .with_extra_volume("account")
        .with_extra_volume("virtualization");
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    assert_volumes_are_expected(&fixture).await;

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn reset_volumes_no_existing_data_volume() {
    let mut builder = new_builder();
    builder
        .with_disk()
        .format_volumes(VolumesSpec { create_data_partition: false, ..volumes_spec() })
        .with_extra_volume("account")
        .with_extra_volume("virtualization");
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    assert_volumes_are_expected(&fixture).await;

    fixture.tear_down().await;
}

#[cfg(feature = "fxblob")]
mod fxblob {
    use super::*;

    #[fuchsia::test]
    async fn set_volume_bytes_limit() {
        let mut builder = new_builder();
        builder
            .fshost()
            .set_config_value("data_max_bytes", data_max_bytes())
            .set_config_value("blob_max_bytes", BLOBFS_MAX_BYTES);
        builder.with_disk().format_volumes(volumes_spec());
        let fixture = builder.build().await;

        fixture.check_fs_type("blob", blob_fs_type()).await;
        fixture.check_fs_type("data", data_fs_type()).await;

        let volumes_dir = fixture.dir("volumes", fio::PERM_READABLE);

        let blob_volume_proxy =
            connect_to_named_protocol_at_dir_root::<FsStartupVolumeMarker>(&volumes_dir, "blob")
                .unwrap();
        let blob_volume_bytes_limit = blob_volume_proxy.get_limit().await.unwrap().unwrap();

        let data_volume_proxy =
            connect_to_named_protocol_at_dir_root::<FsStartupVolumeMarker>(&volumes_dir, "data")
                .unwrap();
        let data_volume_bytes_limit = data_volume_proxy.get_limit().await.unwrap().unwrap();
        assert_eq!(blob_volume_bytes_limit, BLOBFS_MAX_BYTES);
        assert_eq!(data_volume_bytes_limit, data_max_bytes());
        fixture.tear_down().await;
    }
}
