// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use blob_writer::BlobWriter;
use crypt_policy as _;
use delivery_blob::{CompressionMode, Type1Blob};
use fidl_fuchsia_fxfs::{BlobCreatorProxy, BlobReaderProxy};
use fidl_fuchsia_io as fio;
use fidl_fuchsia_update_verify::HealthStatus;
use fshost_test_fixture::VFS_TYPE_MEMFS;
use fshost_test_fixture::disk_builder::{BLOBFS_MAX_BYTES, VolumesSpec};
use fuchsia_async as fasync;
use futures::FutureExt as _;

pub mod config;

use config::{blob_fs_type, data_fs_spec, data_fs_type, new_builder, volumes_spec};

#[fuchsia::test]
async fn blobfs_and_data_mounted() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).format_data(data_fs_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("blob-exec", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    // Also make sure tmpfs is getting exported.
    fixture.check_fs_type("tmp", VFS_TYPE_MEMFS).await;
    fixture.check_test_data_file().await;
    fixture.check_test_blob().await;

    let blob_dir =
        fixture.dir("blob-exec", fio::PERM_READABLE | fio::PERM_WRITABLE | fio::PERM_EXECUTABLE);
    assert!(fuchsia_fs::directory::readdir(&blob_dir).await.unwrap().len() > 0);

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn blobfs_and_data_mounted_with_extra_volume() {
    let mut builder = new_builder();
    builder
        .with_disk()
        .format_volumes(volumes_spec())
        .format_data(data_fs_spec())
        .with_extra_volume("internal");
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("blob-exec", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    fixture.check_test_data_file().await;
    fixture.check_test_blob().await;

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn blobfs_and_data_mounted_legacy_label() {
    let mut builder = new_builder();
    builder
        .with_disk()
        .format_volumes(volumes_spec())
        .format_data(data_fs_spec())
        .with_legacy_data_label();
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    fixture.check_test_data_file().await;
    fixture.check_test_blob().await;

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn data_formatted() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    fixture.check_test_blob().await;

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn data_partition_nonexistent() {
    let mut builder = new_builder();
    builder
        .with_disk()
        .format_volumes(VolumesSpec { create_data_partition: false, ..volumes_spec() });
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    fixture.check_test_blob().await;

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn data_formatted_legacy_label() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).with_legacy_data_label();
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn data_formatted_no_fuchsia_boot() {
    let mut builder = new_builder().no_fuchsia_boot();
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    fixture.tear_down().await;
}
#[fuchsia::test]
async fn delivery_blob_support() {
    let mut builder = new_builder();
    builder.fshost().set_config_value("blob_max_bytes", BLOBFS_MAX_BYTES);
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    let data: Vec<u8> = vec![0xff; 65536];
    let hash = fuchsia_merkle::root_from_slice(&data);
    let payload = Type1Blob::generate(&data, CompressionMode::Always);

    let blob_creator: BlobCreatorProxy = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir()
        .expect("connect_to_protocol_at_exposed_dir failed");
    let blob_writer_client_end = blob_creator
        .create(&hash.into(), false)
        .await
        .expect("transport error on create")
        .expect("failed to create blob");

    let writer = blob_writer_client_end.into_proxy();
    let mut blob_writer = BlobWriter::create(writer, payload.len() as u64)
        .await
        .expect("failed to create BlobWriter");
    blob_writer.write(&payload).await.unwrap();

    // We should now be able to open the blob by its hash and read the contents back.
    let blob_reader: BlobReaderProxy = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir()
        .expect("connect_to_protocol_at_exposed_dir failed");
    let vmo = blob_reader.get_vmo(&hash.into()).await.unwrap().unwrap();

    // Read the last 1024 bytes of the file and ensure the bytes match the original `data`.
    let mut buf = vec![0; 1024];
    let offset: u64 = data.len().checked_sub(1024).unwrap() as u64;
    let () = vmo.read(&mut buf, offset).unwrap();
    assert_eq!(&buf, &data[offset as usize..]);

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn data_persists() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).format_data(data_fs_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    let data_root = fixture.dir("data", fio::PERM_READABLE | fio::PERM_WRITABLE);
    let file = fuchsia_fs::directory::open_file(
        &data_root,
        "file",
        fio::Flags::FLAG_MUST_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE,
    )
    .await
    .unwrap();
    fuchsia_fs::file::write(&file, "file contents!").await.unwrap();

    // Shut down fshost, which should propagate to the data filesystem too.
    let disk = fixture.tear_down().await.unwrap();
    let builder = new_builder().with_disk_from(disk);
    let fixture = builder.build().await;

    fixture.check_fs_type("data", data_fs_type()).await;

    let data_root = fixture.dir("data", fio::PERM_READABLE);
    let file =
        fuchsia_fs::directory::open_file(&data_root, "file", fio::PERM_READABLE).await.unwrap();
    assert_eq!(&fuchsia_fs::file::read(&file).await.unwrap()[..], b"file contents!");

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn health_check_service() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).format_data(data_fs_spec());
    let fixture = builder.build().await;

    let proxy = fuchsia_component::client::connect_to_protocol_at_dir_root::<
        fidl_fuchsia_update_verify::ComponentOtaHealthCheckMarker,
    >(fixture.exposed_dir())
    .unwrap();
    let status = proxy.get_health_status().await.expect("FIDL error");
    assert_eq!(status, HealthStatus::Healthy);

    // `get_health_status` only blocks until `blob` is mounted and does not wait for `data`, so
    // `fshost` may still be mounting `data` when it returns. Wait for `data` to finish mounting
    // before tearing down the realm.
    fixture.check_fs_type("data", data_fs_type()).await;

    fixture.tear_down().await;
}
#[fuchsia::test]
async fn disable_block_watcher() {
    let mut builder = new_builder();
    builder.fshost().set_config_value("disable_block_watcher", true);
    builder.with_disk().format_volumes(volumes_spec()).format_data(data_fs_spec());
    let fixture = builder.build().await;

    // The filesystems are not mounted when the block watcher is disabled.
    futures::select! {
        _ = fixture.check_fs_type("data", data_fs_type()).fuse() => {
            panic!("check_fs_type returned unexpectedly - data was mounted");
        },
        _ = fixture.check_fs_type("blob", blob_fs_type()).fuse() => {
            panic!("check_fs_type returned unexpectedly - blob was mounted");
        },
        _ = fasync::Timer::new(std::time::Duration::from_secs(2)).fuse() => (),
    }

    fixture.tear_down().await;
}

#[cfg(feature = "fxblob")]
mod fxblob {
    use super::*;

    use assert_matches::assert_matches;
    use diagnostics_assertions::assert_data_tree;
    use diagnostics_reader::ArchiveReader;
    use fidl::endpoints::{DiscoverableProtocolMarker as _, Proxy};
    use fidl_fuchsia_fshost::StarnixVolumeProviderProxy;
    use fshost_test_fixture::STARNIX_VOLUME_NAME;

    async fn shutdown_starnix_volume(exposed_dir: fio::DirectoryProxy) {
        let (proxy, server_end) = fidl::endpoints::create_proxy::<fidl_fuchsia_fs::AdminMarker>();
        exposed_dir
            .open(
                &format!("svc/{}", fidl_fuchsia_fs::AdminMarker::PROTOCOL_NAME),
                fio::Flags::PROTOCOL_SERVICE,
                &fio::Options::default(),
                server_end.into(),
            )
            .expect("fidl transport error");

        proxy.shutdown().await.expect("fidl transport error");
    }

    #[fuchsia::test]
    async fn create_unmount_and_remount_starnix_volume() {
        let mut builder = new_builder();
        builder
            .fshost()
            .create_starnix_volume_crypt()
            .set_config_value("starnix_volume_name", STARNIX_VOLUME_NAME);
        builder.with_disk().format_volumes(volumes_spec());
        let fixture = builder.build().await;

        fixture.check_fs_type("blob", blob_fs_type()).await;
        fixture.check_fs_type("data", data_fs_type()).await;

        let volume_provider: StarnixVolumeProviderProxy =
            fixture.realm.root.connect_to_protocol_at_exposed_dir().expect(
                "connect_to_protocol_at_exposed_dir failed for the StarnixVolumeProvider protocol",
            );
        // Check should succeed when there's no volume
        let (crypt, _crypt_management) = fixture.setup_starnix_crypt().await;
        volume_provider
            .check(crypt.into_client_end().unwrap())
            .await
            .expect("fidl transport error")
            .expect("check with absent volume failed");

        let crypt = fixture.connect_to_crypt();
        let (exposed_dir_proxy, exposed_dir_server) =
            fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
        volume_provider
            .mount(
                crypt.into_client_end().unwrap(),
                fidl_fuchsia_fshost::MountMode::AlwaysCreate,
                exposed_dir_server,
            )
            .await
            .expect("fidl transport error")
            .expect("mount failed");

        async fn check_inspect(child_name: &str) {
            let inspector = ArchiveReader::inspect()
                .add_selector(format!("realm_builder\\:{}/test-fshost/fxfs:root", child_name,))
                .snapshot()
                .await
                .expect("inspect snapshot failed")
                .into_iter()
                .next()
                .and_then(|result| result.payload)
                .expect("expected one inspect hierarchy");

            assert_data_tree!(inspector, root: contains {
                stores: contains {
                    STARNIX_VOLUME_NAME.to_string() => contains {
                        low_32_bit_object_ids: true,
                    }
                }
            });
        }

        check_inspect(fixture.realm.root.child_name()).await;

        let starnix_volume_root_dir = fuchsia_fs::directory::open_directory(
            &exposed_dir_proxy,
            "root",
            fio::PERM_READABLE | fio::PERM_WRITABLE,
        )
        .await
        .expect("Failed to open the root dir of the starnix volume");

        let starnix_volume_file = fuchsia_fs::directory::open_file(
            &starnix_volume_root_dir,
            "file",
            fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE,
        )
        .await
        .expect("Failed to create file in starnix volume");
        fuchsia_fs::file::write(&starnix_volume_file, "file contents!").await.unwrap();

        shutdown_starnix_volume(exposed_dir_proxy).await;

        let disk = fixture.tear_down().await.unwrap();
        let mut builder = new_builder().with_disk_from(disk);
        builder
            .fshost()
            .create_starnix_volume_crypt()
            .set_config_value("starnix_volume_name", STARNIX_VOLUME_NAME);
        let fixture = builder.build().await;

        fixture.check_fs_type("blob", blob_fs_type()).await;
        fixture.check_fs_type("data", data_fs_type()).await;

        let volume_provider: StarnixVolumeProviderProxy =
            fixture.realm.root.connect_to_protocol_at_exposed_dir().expect(
                "connect_to_protocol_at_exposed_dir failed for the StarnixVolumeProvider protocol",
            );
        let (crypt, _crypt_management) = fixture.setup_starnix_crypt().await;
        volume_provider
            .check(crypt.into_client_end().unwrap())
            .await
            .expect("fidl transport error")
            .expect("check failed");

        let crypt = fixture.connect_to_crypt();
        let (exposed_dir_proxy, exposed_dir_server) =
            fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
        volume_provider
            .mount(
                crypt.into_client_end().unwrap(),
                fidl_fuchsia_fshost::MountMode::MaybeCreate,
                exposed_dir_server,
            )
            .await
            .expect("fidl transport error")
            .expect("mount failed");

        let starnix_volume_root_dir =
            fuchsia_fs::directory::open_directory(&exposed_dir_proxy, "root", fio::PERM_READABLE)
                .await
                .expect("Failed to open the root dir of the starnix volume");

        let starnix_volume_file =
            fuchsia_fs::directory::open_file(&starnix_volume_root_dir, "file", fio::PERM_READABLE)
                .await
                .expect("Failed to create file in starnix volume");
        assert_eq!(
            &fuchsia_fs::file::read(&starnix_volume_file).await.unwrap()[..],
            b"file contents!"
        );

        check_inspect(fixture.realm.root.child_name()).await;

        fixture.tear_down().await;
    }

    #[fuchsia::test]
    async fn create_mount_and_remount_starnix_volume() {
        let mut builder = new_builder();
        builder
            .fshost()
            .create_starnix_volume_crypt()
            .set_config_value("starnix_volume_name", STARNIX_VOLUME_NAME);
        builder.with_disk().format_volumes(volumes_spec());
        let fixture = builder.build().await;

        fixture.check_fs_type("blob", blob_fs_type()).await;
        fixture.check_fs_type("data", data_fs_type()).await;

        let volume_provider: StarnixVolumeProviderProxy =
            fixture.realm.root.connect_to_protocol_at_exposed_dir().expect(
                "connect_to_protocol_at_exposed_dir failed for the StarnixVolumeProvider protocol",
            );
        let (crypt, _crypt_management) = fixture.setup_starnix_crypt().await;
        let (exposed_dir_proxy, exposed_dir_server) =
            fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
        volume_provider
            .mount(
                crypt.into_client_end().unwrap(),
                fidl_fuchsia_fshost::MountMode::AlwaysCreate,
                exposed_dir_server,
            )
            .await
            .expect("fidl transport error")
            .expect("mount failed");

        let starnix_volume_root_dir = fuchsia_fs::directory::open_directory(
            &exposed_dir_proxy,
            "root",
            fio::PERM_READABLE | fio::PERM_WRITABLE,
        )
        .await
        .expect("Failed to open the root dir of the starnix volume");

        let starnix_volume_file = fuchsia_fs::directory::open_file(
            &starnix_volume_root_dir,
            "file",
            fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE,
        )
        .await
        .expect("Failed to create file in starnix volume");
        fuchsia_fs::file::write(&starnix_volume_file, "file contents!").await.unwrap();

        shutdown_starnix_volume(exposed_dir_proxy).await;

        let crypt = fixture.connect_to_crypt();
        let (exposed_dir_proxy, exposed_dir_server) =
            fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
        volume_provider
            .mount(
                crypt.into_client_end().unwrap(),
                fidl_fuchsia_fshost::MountMode::MaybeCreate,
                exposed_dir_server,
            )
            .await
            .expect("fidl transport error")
            .expect("mount failed");

        let starnix_volume_root_dir =
            fuchsia_fs::directory::open_directory(&exposed_dir_proxy, "root", fio::PERM_READABLE)
                .await
                .expect("Failed to open the root dir of the starnix volume");

        let starnix_volume_file =
            fuchsia_fs::directory::open_file(&starnix_volume_root_dir, "file", fio::PERM_READABLE)
                .await
                .expect("Failed to create file in starnix volume");
        assert_eq!(
            &fuchsia_fs::file::read(&starnix_volume_file).await.unwrap()[..],
            b"file contents!"
        );

        fixture.tear_down().await;
    }

    #[fuchsia::test]
    async fn create_starnix_volume_wipes_previous_volume() {
        let mut builder = new_builder();
        builder
            .fshost()
            .create_starnix_volume_crypt()
            .set_config_value("starnix_volume_name", STARNIX_VOLUME_NAME);
        builder.with_disk().format_volumes(volumes_spec());
        let fixture = builder.build().await;

        fixture.check_fs_type("blob", blob_fs_type()).await;
        fixture.check_fs_type("data", data_fs_type()).await;

        let volume_provider: StarnixVolumeProviderProxy =
            fixture.realm.root.connect_to_protocol_at_exposed_dir().expect(
                "connect_to_protocol_at_exposed_dir failed for the StarnixVolumeProvider protocol",
            );
        let (crypt, _crypt_management) = fixture.setup_starnix_crypt().await;
        let (exposed_dir_proxy, exposed_dir_server) =
            fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
        volume_provider
            .mount(
                crypt.into_client_end().unwrap(),
                fidl_fuchsia_fshost::MountMode::AlwaysCreate,
                exposed_dir_server,
            )
            .await
            .expect("fidl transport error")
            .expect("mount failed");

        let starnix_volume_root_dir = fuchsia_fs::directory::open_directory(
            &exposed_dir_proxy,
            "root",
            fio::PERM_READABLE | fio::PERM_WRITABLE,
        )
        .await
        .expect("Failed to open the root dir of the starnix volume");

        let starnix_volume_file = fuchsia_fs::directory::open_file(
            &starnix_volume_root_dir,
            "file",
            fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE,
        )
        .await
        .expect("Failed to create file in starnix volume");
        fuchsia_fs::file::write(&starnix_volume_file, "file contents!").await.unwrap();

        shutdown_starnix_volume(exposed_dir_proxy).await;

        let disk = fixture.tear_down().await.unwrap();
        let mut builder = new_builder().with_disk_from(disk);
        builder
            .fshost()
            .create_starnix_volume_crypt()
            .set_config_value("starnix_volume_name", STARNIX_VOLUME_NAME);
        let fixture = builder.build().await;

        fixture.check_fs_type("blob", blob_fs_type()).await;
        fixture.check_fs_type("data", data_fs_type()).await;

        let volume_provider: StarnixVolumeProviderProxy =
            fixture.realm.root.connect_to_protocol_at_exposed_dir().expect(
                "connect_to_protocol_at_exposed_dir failed for the StarnixVolumeProvider protocol",
            );
        let (crypt, _crypt_management) = fixture.setup_starnix_crypt().await;
        let (exposed_dir_proxy, exposed_dir_server) =
            fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
        volume_provider
            .mount(
                crypt.into_client_end().unwrap(),
                fidl_fuchsia_fshost::MountMode::AlwaysCreate,
                exposed_dir_server,
            )
            .await
            .expect("fidl transport error")
            .expect("mount failed");

        let starnix_volume_root_dir =
            fuchsia_fs::directory::open_directory(&exposed_dir_proxy, "root", fio::PERM_READABLE)
                .await
                .expect("Failed to open the root dir of the starnix volume");

        assert_matches!(
            fuchsia_fs::directory::open_file(&starnix_volume_root_dir, "file", fio::PERM_READABLE)
                .await
                .expect_err(
                    "StarnixVolumeProvider.Create should wipe the Starnix volume if it exists"
                ),
            fuchsia_fs::node::OpenError::OpenError(zx::Status::NOT_FOUND)
        );

        fixture.tear_down().await;
    }

    #[fuchsia::test]
    async fn vend_a_fresh_starnix_test_volume_on_each_mount() {
        let mut builder = new_builder();
        builder.with_disk().format_volumes(volumes_spec());
        builder.fshost().create_starnix_volume_crypt();
        let fixture = builder.build().await;

        fixture.check_fs_type("blob", blob_fs_type()).await;
        fixture.check_fs_type("data", data_fs_type()).await;

        // Need to connect to the StarnixVolumeProvider protocol that fshost exposes and Mount the
        // starnix volume.
        let volume_provider: StarnixVolumeProviderProxy =
            fixture.realm.root.connect_to_protocol_at_exposed_dir().expect(
                "connect_to_protocol_at_exposed_dir failed for the StarnixVolumeProvider protocol",
            );
        let (crypt, _crypt_management) = fixture.setup_starnix_crypt().await;
        let (exposed_dir_proxy, exposed_dir_server) =
            fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
        volume_provider
            .mount(
                crypt.into_client_end().unwrap(),
                fidl_fuchsia_fshost::MountMode::MaybeCreate,
                exposed_dir_server,
            )
            .await
            .expect("fidl transport error")
            .expect("mount failed");

        let starnix_volume_root_dir = fuchsia_fs::directory::open_directory(
            &exposed_dir_proxy,
            "root",
            fio::PERM_READABLE | fio::PERM_WRITABLE,
        )
        .await
        .expect("Failed to open the root dir of the starnix volume");

        let starnix_volume_file = fuchsia_fs::directory::open_file(
            &starnix_volume_root_dir,
            "file",
            fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE,
        )
        .await
        .expect("Failed to create file in starnix volume");
        fuchsia_fs::file::write(&starnix_volume_file, "file contents!").await.unwrap();

        shutdown_starnix_volume(exposed_dir_proxy).await;

        let disk = fixture.tear_down().await.unwrap();
        let mut builder = new_builder().with_disk_from(disk);
        builder.fshost().create_starnix_volume_crypt();
        let fixture = builder.build().await;

        fixture.check_fs_type("blob", blob_fs_type()).await;
        fixture.check_fs_type("data", data_fs_type()).await;

        // Need to connect to the StarnixVolumeProvider protocol that fshost exposes and Mount the
        // starnix volume.
        let volume_provider: StarnixVolumeProviderProxy =
            fixture.realm.root.connect_to_protocol_at_exposed_dir().expect(
                "connect_to_protocol_at_exposed_dir failed for the StarnixVolumeProvider protocol",
            );
        let (crypt, _crypt_management) = fixture.setup_starnix_crypt().await;
        let (exposed_dir_proxy, exposed_dir_server) =
            fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
        volume_provider
            .mount(
                crypt.into_client_end().unwrap(),
                fidl_fuchsia_fshost::MountMode::MaybeCreate,
                exposed_dir_server,
            )
            .await
            .expect("fidl transport error")
            .expect("mount failed");

        let starnix_volume_root_dir =
            fuchsia_fs::directory::open_directory(&exposed_dir_proxy, "root", fio::PERM_READABLE)
                .await
                .expect("Failed to open the root dir of the starnix volume");

        // fshost should vend a fresh Starnix test volume on every mount so this file should no
        // longer exist.
        fuchsia_fs::directory::open_file(&starnix_volume_root_dir, "file", fio::PERM_READABLE)
            .await
            .expect_err("fshost should vend a fresh Starnix test volume on every mount");

        fixture.tear_down().await;
    }

    #[fuchsia::test]
    async fn health_check_blobs() {
        let mut builder = new_builder();
        builder.with_disk().format_volumes(volumes_spec()).format_data(data_fs_spec());
        let fixture = builder.build().await;

        let blobfs_health_check: fidl_fuchsia_update_verify::ComponentOtaHealthCheckProxy = fixture
            .realm
            .root
            .connect_to_protocol_at_exposed_dir()
            .expect("connect_to_protcol_at_exposed_dir failed");
        let status = blobfs_health_check.get_health_status().await.expect("FIDL failure");
        assert_eq!(status, HealthStatus::Healthy);

        fixture.tear_down().await;
    }
}
