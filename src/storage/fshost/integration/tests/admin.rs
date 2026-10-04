// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use assert_matches::assert_matches;
use crypt_policy as _;
use fidl_fuchsia_fshost::AdminProxy;
use fidl_fuchsia_io as fio;

pub mod config;

use config::{new_builder, volumes_spec};

#[fuchsia::test]
#[cfg_attr(not(any(feature = "f2fs", feature = "minfs-no-zxcrypt")), ignore)]
async fn shred_data_volume_not_supported() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    let admin: AdminProxy = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir()
        .expect("connect_to_protcol_at_exposed_dir failed");

    let status = admin
        .shred_data_volume()
        .await
        .expect("shred_data_volume FIDL failed")
        .expect_err("shred_data_volume should fail");
    assert_eq!(zx::Status::ok(status), Err(zx::Status::NOT_SUPPORTED));

    fixture.tear_down().await;
}

#[fuchsia::test]
#[cfg_attr(any(feature = "f2fs", feature = "minfs-no-zxcrypt"), ignore)]
async fn shred_data_volume_when_mounted() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    fuchsia_fs::directory::open_file(
        &fixture.dir("data", fio::PERM_READABLE | fio::PERM_WRITABLE),
        "test-file",
        fio::Flags::FLAG_MAYBE_CREATE,
    )
    .await
    .expect("open_file failed");

    let admin: AdminProxy = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir()
        .expect("connect_to_protcol_at_exposed_dir failed");

    admin
        .shred_data_volume()
        .await
        .expect("shred_data_volume FIDL failed")
        .expect("shred_data_volume failed");

    let disk = fixture.tear_down().await.unwrap();

    let fixture = new_builder().with_disk_from(disk).build().await;

    // If we try and open the same test file, it shouldn't exist because the data volume should have
    // been shredded.
    assert_matches!(
        fuchsia_fs::directory::open_file(
            &fixture.dir("data", fio::PERM_READABLE),
            "test-file",
            fio::PERM_READABLE,
        )
        .await
        .expect_err("open_file failed"),
        fuchsia_fs::node::OpenError::OpenError(zx::Status::NOT_FOUND)
    );

    fixture.tear_down().await;
}

#[fuchsia::test]
#[cfg_attr(any(feature = "f2fs", feature = "minfs-no-zxcrypt"), ignore)]
async fn shred_data_volume_from_recovery() {
    let mut builder = new_builder();
    builder.with_disk().with_gpt().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    fuchsia_fs::directory::open_file(
        &fixture.dir("data", fio::PERM_READABLE | fio::PERM_WRITABLE),
        "test-file",
        fio::Flags::FLAG_MAYBE_CREATE,
    )
    .await
    .expect("open_file failed");

    let disk = fixture.tear_down().await.unwrap();

    // Launch a version of fshost that will behave like recovery: it will mount data and blob from
    // a ramdisk it launches, binding the fvm on the "regular" disk but otherwise leaving it alone.
    let mut builder = new_builder().with_disk_from(disk);
    builder.fshost().set_config_value("ramdisk_image", true);
    builder.with_zbi_ramdisk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    let admin: AdminProxy = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir()
        .expect("connect_to_protcol_at_exposed_dir failed");

    admin
        .shred_data_volume()
        .await
        .expect("shred_data_volume FIDL failed")
        .expect("shred_data_volume failed");

    let disk = fixture.tear_down().await.unwrap();

    let fixture = new_builder().with_disk_from(disk).build().await;

    // If we try and open the same test file, it shouldn't exist because the data volume should have
    // been shredded.
    assert_matches!(
        fuchsia_fs::directory::open_file(
            &fixture.dir("data", fio::PERM_READABLE),
            "test-file",
            fio::PERM_READABLE
        )
        .await
        .expect_err("open_file failed"),
        fuchsia_fs::node::OpenError::OpenError(zx::Status::NOT_FOUND)
    );

    fixture.tear_down().await;
}

#[cfg(feature = "fxblob")]
mod fxblob {
    use super::*;
    use config::{blob_fs_type, data_fs_type};
    use fidl::endpoints::Proxy as _;
    use fidl_fuchsia_fshost::StarnixVolumeProviderProxy;
    use fshost_test_fixture::STARNIX_VOLUME_NAME;
    use fshost_test_fixture::disk_builder::DataSpec;

    fn keymint_data_fs_spec() -> DataSpec {
        DataSpec {
            format: Some("fxfs"),
            zxcrypt: false,
            crypt_policy: crypt_policy::Policy::Keymint,
        }
    }

    #[fuchsia::test]
    async fn shred_data_volume_when_mounted_keymint() {
        let mut builder = new_builder().with_crypt_policy(crypt_policy::Policy::Keymint);
        builder.with_disk().format_volumes(volumes_spec()).format_data(keymint_data_fs_spec());
        let fixture = builder.build().await;

        fuchsia_fs::directory::open_file(
            &fixture.dir("data", fio::PERM_READABLE | fio::PERM_WRITABLE),
            "test-file",
            fio::Flags::FLAG_MAYBE_CREATE,
        )
        .await
        .expect("open_file failed");

        let admin: AdminProxy = fixture
            .realm
            .root
            .connect_to_protocol_at_exposed_dir()
            .expect("connect_to_protcol_at_exposed_dir failed");

        admin
            .shred_data_volume()
            .await
            .expect("shred_data_volume FIDL failed")
            .expect("shred_data_volume failed");

        let disk = fixture.tear_down().await.unwrap();

        let fixture = new_builder().with_disk_from(disk).build().await;

        // If we try and open the same test file, it shouldn't exist because the data volume should
        // have been shredded.
        assert_matches!(
            fuchsia_fs::directory::open_file(
                &fixture.dir("data", fio::PERM_READABLE),
                "test-file",
                fio::PERM_READABLE,
            )
            .await
            .expect_err("open_file failed"),
            fuchsia_fs::node::OpenError::OpenError(zx::Status::NOT_FOUND)
        );

        fixture.tear_down().await;
    }

    #[fuchsia::test]
    async fn shred_data_deletes_starnix_volume() {
        let mut builder = new_builder();
        builder.with_disk().format_volumes(volumes_spec());
        builder
            .fshost()
            .create_starnix_volume_crypt()
            .set_config_value("starnix_volume_name", STARNIX_VOLUME_NAME);
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
        let (_exposed_dir_proxy, exposed_dir_server) =
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

        let admin: AdminProxy = fixture
            .realm
            .root
            .connect_to_protocol_at_exposed_dir()
            .expect("connect_to_protcol_at_exposed_dir failed");

        admin
            .shred_data_volume()
            .await
            .expect("shred_data_volume FIDL failed")
            .expect("shred_data_volume failed");
        let disk = fixture.tear_down().await.unwrap();

        let mut builder = new_builder().with_disk_from(disk);
        builder
            .fshost()
            .create_starnix_volume_crypt()
            .set_config_value("starnix_volume_name", STARNIX_VOLUME_NAME);
        let fixture = builder.build().await;

        fixture.check_fs_type("blob", blob_fs_type()).await;
        fixture.check_fs_type("data", data_fs_type()).await;

        let volumes_dir = fixture.dir("volumes", fio::PERM_READABLE);
        let dir_entries = fuchsia_fs::directory::readdir(&volumes_dir)
            .await
            .expect("Failed to readdir the volumes");
        assert!(dir_entries.iter().find(|x| x.name.contains(STARNIX_VOLUME_NAME)).is_none());

        fixture.tear_down().await;
    }
}
