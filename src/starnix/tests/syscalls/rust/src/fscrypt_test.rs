// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(test)]
mod fscrypt_test {
    use fscrypt_shared::{
        FscryptOutput, fscrypt_add_key_arg, fscrypt_key_specifier, fscrypt_remove_key_arg,
    };
    use linux_uapi::{
        FS_IOC_ADD_ENCRYPTION_KEY, FS_IOC_GET_ENCRYPTION_POLICY, FS_IOC_GET_ENCRYPTION_POLICY_EX,
        FS_IOC_REMOVE_ENCRYPTION_KEY, FS_IOC_SET_ENCRYPTION_POLICY,
        FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER, FSCRYPT_MODE_AES_256_CTS, FSCRYPT_MODE_AES_256_HCTR2,
        FSCRYPT_MODE_AES_256_XTS, FSCRYPT_POLICY_FLAG_IV_INO_LBLK_64, FSCRYPT_POLICY_FLAGS_PAD_16,
        fscrypt_get_policy_ex_arg, fscrypt_policy_v1, fscrypt_policy_v2,
    };
    use serial_test::serial;
    use std::env::VarError;
    use std::ffi::OsString;
    use std::io::ErrorKind;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use zerocopy::{FromBytes, IntoBytes};

    fn add_encryption_key(root_dir: &std::fs::File) -> (i32, Vec<u8>) {
        let key_spec =
            fscrypt_key_specifier { type_: FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER, ..Default::default() };
        let arg = fscrypt_add_key_arg { key_spec: key_spec, raw_size: 64, ..Default::default() };
        let mut arg_vec = arg.as_bytes().to_vec();
        let mut random_vector: [u8; 64] = [0; 64];
        for i in 0..64 {
            let rand_u8: u8 = rand::random();
            random_vector[i] = rand_u8;
        }
        arg_vec.extend(random_vector);

        let ret = unsafe {
            libc::ioctl(
                root_dir.as_raw_fd(),
                FS_IOC_ADD_ENCRYPTION_KEY.try_into().unwrap(),
                arg_vec.as_mut_ptr(),
            )
        };
        (ret, arg_vec)
    }

    fn add_encryption_key_with_key_bytes(
        root_dir: &std::fs::File,
        raw_key: &[u8],
    ) -> (i32, Vec<u8>) {
        let key_spec =
            fscrypt_key_specifier { type_: FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER, ..Default::default() };
        let arg = fscrypt_add_key_arg { key_spec: key_spec, raw_size: 64, ..Default::default() };
        let mut arg_vec = arg.as_bytes().to_vec();
        arg_vec.extend(raw_key);

        let ret = unsafe {
            libc::ioctl(
                root_dir.as_raw_fd(),
                FS_IOC_ADD_ENCRYPTION_KEY.try_into().unwrap(),
                arg_vec.as_mut_ptr(),
            )
        };
        (ret, arg_vec)
    }

    fn set_encryption_policy(dir: &std::fs::File, identifier: [u8; 16]) -> i32 {
        set_encryption_policy_with_flags(dir, identifier, FSCRYPT_POLICY_FLAGS_PAD_16 as u8)
    }

    fn set_encryption_policy_with_flags(
        dir: &std::fs::File,
        identifier: [u8; 16],
        flags: u8,
    ) -> i32 {
        let policy = fscrypt_policy_v2 {
            version: 2,
            contents_encryption_mode: FSCRYPT_MODE_AES_256_XTS as u8,
            filenames_encryption_mode: FSCRYPT_MODE_AES_256_CTS as u8,
            flags,
            master_key_identifier: identifier,
            ..Default::default()
        };
        // SAFETY:
        // - `dir` is an open `File`, so `dir.as_raw_fd()` is a valid fd.
        // - `&policy` is a valid pointer to a correctly initialized `fscrypt_policy_v2`.
        // - The `ioctl` command accepts a pointer to a `fscrypt_policy_v2` struct.
        let ret = unsafe {
            libc::ioctl(dir.as_raw_fd(), FS_IOC_SET_ENCRYPTION_POLICY.try_into().unwrap(), &policy)
        };
        ret
    }

    fn set_encryption_policy_hctr2(dir: &std::fs::File, identifier: [u8; 16]) -> i32 {
        let ret = unsafe {
            let policy = fscrypt_policy_v2 {
                version: 2,
                contents_encryption_mode: FSCRYPT_MODE_AES_256_XTS as u8,
                filenames_encryption_mode: FSCRYPT_MODE_AES_256_HCTR2 as u8,
                flags: FSCRYPT_POLICY_FLAGS_PAD_16 as u8,
                master_key_identifier: identifier,
                ..Default::default()
            };
            libc::ioctl(dir.as_raw_fd(), FS_IOC_SET_ENCRYPTION_POLICY.try_into().unwrap(), &policy)
        };
        ret
    }

    fn remove_encryption_key(root_dir: &std::fs::File, identifier: [u8; 16]) -> i32 {
        let ret = unsafe {
            let mut key_spec = fscrypt_key_specifier {
                type_: FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER,
                ..Default::default()
            };
            key_spec.u.identifier.value = identifier;
            let remove_arg = fscrypt_remove_key_arg { key_spec: key_spec, ..Default::default() };

            libc::ioctl(
                root_dir.as_raw_fd(),
                FS_IOC_REMOVE_ENCRYPTION_KEY.try_into().unwrap(),
                &remove_arg,
            )
        };
        ret
    }

    fn get_root_path() -> Option<String> {
        // TODO(https://fxbug.dev/317285180): Once host syscall tests can be run in CQ with root
        // access, define MUTABLE_STORAGE for host tests.
        match std::env::var("MUTABLE_STORAGE") {
            Ok(root_path) => Some(root_path),
            Err(e) if e == VarError::NotPresent => None,
            Err(e) => panic!("fetching env var MUTABLE_STORAGE failed with {:?}", e),
        }
    }

    #[test]
    #[serial]
    fn remove_key_that_was_never_added() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let mut random_vector: [u8; 16] = [0; 16];
        for i in 0..16 {
            let rand_u8: u8 = rand::random();
            random_vector[i] = rand_u8;
        }
        let ret = remove_encryption_key(&root_dir, random_vector);
        assert!(
            ret != 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
    }

    fn get_child_binary_path() -> std::path::PathBuf {
        std::path::PathBuf::from("data/fscrypt_test")
    }

    #[test]
    #[serial]
    fn remove_key_added_by_different_user_non_root() {
        let Some(_) = get_root_path() else { return };
        let child_binary_path = get_child_binary_path();

        let output = std::process::Command::new(child_binary_path)
            .args(["--uid", "1000", "add"])
            .output()
            .expect("add encryption key failed");
        let output_str = String::from_utf8_lossy(&output.stdout);
        let fscrypt_output: FscryptOutput = serde_json::from_str(&output_str).unwrap();
        assert!(output.status.success(), "{:#?}", output.status);

        let child_binary_path = get_child_binary_path();
        let output = std::process::Command::new(child_binary_path)
            .args([
                "--uid",
                "2000",
                "remove",
                "--identifier",
                &hex::encode(fscrypt_output.identifier),
                "--should-fail",
                "true",
            ])
            .output()
            .expect("remove encryption key failed");
        assert!(output.status.success(), "{:#?}", output.status);

        // Cleanup
        let child_binary_path = get_child_binary_path();
        let output = std::process::Command::new(child_binary_path)
            .args([
                "--uid",
                "1000",
                "remove",
                "--identifier",
                &hex::encode(fscrypt_output.identifier),
                "--should-fail",
                "false",
            ])
            .output()
            .expect("remove encryption key failed");
        assert!(output.status.success(), "{:#?}", output.status);
    }

    #[test]
    #[serial]
    fn remove_key_added_by_different_user_root() {
        let Some(_) = get_root_path() else { return };
        let child_binary_path = get_child_binary_path();

        let output = std::process::Command::new(child_binary_path)
            .args(["--uid", "1000", "add"])
            .output()
            .expect("add encryption key failed");
        let output_str = String::from_utf8_lossy(&output.stdout);
        let fscrypt_output: FscryptOutput = serde_json::from_str(&output_str).unwrap();
        assert!(output.status.success(), "{:#?}", output.status);
        let root_dir =
            std::fs::File::open(&std::env::var("MUTABLE_STORAGE").unwrap()).expect("open failed");
        let ret = remove_encryption_key(&root_dir, fscrypt_output.identifier);

        assert!(
            ret != 0,
            "remove encryption key ioctl should have failed: {:?}",
            std::io::Error::last_os_error()
        );

        // Cleanup
        let child_binary_path = get_child_binary_path();
        let output = std::process::Command::new(child_binary_path)
            .args([
                "--uid",
                "1000",
                "remove",
                "--identifier",
                &hex::encode(fscrypt_output.identifier),
                "--should-fail",
                "false",
            ])
            .output()
            .expect("remove encryption key failed");
        assert!(output.status.success(), "{:#?}", output.status);
    }

    #[test]
    #[ignore] // TODO(https://fxbug.dev/359885449) use expectations
    #[serial]
    // TODO(https://fxbug.dev/358420498) Purge the key manager on FS_IOC_REMOVE_ENCRYPTION_KEY
    fn user_reads_directory_unlocked_by_different_user() {
        let Some(root_path) = get_root_path() else { return };
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();

        let child_binary_path = get_child_binary_path();

        std::os::unix::fs::chown(dir_path.clone(), Some(1000), Some(1000)).expect("chown failed");
        let output = std::process::Command::new(child_binary_path)
            .args(["--uid", "1000", "add"])
            .output()
            .expect("set encryption policy failed");
        let output_str = String::from_utf8_lossy(&output.stdout);
        let fscrypt_output: FscryptOutput = serde_json::from_str(&output_str).unwrap();
        assert!(output.status.success(), "{:#?}", output.status);

        let child_binary_path = get_child_binary_path();
        let output = std::process::Command::new(child_binary_path)
            .args([
                "--uid",
                "1000",
                "set",
                "--should-fail",
                "false",
                "--identifier",
                &hex::encode(fscrypt_output.identifier),
            ])
            .output()
            .expect("set encryption policy failed");
        assert!(output.status.success(), "{:#?}", output.status);
        std::fs::create_dir(dir_path.join("subdir")).unwrap();

        let child_binary_path = get_child_binary_path();
        let output = std::process::Command::new(child_binary_path)
            .args(["--uid", "2000", "read", "--locked", "false"])
            .output()
            .expect("set encryption policy failed");
        assert!(output.status.success(), "{:#?}", output.status);

        let child_binary_path = get_child_binary_path();
        let output = std::process::Command::new(child_binary_path)
            .args([
                "--uid",
                "1000",
                "remove",
                "--identifier",
                &hex::encode(fscrypt_output.identifier),
                "--should-fail",
                "false",
            ])
            .output()
            .expect("remove encryption key failed");
        assert!(output.status.success(), "{:#?}", output.status);

        let child_binary_path = get_child_binary_path();
        let output = std::process::Command::new(child_binary_path)
            .args(["--uid", "2000", "read", "--locked", "true"])
            .output()
            .expect("set encryption policy failed");
        assert!(output.status.success(), "{:#?}", output.status);
        std::fs::remove_dir_all(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[ignore] // TODO(https://fxbug.dev/359885449) use expectations
    #[serial]
    // TODO(https://fxbug.dev/358420498) Purge the key manager on FS_IOC_REMOVE_ENCRYPTION_KEY
    fn readdir_encrypted_directory_name() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();
        let dir = std::fs::File::open(dir_path.clone()).unwrap();
        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();
        let ret = unsafe { set_encryption_policy(&dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::fs::create_dir_all(dir_path.clone().join("subdir/subsubdir"))
            .expect("failed to create subdir");

        drop(dir);
        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        let entries = std::fs::read_dir(dir_path.clone()).expect("readdir failed");
        let mut count = 0;
        let mut encrypted_dir_name = OsString::new();
        for entry in entries {
            let entry = entry.expect("invalid entry");
            assert!(entry.file_name() != "subdir");
            encrypted_dir_name = entry.file_name();
            count += 1;
        }
        assert_eq!(count, 1);

        std::fs::read_dir(dir_path.clone().join("subdir")).expect_err(
            "should not be able to readdir locked encrypted directory
                    with its plaintext filename",
        );

        let entries = std::fs::read_dir(dir_path.join(encrypted_dir_name))
            .expect("readdir of encrypted subdir failed");
        let mut count = 0;
        for entry in entries {
            let entry = entry.expect("invalid entry");
            assert!(entry.file_name() != "subsubdir");
            count += 1;
        }
        assert_eq!(count, 1);
        std::fs::remove_dir_all(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[ignore] // TODO(https://fxbug.dev/359885449) use expectations
    #[serial]
    // TODO(https://fxbug.dev/358420498) Purge the key manager on FS_IOC_REMOVE_ENCRYPTION_KEY
    fn read_file_contents_from_handle_created_before_remove_encryption_key() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();
        let dir = std::fs::File::open(dir_path.clone()).unwrap();
        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();
        let ret = unsafe { set_encryption_policy(&dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::fs::write(dir_path.clone().join("file.txt"), "file_contents")
            .expect("create or write failed on file.txt");
        let file = std::fs::File::open(dir_path.clone().join("file.txt")).unwrap();
        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct.key_spec.u.identifier.value) };

        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        let buf = std::fs::read(dir_path.join("file.txt")).expect("read file failed");
        assert_eq!(buf.as_bytes(), "file_contents".as_bytes());

        drop(file);
        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::fs::read(dir_path.join("file.txt"))
            .expect_err("should not be able to read locked file");
        std::fs::remove_dir_all(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[ignore] // TODO(https://fxbug.dev/359885449) use expectations
    #[serial]
    // TODO(https://fxbug.dev/358420498) Purge the key manager on FS_IOC_REMOVE_ENCRYPTION_KEY
    fn readdir_locked_directory() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();
        let dir = std::fs::File::open(dir_path.clone()).unwrap();
        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();
        let ret = unsafe { set_encryption_policy(&dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::fs::create_dir(dir_path.clone().join("subdir")).expect("failed to create subdir");
        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        let entries = std::fs::read_dir(dir_path.clone()).expect("readdir failed");
        let mut count = 0;
        for entry in entries {
            let entry = entry.expect("invalid entry");
            assert!(entry.file_name() == "subdir");
            count += 1;
        }
        assert_eq!(count, 1);

        drop(dir);
        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        let entries = std::fs::read_dir(dir_path.clone()).expect("readdir failed");
        let mut count = 0;
        for entry in entries {
            let entry = entry.expect("invalid entry");
            assert!(entry.file_name() != "subdir");
            count += 1;
        }
        assert_eq!(count, 1);
        std::fs::remove_dir_all(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[serial]
    fn set_encryption_policy_with_fake_identifier_non_root() {
        let Some(root_path) = get_root_path() else { return };
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();
        let mut random_vector: [u8; 16] = [0; 16];
        for i in 0..16 {
            let rand_u8: u8 = rand::random();
            random_vector[i] = rand_u8;
        }

        std::os::unix::fs::chown(dir_path.clone(), Some(2000), Some(2000)).expect("chown failed");
        let child_binary_path = get_child_binary_path();
        let output = std::process::Command::new(child_binary_path)
            .args([
                "--uid",
                "2000",
                "set",
                "--should-fail",
                "true",
                "--identifier",
                &hex::encode(random_vector),
            ])
            .output()
            .expect("set encryption policy failed");

        assert!(output.status.success(), "{:#?}", output.status);
        std::fs::remove_dir_all(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[ignore] // TODO(https://fxbug.dev/359885449) use expectations
    #[serial]
    // TODO(https://fxbug.dev/358420498) Purge the key manager on FS_IOC_REMOVE_ENCRYPTION_KEY
    fn set_encryption_policy_with_fake_identifier_root() {
        let Some(root_path) = get_root_path() else { return };
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();
        let dir = std::fs::File::open(dir_path.clone()).unwrap();
        let mut random_vector: [u8; 16] = [0; 16];
        for i in 0..16 {
            let rand_u8: u8 = rand::random();
            random_vector[i] = rand_u8;
        }
        let ret = set_encryption_policy(&dir, random_vector);
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::fs::remove_dir_all(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[serial]
    fn set_encryption_policy_with_unknown_flags() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let dir_path = std::path::Path::new(&root_path).join("unknown_flags_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();
        let dir = std::fs::File::open(dir_path.clone()).unwrap();

        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();
        // SAFETY: `arg_struct` was returned by `add_encryption_key`, which initializes `type_` to `FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER`. Therefore, reading `identifier.value` from the union is safe.
        let identifier = unsafe { arg_struct.key_spec.u.identifier.value };

        // 0x80 is not a policy flag defined by Linux, so the policy must be rejected.
        let ret = set_encryption_policy_with_flags(&dir, identifier, 0x80);
        assert_eq!(ret, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EINVAL));

        // The rejected policy must not have been applied to the directory.
        let ret = set_encryption_policy(&dir, identifier);
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        let ret = remove_encryption_key(&root_dir, identifier);
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::fs::remove_dir_all(dir_path).expect("failed to remove unknown_flags_dir");
    }

    #[test]
    #[serial]
    fn hard_link_encrypted_file_into_unencrypted_dir() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let unencrypted_dir_path = std::path::Path::new(&root_path).join("unencrypted");
        std::fs::create_dir_all(unencrypted_dir_path.clone()).unwrap();

        let encrypted_dir_path = std::path::Path::new(&root_path).join("encrypted");
        std::fs::create_dir_all(encrypted_dir_path.clone()).unwrap();

        let dir = std::fs::File::open(encrypted_dir_path.clone()).unwrap();
        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct_1 = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();

        let ret = unsafe { set_encryption_policy(&dir, arg_struct_1.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::fs::File::create_new(encrypted_dir_path.clone().join("file"))
            .expect("failed to create file in encrypted directory");

        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct_1.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        let entries = std::fs::read_dir(encrypted_dir_path.clone()).expect("readdir failed");
        let mut encrypted_file_name = OsString::new();
        let mut count = 0;
        for entry in entries {
            let entry = entry.expect("invalid entry");
            encrypted_file_name = entry.file_name();
            count += 1;
        }
        assert_eq!(count, 1);

        std::fs::hard_link(
            encrypted_dir_path.join(encrypted_file_name),
            unencrypted_dir_path.clone().join("file"),
        )
        .expect("failed to hard link an encrypted file into an unencrypted directory ");

        let _ =
            std::fs::metadata(unencrypted_dir_path.clone().join("file")).expect("metadata failed");
        std::fs::File::open(unencrypted_dir_path.clone().join("file"))
            .expect_err("opening a locked encrypted file should fail");

        std::fs::remove_dir_all(unencrypted_dir_path).expect("failed to remove unencrypted dir");
        std::fs::remove_dir_all(encrypted_dir_path).expect("failed to remove encrypted dir");
    }

    #[test]
    #[serial]
    fn hard_link_unencrypted_file_into_encrypted_dir() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let unencrypted_dir_path = std::path::Path::new(&root_path).join("unencrypted");
        std::fs::create_dir_all(unencrypted_dir_path.clone()).unwrap();
        std::fs::File::create_new(unencrypted_dir_path.clone().join("file"))
            .expect("failed to create file in unencrypted directory");

        let encrypted_dir_path = std::path::Path::new(&root_path).join("encrypted");
        std::fs::create_dir_all(encrypted_dir_path.clone()).unwrap();

        let dir = std::fs::File::open(encrypted_dir_path.clone()).unwrap();
        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct_1 = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();

        let ret = unsafe { set_encryption_policy(&dir, arg_struct_1.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        std::fs::hard_link(
            unencrypted_dir_path.join("file"),
            encrypted_dir_path.clone().join("subdir"),
        )
        .expect_err(
            "hard linking an unencrypted file into an encrypted directory should fail with EXDEV",
        );

        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct_1.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::fs::remove_dir_all(unencrypted_dir_path).expect("failed to remove unencrypted dir");
        std::fs::remove_dir_all(encrypted_dir_path).expect("failed to remove encrypted dir");
    }

    #[test]
    #[serial]
    fn rename_unencrypted_file_into_encrypted_dir() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let unencrypted_dir_path = std::path::Path::new(&root_path).join("rename_unencrypted");
        std::fs::create_dir_all(unencrypted_dir_path.clone()).unwrap();
        std::fs::File::create_new(unencrypted_dir_path.clone().join("file"))
            .expect("failed to create file in unencrypted directory");

        let encrypted_dir_path = std::path::Path::new(&root_path).join("rename_encrypted");
        std::fs::create_dir_all(encrypted_dir_path.clone()).unwrap();

        let dir = std::fs::File::open(encrypted_dir_path.clone()).unwrap();
        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct_1 = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();

        let ret = set_encryption_policy(&dir, unsafe { arg_struct_1.key_spec.u.identifier.value });
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        let err =
            std::fs::rename(unencrypted_dir_path.join("file"), encrypted_dir_path.join("file"))
                .expect_err("renaming unencrypted file into encrypted dir should fail with EXDEV");
        assert_eq!(err.raw_os_error(), Some(libc::EXDEV));

        let ret =
            remove_encryption_key(&root_dir, unsafe { arg_struct_1.key_spec.u.identifier.value });
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::fs::remove_dir_all(unencrypted_dir_path).expect("failed to remove unencrypted dir");
        std::fs::remove_dir_all(encrypted_dir_path).expect("failed to remove encrypted dir");
    }

    #[test]
    #[serial]
    fn rename_encrypted_file_into_unencrypted_dir() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let unencrypted_dir_path = std::path::Path::new(&root_path).join("rename_unencrypted_2");
        std::fs::create_dir_all(unencrypted_dir_path.clone()).unwrap();

        let encrypted_dir_path = std::path::Path::new(&root_path).join("rename_encrypted_2");
        std::fs::create_dir_all(encrypted_dir_path.clone()).unwrap();

        let dir = std::fs::File::open(encrypted_dir_path.clone()).unwrap();
        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct_1 = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();

        let ret = set_encryption_policy(&dir, unsafe { arg_struct_1.key_spec.u.identifier.value });
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::fs::File::create_new(encrypted_dir_path.clone().join("file"))
            .expect("failed to create file in encrypted directory");

        let err =
            std::fs::rename(encrypted_dir_path.join("file"), unencrypted_dir_path.join("file"))
                .expect_err("renaming encrypted file into unencrypted dir should fail with EXDEV");
        assert_eq!(err.raw_os_error(), Some(libc::EXDEV));

        let ret =
            remove_encryption_key(&root_dir, unsafe { arg_struct_1.key_spec.u.identifier.value });
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::fs::remove_dir_all(unencrypted_dir_path).expect("failed to remove unencrypted dir");
        std::fs::remove_dir_all(encrypted_dir_path).expect("failed to remove encrypted dir");
    }

    #[test]
    #[ignore] // TODO(https://fxbug.dev/359885449) use expectations
    #[serial]
    // TODO(https://fxbug.dev/358420498) Purge the key manager on FS_IOC_REMOVE_ENCRYPTION_KEY
    fn hard_link_encrypted_file_into_second_dir_encrypted_with_same_policy() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let encrypted_1_dir_path = std::path::Path::new(&root_path).join("encrypted_1");
        std::fs::create_dir_all(encrypted_1_dir_path.clone()).unwrap();

        let encrypted_2_dir_path = std::path::Path::new(&root_path).join("encrypted_2");
        std::fs::create_dir_all(encrypted_2_dir_path.clone()).unwrap();

        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, arg_struct_raw_key) =
            arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct_1 = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();

        let dir_1 = std::fs::File::open(encrypted_1_dir_path.clone()).unwrap();
        let dir_2 = std::fs::File::open(encrypted_2_dir_path.clone()).unwrap();

        let ret =
            unsafe { set_encryption_policy(&dir_1, arg_struct_1.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        std::fs::File::create_new(encrypted_1_dir_path.clone().join("file"))
            .expect("failed to create file in unencrypted directory");

        let ret =
            unsafe { set_encryption_policy(&dir_2, arg_struct_1.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct_1.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        std::fs::hard_link(
            encrypted_1_dir_path.join("file"),
            encrypted_2_dir_path.clone().join("subdir"),
        )
        .expect_err(
            "hard linking between two directories encrypted with the same policy should fail if
                they are locked",
        );

        let (ret, _) = add_encryption_key_with_key_bytes(&root_dir, arg_struct_raw_key);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());

        std::fs::hard_link(
            encrypted_1_dir_path.join("file"),
            encrypted_2_dir_path.clone().join("subdir"),
        )
        .expect(
            "failed to hard link a file from an encrypted directory into another directory
                encrypted with the same policy",
        );

        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct_1.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::fs::remove_dir_all(encrypted_1_dir_path).expect("failed to remove unencrypted dir");
        std::fs::remove_dir_all(encrypted_2_dir_path).expect("failed to remove encrypted dir");
    }

    #[test]
    #[ignore] // TODO(https://fxbug.dev/359885449) use expectations
    #[serial]
    // TODO(https://fxbug.dev/358420498) Purge the key manager on FS_IOC_REMOVE_ENCRYPTION_KEY
    fn rename_in_locked_encrypted_directory_fails() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let encrypted_dir_path = std::path::Path::new(&root_path).join("rename_locked_encrypted");
        std::fs::create_dir_all(encrypted_dir_path.clone()).unwrap();

        let dir = std::fs::File::open(encrypted_dir_path.clone()).unwrap();
        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct_1 = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();

        let ret = set_encryption_policy(&dir, unsafe { arg_struct_1.key_spec.u.identifier.value });
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        std::fs::File::create_new(encrypted_dir_path.clone().join("file"))
            .expect("failed to create file in encrypted directory");

        let ret =
            remove_encryption_key(&root_dir, unsafe { arg_struct_1.key_spec.u.identifier.value });
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        let err = std::fs::rename(
            encrypted_dir_path.join("file"),
            encrypted_dir_path.join("file_renamed"),
        )
        .expect_err("renaming inside locked encrypted directory should fail with ENOKEY");
        assert_eq!(err.raw_os_error(), Some(libc::ENOKEY));

        std::fs::remove_dir_all(encrypted_dir_path).expect("failed to remove encrypted dir");
    }

    #[test]
    #[serial]
    fn hard_link_encrypted_file_into_second_dir_encrypted_with_different_policy() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let encrypted_1_dir_path = std::path::Path::new(&root_path).join("encrypted_1");
        std::fs::create_dir_all(encrypted_1_dir_path.clone()).unwrap();

        let encrypted_2_dir_path = std::path::Path::new(&root_path).join("encrypted_2");
        std::fs::create_dir_all(encrypted_2_dir_path.clone()).unwrap();

        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct_1 = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();

        let dir_1 = std::fs::File::open(encrypted_1_dir_path.clone()).unwrap();
        let dir_2 = std::fs::File::open(encrypted_2_dir_path.clone()).unwrap();

        let ret =
            unsafe { set_encryption_policy(&dir_1, arg_struct_1.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        std::fs::File::create_new(encrypted_1_dir_path.clone().join("file"))
            .expect("failed to create file in unencrypted directory");

        let ret = unsafe {
            set_encryption_policy_hctr2(&dir_2, arg_struct_1.key_spec.u.identifier.value)
        };
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        std::fs::hard_link(
            encrypted_1_dir_path.join("file"),
            encrypted_2_dir_path.clone().join("subdir"),
        )
        .expect(
            "hard linking a file in encrypted directory into a second encrypted directory with a
                different policy should fail",
        );

        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct_1.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::fs::remove_dir_all(encrypted_1_dir_path).expect("failed to remove unencrypted dir");
        std::fs::remove_dir_all(encrypted_2_dir_path).expect("failed to remove encrypted dir");
    }

    #[test]
    #[serial]
    fn set_encryption_policy_on_directory_encrypted_with_different_policy() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();

        let dir = std::fs::File::open(dir_path.clone()).unwrap();
        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct_1 = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();

        let ret = unsafe { set_encryption_policy(&dir, arg_struct_1.key_spec.u.identifier.value) };

        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct_2 = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();
        let ret = unsafe { set_encryption_policy(&dir, arg_struct_2.key_spec.u.identifier.value) };

        assert!(
            ret != 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        // Cleanup
        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct_1.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct_2.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::fs::remove_dir_all(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[serial]
    fn set_encryption_policy_on_directory_encrypted_with_same_policy() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let dir_path = std::path::Path::new(&root_path).join("same_policy_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();

        let dir = std::fs::File::open(dir_path.clone()).unwrap();
        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();

        let ret = unsafe { set_encryption_policy(&dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        // Setting the policy again on the same open directory must succeed with the same policy.
        let ret = unsafe { set_encryption_policy(&dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "re-setting same encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        // Reopen the directory and verify setting the policy again still succeeds.
        drop(dir);
        let reopened_dir = std::fs::File::open(dir_path.clone()).unwrap();
        let ret =
            unsafe { set_encryption_policy(&reopened_dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "setting same encryption policy on reopened dir failed: {:?}",
            std::io::Error::last_os_error()
        );

        // Cleanup
        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::fs::remove_dir_all(dir_path).expect("failed to remove same_policy_dir");
    }

    #[test]
    #[serial]
    fn set_encryption_policy_on_file() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();
        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();

        let file =
            std::fs::File::create_new(dir_path.join("file.txt")).expect("create file failed");
        let ret = unsafe { set_encryption_policy(&file, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret != 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::fs::remove_dir_all(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[serial]
    fn set_encryption_policy_on_non_empty_directory() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();
        let dir = std::fs::File::open(dir_path.clone()).unwrap();
        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();
        std::fs::create_dir(dir_path.join("subdir")).expect("create dir failed");
        let ret = unsafe { set_encryption_policy(&dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret != 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::fs::remove_dir_all(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[serial]
    fn set_encryption_key_on_directory_owned_by_different_user() {
        let Some(root_path) = get_root_path() else { return };
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();

        let child_binary_path = get_child_binary_path();

        std::os::unix::fs::chown(dir_path.clone(), Some(1000), Some(1000)).expect("chown failed");
        let output = std::process::Command::new(child_binary_path)
            .args(["--uid", "1000", "add"])
            .output()
            .expect("set encryption policy failed");
        let output_str = String::from_utf8_lossy(&output.stdout);
        let fscrypt_output: FscryptOutput = serde_json::from_str(&output_str).unwrap();
        assert!(output.status.success(), "{:#?}", output.status);

        let child_binary_path = get_child_binary_path();
        std::os::unix::fs::chown(dir_path.clone(), Some(2000), Some(2000)).expect("chown failed");
        let output = std::process::Command::new(child_binary_path)
            .args([
                "--uid",
                "1000",
                "set",
                "--should-fail",
                "true",
                "--identifier",
                &hex::encode(fscrypt_output.identifier),
            ])
            .output()
            .expect("set encryption policy failed");
        assert!(output.status.success(), "{:#?}", output.status);
        std::fs::remove_dir_all(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[serial]
    fn set_encryption_policy_with_encryption_key_added_by_different_user() {
        let Some(root_path) = get_root_path() else { return };
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();

        let child_binary_path = get_child_binary_path();

        std::os::unix::fs::chown(dir_path.clone(), Some(1000), Some(1000)).expect("chown failed");
        let output = std::process::Command::new(child_binary_path)
            .args(["--uid", "1000", "add"])
            .output()
            .expect("set encryption policy failed");
        let output_str = String::from_utf8_lossy(&output.stdout);
        let fscrypt_output: FscryptOutput = serde_json::from_str(&output_str).unwrap();
        assert!(output.status.success(), "{:#?}", output.status);

        std::os::unix::fs::chown(dir_path.clone(), Some(2000), Some(2000)).expect("chown failed");
        let child_binary_path = get_child_binary_path();
        let output = std::process::Command::new(child_binary_path)
            .args([
                "--uid",
                "2000",
                "set",
                "--should-fail",
                "true",
                "--identifier",
                &hex::encode(fscrypt_output.identifier),
            ])
            .output()
            .expect("set encryption policy failed");
        assert!(output.status.success(), "{:#?}", output.status);
        std::fs::remove_dir_all(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[ignore] // TODO(https://fxbug.dev/359885449) use expectations
    #[serial]
    // TODO(https://fxbug.dev/358420498) Purge the key manager on FS_IOC_REMOVE_ENCRYPTION_KEY
    fn stat_encrypted_symlink() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();
        let dir = std::fs::File::open(dir_path.clone()).unwrap();
        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();
        let ret = unsafe { set_encryption_policy(&dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        let target_path = dir_path.clone().join("foo.txt");
        let link = dir_path.clone().join("blah.txt");
        {
            let _ = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(&target_path)
                .unwrap();
        }
        std::os::unix::fs::symlink(target_path, &link).expect("failed to create symlink");
        drop(dir);
        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        let entries = std::fs::read_dir(dir_path.clone()).expect("readdir failed");
        for entry in entries {
            let entry = entry.expect("invalid entry");
            if entry.file_type().unwrap().is_symlink() {
                let encrypted_target_path =
                    std::fs::read_link(entry.path()).expect("read_link failed");
                let encrypted_target_len = encrypted_target_path.as_path().to_str().unwrap().len();
                let symlink_met = std::fs::symlink_metadata(entry.path())
                    .expect("failed to read metadata of symlink");
                assert_eq!(encrypted_target_len as u64, symlink_met.len());
                std::fs::metadata(encrypted_target_path)
                    .expect_err("there should be no existing directory at this gibberish path");
            }
        }
        std::fs::remove_dir_all(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[ignore] // TODO(https://fxbug.dev/359885449) use expectations
    #[serial]
    // TODO(https://fxbug.dev/358420498) Purge the key manager on FS_IOC_REMOVE_ENCRYPTION_KEY
    fn create_symlink_in_locked_directory_fails() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();
        let dir = std::fs::File::open(dir_path.clone()).unwrap();
        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();
        let ret = unsafe { set_encryption_policy(&dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        let target_path = dir_path.clone().join("foo.txt");
        let link = dir_path.clone().join("blah.txt");
        {
            let _ = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(&target_path)
                .unwrap();
        }

        drop(dir);
        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::os::unix::fs::symlink(target_path, &link)
            .expect_err("creating a symlink in locked directory should fail");
        std::fs::remove_dir_all(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[ignore] // TODO(https://fxbug.dev/359885449) use expectations
    #[serial]
    // TODO(https://fxbug.dev/358420498) Purge the key manager on FS_IOC_REMOVE_ENCRYPTION_KEY
    fn stat_locked_file() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();
        let dir = std::fs::File::open(dir_path.clone()).unwrap();
        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, arg_struct_raw_key) =
            arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();
        let ret = unsafe { set_encryption_policy(&dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        {
            let _ = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(dir_path.clone().join("foo.txt"))
                .unwrap();
        }
        drop(dir);

        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        let entries = std::fs::read_dir(dir_path.clone()).expect("readdir failed");
        let mut encrypted_file_name = None;
        for entry in entries {
            let entry = entry.expect("invalid entry");
            if entry.file_type().unwrap().is_file() {
                eprintln!("file name is {:?}", entry.file_name());
                encrypted_file_name = Some(entry.file_name());
                break;
            }
        }

        let _ = std::fs::metadata(dir_path.clone().join(encrypted_file_name.clone().unwrap()))
            .expect("metadata failed");

        let (ret, _) = add_encryption_key_with_key_bytes(&root_dir, arg_struct_raw_key);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());

        std::fs::metadata(dir_path.clone().join(encrypted_file_name.unwrap()))
            .expect_err("metadata should fail on an encrypted path for an unlocked directory");
        std::fs::remove_dir_all(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[ignore] // TODO(https://fxbug.dev/359885449) use expectations
    #[serial]
    // TODO(https://fxbug.dev/358420498) Purge the key manager on FS_IOC_REMOVE_ENCRYPTION_KEY
    fn one_user_adds_the_same_encryption_key_twice() {
        let Some(root_path) = get_root_path() else { return };
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();

        let child_binary_path = get_child_binary_path();
        let wrapping_key: [u8; 64] = [2; 64];
        let output = std::process::Command::new(child_binary_path)
            .args(["--uid", "1000", "add", "--key", &hex::encode(wrapping_key)])
            .output()
            .expect("set encryption policy failed");
        assert!(output.status.success(), "{:#?}", output.status);
        let child_binary_path = get_child_binary_path();

        let output = std::process::Command::new(child_binary_path)
            .args(["--uid", "1000", "add", "--key", &hex::encode(wrapping_key)])
            .output()
            .expect("set encryption policy failed");
        let output_str = String::from_utf8_lossy(&output.stdout);
        let fscrypt_output: FscryptOutput = serde_json::from_str(&output_str).unwrap();
        assert!(output.status.success(), "{:#?}", output.status);

        let child_binary_path = get_child_binary_path();
        let output = std::process::Command::new(child_binary_path)
            .args([
                "--uid",
                "1000",
                "remove",
                "--identifier",
                &hex::encode(fscrypt_output.identifier),
                "--should-fail",
                "false",
            ])
            .output()
            .expect("remove encryption key failed");
        assert!(output.status.success(), "{:#?}", output.status);
        std::fs::remove_dir_all(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[ignore] // TODO(https://fxbug.dev/359885449) use expectations
    #[serial]
    // TODO(https://fxbug.dev/358420498) Purge the key manager on FS_IOC_REMOVE_ENCRYPTION_KEY
    fn different_user_add_the_same_encryption_key() {
        let Some(root_path) = get_root_path() else { return };
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();
        let dir = std::fs::File::open(dir_path.clone()).unwrap();

        let child_binary_path = get_child_binary_path();
        let wrapping_key: [u8; 64] = [2; 64];
        let output = std::process::Command::new(child_binary_path)
            .args(["--uid", "1000", "add", "--key", &hex::encode(wrapping_key)])
            .output()
            .expect("set encryption policy failed");
        eprintln!("std err is {:?}", String::from_utf8_lossy(&output.stderr));
        assert!(output.status.success(), "{:#?}", output.status);
        let child_binary_path = get_child_binary_path();

        let output = std::process::Command::new(child_binary_path)
            .args(["--uid", "2000", "add", "--key", &hex::encode(wrapping_key)])
            .output()
            .expect("set encryption policy failed");
        let output_str = String::from_utf8_lossy(&output.stdout);
        let fscrypt_output: FscryptOutput = serde_json::from_str(&output_str).unwrap();
        assert!(output.status.success(), "{:#?}", output.status);

        let child_binary_path = get_child_binary_path();
        std::os::unix::fs::chown(dir_path.clone(), Some(1000), Some(1000)).expect("chown failed");
        let output = std::process::Command::new(child_binary_path)
            .args([
                "--uid",
                "1000",
                "set",
                "--should-fail",
                "false",
                "--identifier",
                &hex::encode(fscrypt_output.identifier),
            ])
            .output()
            .expect("set encryption policy failed");
        assert!(output.status.success(), "{:#?}", output.status);

        std::fs::create_dir(dir_path.join("subdir")).unwrap();
        drop(dir);

        let child_binary_path = get_child_binary_path();
        let output = std::process::Command::new(child_binary_path)
            .args([
                "--uid",
                "1000",
                "remove",
                "--identifier",
                &hex::encode(fscrypt_output.identifier),
                "--should-fail",
                "false",
            ])
            .output()
            .expect("remove encryption key failed");
        assert!(output.status.success(), "{:#?}", output.status);

        let entries = std::fs::read_dir(dir_path.clone()).expect("readdir failed");
        let mut count = 0;
        for entry in entries {
            let entry = entry.expect("invalid entry");
            assert!(entry.file_name() == "subdir");
            count += 1;
        }
        assert_eq!(count, 1);

        let child_binary_path = get_child_binary_path();
        let output = std::process::Command::new(child_binary_path)
            .args([
                "--uid",
                "2000",
                "remove",
                "--identifier",
                &hex::encode(fscrypt_output.identifier),
                "--should-fail",
                "false",
            ])
            .output()
            .expect("remove encryption key failed");
        assert!(output.status.success(), "{:#?}", output.status);
        let entries = std::fs::read_dir(dir_path.clone()).expect("readdir failed");
        let mut count = 0;
        for entry in entries {
            let entry = entry.expect("invalid entry");
            assert!(entry.file_name() != "subdir");
            count += 1;
        }
        assert_eq!(count, 1);
        std::fs::remove_dir_all(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[serial]
    fn root_sets_encryption_policy_on_a_directory_it_does_not_own() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();
        let dir = std::fs::File::open(dir_path.clone()).unwrap();

        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();

        std::os::unix::fs::chown(dir_path.clone(), Some(1000), Some(1000)).expect("chown failed");
        let ret = unsafe { set_encryption_policy(&dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        std::fs::remove_dir_all(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[ignore] // TODO(https://fxbug.dev/359885449) use expectations
    #[serial]
    // TODO(https://fxbug.dev/358420498) Purge the key manager on FS_IOC_REMOVE_ENCRYPTION_KEY
    fn unlink_locked_empty_encrypted_directory() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();
        let dir = std::fs::File::open(dir_path.clone()).unwrap();

        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();

        let ret = unsafe { set_encryption_policy(&dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        std::fs::create_dir(dir_path.join("subdir")).unwrap();
        drop(dir);

        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        let entries = std::fs::read_dir(dir_path.clone()).expect("readdir failed");
        let mut encrypted_dir_name = OsString::new();
        let mut count = 0;
        for entry in entries {
            let entry = entry.expect("invalid entry");
            encrypted_dir_name = entry.file_name();
            count += 1;
        }
        assert_eq!(count, 1);

        std::fs::remove_dir(dir_path.clone().join(encrypted_dir_name)).expect("remove dir failed");
        let entries = std::fs::read_dir(dir_path.clone()).expect("readdir failed");
        let mut count = 0;
        for _ in entries {
            count += 1;
        }
        assert_eq!(count, 0);
        std::fs::remove_dir(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[ignore] // TODO(https://fxbug.dev/359885449) use expectations
    #[serial]
    // TODO(https://fxbug.dev/358420498) Purge the key manager on FS_IOC_REMOVE_ENCRYPTION_KEY
    fn unlink_locked_encrypted_file() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let dir_path = std::path::Path::new(&root_path).join("my_dir");
        std::fs::create_dir_all(dir_path.clone()).unwrap();
        let dir = std::fs::File::open(dir_path.clone()).unwrap();

        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert!(ret == 0, "add encryption key ioctl failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();

        let ret = unsafe { set_encryption_policy(&dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "set encryption policy ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );
        {
            let _ = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(dir_path.clone().join("foo.txt"))
                .unwrap();
        }

        drop(dir);
        let ret =
            unsafe { remove_encryption_key(&root_dir, arg_struct.key_spec.u.identifier.value) };
        assert!(
            ret == 0,
            "remove encryption key ioctl failed: {:?}",
            std::io::Error::last_os_error()
        );

        let entries = std::fs::read_dir(dir_path.clone()).expect("readdir failed");
        let mut encrypted_file_name = OsString::new();
        let mut count = 0;
        for entry in entries {
            let entry = entry.expect("invalid entry");
            encrypted_file_name = entry.file_name();
            count += 1;
        }
        assert_eq!(count, 1);

        std::fs::remove_file(dir_path.clone().join(encrypted_file_name))
            .expect("remove file failed");
        let entries = std::fs::read_dir(dir_path.clone()).expect("readdir failed");
        let mut count = 0;
        for _ in entries {
            count += 1;
        }
        assert_eq!(count, 0);
        std::fs::remove_dir(dir_path).expect("failed to remove my_dir");
    }

    #[test]
    #[serial]
    fn get_encryption_policy_ex_on_locked_directory() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open failed");
        let dir_path = std::path::Path::new(&root_path).join("policy_ex_dir");
        std::fs::create_dir_all(&dir_path).unwrap();
        let dir = std::fs::File::open(&dir_path).unwrap();

        // Unencrypted directory returns ENODATA for both GET_ENCRYPTION_POLICY and
        // GET_ENCRYPTION_POLICY_EX.
        let mut v1_policy = fscrypt_policy_v1::default();
        let ret = unsafe {
            libc::ioctl(
                dir.as_raw_fd(),
                FS_IOC_GET_ENCRYPTION_POLICY.try_into().unwrap(),
                &mut v1_policy,
            )
        };
        assert_eq!(ret, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ENODATA));

        let mut ex_arg = fscrypt_get_policy_ex_arg {
            policy_size: std::mem::size_of::<fscrypt_policy_v2>() as u64,
            ..Default::default()
        };
        let ret = unsafe {
            libc::ioctl(
                dir.as_raw_fd(),
                FS_IOC_GET_ENCRYPTION_POLICY_EX.try_into().unwrap(),
                &mut ex_arg,
            )
        };
        assert_eq!(ret, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ENODATA));

        // Add key and set LBLK64 + PAD_16 policy.
        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert_eq!(ret, 0);
        let (arg_struct_bytes, _) = arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();
        let identifier = unsafe { arg_struct.key_spec.u.identifier.value };

        let expected_flags =
            (FSCRYPT_POLICY_FLAGS_PAD_16 | FSCRYPT_POLICY_FLAG_IV_INO_LBLK_64) as u8;
        let policy = fscrypt_policy_v2 {
            version: 2,
            contents_encryption_mode: FSCRYPT_MODE_AES_256_XTS as u8,
            filenames_encryption_mode: FSCRYPT_MODE_AES_256_CTS as u8,
            flags: expected_flags,
            master_key_identifier: identifier,
            ..Default::default()
        };
        let ret = unsafe {
            libc::ioctl(dir.as_raw_fd(), FS_IOC_SET_ENCRYPTION_POLICY.try_into().unwrap(), &policy)
        };
        assert_eq!(ret, 0);

        std::fs::create_dir(dir_path.join("subdir")).expect("failed to create subdir");
        drop(dir);

        // Lock the directory by removing the wrapping key.
        let ret = remove_encryption_key(&root_dir, identifier);
        assert_eq!(ret, 0);

        // Reopen the locked directory and verify FS_IOC_GET_ENCRYPTION_POLICY_EX returns the exact
        // policy, and returns EOVERFLOW when policy_size is smaller than sizeof(fscrypt_policy_v2).
        let locked_dir = std::fs::File::open(&dir_path).expect("open locked dir failed");
        let v2_size = std::mem::size_of::<fscrypt_policy_v2>() as u64;
        for small_size in [0, 1, 2, v2_size - 1] {
            let mut ex_arg =
                fscrypt_get_policy_ex_arg { policy_size: small_size, ..Default::default() };
            let ret = unsafe {
                libc::ioctl(
                    locked_dir.as_raw_fd(),
                    FS_IOC_GET_ENCRYPTION_POLICY_EX.try_into().unwrap(),
                    &mut ex_arg,
                )
            };
            assert_eq!(ret, -1, "expected EOVERFLOW for policy_size={small_size}");
            assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EOVERFLOW));
            assert_eq!(ex_arg.policy_size, small_size);
        }

        let mut ex_arg =
            fscrypt_get_policy_ex_arg { policy_size: v2_size + 16, ..Default::default() };
        let ret = unsafe {
            libc::ioctl(
                locked_dir.as_raw_fd(),
                FS_IOC_GET_ENCRYPTION_POLICY_EX.try_into().unwrap(),
                &mut ex_arg,
            )
        };
        assert_eq!(ret, 0, "GET_ENCRYPTION_POLICY_EX failed on locked dir");
        assert_eq!(ex_arg.policy_size, v2_size);
        let returned_v2 = unsafe { ex_arg.policy.v2 };
        assert_eq!(returned_v2.version, 2);
        assert_eq!(returned_v2.contents_encryption_mode, FSCRYPT_MODE_AES_256_XTS as u8);
        assert_eq!(returned_v2.filenames_encryption_mode, FSCRYPT_MODE_AES_256_CTS as u8);
        assert_eq!(returned_v2.flags, expected_flags);
        assert_eq!(returned_v2.master_key_identifier, identifier);

        std::fs::remove_dir_all(dir_path).expect("failed to remove policy_ex_dir");
    }

    #[test]
    #[serial]
    fn unlock_reopen_rename_across_nokey_and_plaintext_fds() {
        let Some(root_path) = get_root_path() else { return };

        let root_dir = std::fs::File::open(&root_path).expect("open root_path failed");
        let dir_path = PathBuf::from(&root_path).join("unlock_reopen_rename_dir");
        let _ = std::fs::remove_dir_all(&dir_path);
        std::fs::create_dir_all(&dir_path).expect("create_dir_all failed");
        std::fs::set_permissions(&dir_path, std::fs::Permissions::from_mode(0o1755))
            .expect("set sticky bit failed");

        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert_eq!(ret, 0, "add encryption key failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct, raw_key) = fscrypt_add_key_arg::read_from_prefix(&arg_vec).unwrap();
        // SAFETY: `add_encryption_key` sets `key_spec.type_` to `FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER`.
        let identifier = unsafe { arg_struct.key_spec.u.identifier.value };

        let dir = std::fs::File::open(&dir_path).expect("open dir failed");
        assert_eq!(set_encryption_policy(&dir, identifier), 0);
        drop(dir);

        let sub_path = dir_path.join("sub");
        std::fs::create_dir(&sub_path).expect("create sub failed");
        std::fs::write(sub_path.join("src"), "updated").expect("write src failed");
        std::fs::write(sub_path.join("dst"), "initial").expect("write dst failed");
        std::fs::create_dir(sub_path.join("child_dir")).expect("create child_dir failed");

        // Lock `dir_path`, drop caches so unwrapped keys are evicted, and open `sub` via its
        // ciphertext no-key name so Starnix allocates an uncached `DirEntry`.
        assert_eq!(remove_encryption_key(&root_dir, identifier), 0);
        std::fs::write("/proc/sys/vm/drop_caches", "2").expect("drop_caches failed");
        let mut nokey_entries = std::fs::read_dir(&dir_path).expect("read_dir while locked failed");
        let nokey_sub_entry = nokey_entries.next().unwrap().unwrap();
        assert!(nokey_entries.next().is_none());
        assert_ne!(nokey_sub_entry.file_name(), "sub");
        let nokey_sub_dir =
            std::fs::File::open(nokey_sub_entry.path()).expect("open no-key subdir failed");

        // Re-add the same wrapping key and open `sub` via its plaintext name so `nokey_sub_dir`
        // and `plain_sub_dir` hold distinct `DirEntry` handles for the same directory `FsNode`.
        assert_eq!(add_encryption_key_with_key_bytes(&root_dir, raw_key).0, 0);
        let plain_sub_dir = std::fs::File::open(&sub_path).expect("open plaintext subdir failed");
        let _ = std::fs::File::open(sub_path.join("src")).expect("open plaintext src failed");

        let nokey_sub_fd = PathBuf::from(format!("/proc/self/fd/{}", nokey_sub_dir.as_raw_fd()));
        let plain_sub_fd = PathBuf::from(format!("/proc/self/fd/{}", plain_sub_dir.as_raw_fd()));

        std::fs::rename(nokey_sub_fd.join("src"), plain_sub_fd.join("dst"))
            .expect("rename across no-key and plaintext directory fds failed");
        assert_eq!(std::fs::read_to_string(sub_path.join("dst")).unwrap(), "updated");
        assert!(!sub_path.join("src").exists());

        // Renaming across two hard links to the same file via the two handles is a no-op.
        std::fs::hard_link(sub_path.join("dst"), sub_path.join("dst_link")).unwrap();
        std::fs::rename(nokey_sub_fd.join("dst"), plain_sub_fd.join("dst_link")).unwrap();
        assert!(sub_path.join("dst").exists() && sub_path.join("dst_link").exists());

        // Moving `sub` into itself via `nokey_sub_dir` must fail with `EINVAL`, and moving a child
        // directory over its ancestor `sub` must fail with `ENOTEMPTY`.
        assert_eq!(
            std::fs::rename(&sub_path, nokey_sub_fd.join("into_self")).unwrap_err().kind(),
            ErrorKind::InvalidInput,
        );
        assert_eq!(
            std::fs::rename(nokey_sub_fd.join("child_dir"), &sub_path).unwrap_err().kind(),
            ErrorKind::DirectoryNotEmpty,
        );

        drop((plain_sub_dir, nokey_sub_dir));
        std::fs::remove_dir_all(&dir_path).expect("remove_dir_all failed");
        assert_eq!(remove_encryption_key(&root_dir, identifier), 0);
    }

    /// Regression test for b/577058351: when a directory is opened or bind-mounted via its
    /// fscrypt no-key name before the wrapping key is added, unlocking and accessing the
    /// directory via its plaintext name must splice onto the existing `DirEntry` rather than
    /// creating a disconnected second `DirEntry` tree.
    #[test]
    #[serial]
    fn bind_mount_locked_dir_and_unlock_single_tree() {
        let Some(root_path) = get_root_path() else { return };
        let root_dir = std::fs::File::open(&root_path).expect("open root failed");
        let base_dir = std::path::Path::new(&root_path).join("bind_splice_test");
        let _ = std::fs::remove_dir_all(&base_dir);
        let enc_dir_path = base_dir.join("enc");
        let mount_target = base_dir.join("bind_target");
        std::fs::create_dir_all(&enc_dir_path).unwrap();
        std::fs::create_dir_all(&mount_target).unwrap();

        let dst_c = std::ffi::CString::new(mount_target.as_os_str().as_bytes()).unwrap();
        struct Cleanup {
            root_dir: std::fs::File,
            key_identifier: Option<[u8; 16]>,
            mounted: bool,
            mount_target: std::ffi::CString,
            base_dir: std::path::PathBuf,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if self.mounted {
                    // SAFETY: `self.mount_target` is a valid NUL-terminated CString.
                    unsafe { libc::umount2(self.mount_target.as_ptr(), libc::MNT_DETACH) };
                }
                let _ = std::fs::remove_dir_all(&self.base_dir);
                if let Some(identifier) = self.key_identifier {
                    let _ = remove_encryption_key(&self.root_dir, identifier);
                }
            }
        }
        let mut cleanup = Cleanup {
            root_dir: root_dir.try_clone().expect("clone root_dir"),
            key_identifier: None,
            mounted: false,
            mount_target: dst_c.clone(),
            base_dir: base_dir.clone(),
        };

        let enc_dir = std::fs::File::open(&enc_dir_path).unwrap();
        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert_eq!(ret, 0, "add encryption key failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct_bytes, arg_struct_raw_key) =
            arg_vec.split_at(std::mem::size_of::<fscrypt_add_key_arg>());
        let arg_struct = fscrypt_add_key_arg::read_from_bytes(arg_struct_bytes).unwrap();
        // SAFETY: `add_encryption_key` initializes `key_spec.type_` to
        // `FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER`, which populates the `identifier` union variant.
        let identifier = unsafe { arg_struct.key_spec.u.identifier.value };
        cleanup.key_identifier = Some(identifier);
        assert_eq!(set_encryption_policy(&enc_dir, identifier), 0);

        let plaintext_subdir = enc_dir_path.join("com.example.app");
        std::fs::create_dir(&plaintext_subdir).expect("create com.example.app");
        drop(enc_dir);

        // Lock the encrypted directory by removing the wrapping key and dropping caches so the
        // subsequent lookup resolves the directory via its fscrypt no-key name.
        assert_eq!(remove_encryption_key(&root_dir, identifier), 0);
        cleanup.key_identifier = None;
        std::fs::write("/proc/sys/vm/drop_caches", b"2").expect("drop_caches");

        // Locate the no-key directory entry, pin it via an open fd, and bind-mount it (matching
        // Direct Boot app data isolation) so the NO_KEY DirEntry outlives unlock.
        let mut no_key_name = None;
        for entry in std::fs::read_dir(&enc_dir_path).expect("readdir locked enc dir") {
            let entry = entry.expect("entry");
            if entry.file_type().expect("file_type").is_dir() {
                no_key_name = Some(entry.file_name());
                break;
            }
        }
        let no_key_name = no_key_name.expect("missing encrypted subdir");
        assert_ne!(no_key_name, OsString::from("com.example.app"));
        let no_key_path = enc_dir_path.join(&no_key_name);
        let no_key_fd = std::fs::File::open(&no_key_path).expect("open no-key subdir");

        let src_c = std::ffi::CString::new(no_key_path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `src_c` and `dst_c` are valid NUL-terminated CStrings and `MS_BIND` ignores
        // `filesystemtype` and `data` (both passed as null).
        let mount_ret = unsafe {
            libc::mount(
                src_c.as_ptr(),
                dst_c.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            )
        };
        assert_eq!(mount_ret, 0, "bind mount failed: {:?}", std::io::Error::last_os_error());
        cleanup.mounted = true;

        // Re-add the wrapping key (unlock).
        let (ret, _) = add_encryption_key_with_key_bytes(&root_dir, arg_struct_raw_key);
        assert_eq!(ret, 0, "unlock failed: {:?}", std::io::Error::last_os_error());
        cleanup.key_identifier = Some(identifier);

        // Operations through the plaintext tree and the Direct-Boot bind mount must share one
        // directory cache and serialization: create via plaintext, unlink via bind mount, then
        // re-create with O_CREAT|O_EXCL via plaintext (which would fail with stale EEXIST across
        // split DirEntry trees), and vice versa.
        let via_plaintext = plaintext_subdir.join("shared.xml");
        let via_bind = mount_target.join("shared.xml");
        std::fs::write(&via_plaintext, b"v1").expect("create via plaintext");
        assert_eq!(std::fs::read(&via_bind).expect("read via bind"), b"v1");
        std::fs::remove_file(&via_bind).expect("unlink via bind mount");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&via_plaintext)
            .expect("create_new via plaintext after unlink via bind mount should succeed");
        std::fs::remove_file(&via_plaintext).expect("unlink via plaintext");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&via_bind)
            .expect("create_new via bind mount after unlink via plaintext should succeed");
        std::fs::remove_file(&via_bind).expect("cleanup shared.xml");

        // After unlock + plaintext access, /proc/self/fd/<no_key_fd> reflects the plaintext
        // directory name rather than remaining stuck on the no-key name.
        let fd_link = std::fs::read_link(format!("/proc/self/fd/{}", no_key_fd.as_raw_fd()))
            .expect("readlink /proc/self/fd");
        assert!(
            fd_link.ends_with("com.example.app"),
            "expected fd path to resolve to com.example.app, got {fd_link:?}"
        );

        drop(no_key_fd);
    }

    #[test]
    #[serial]
    fn rename_nokey_name_and_concurrent_unlock() {
        let Some(root_path) = get_root_path() else { return };

        let root_dir = std::fs::File::open(&root_path).expect("open root_path failed");
        let unenc_dir = PathBuf::from(&root_path).join("nokey_rename_unenc");
        let enc_dir = PathBuf::from(&root_path).join("nokey_rename_enc");
        let _ = std::fs::remove_dir_all(&unenc_dir);
        let _ = std::fs::remove_dir_all(&enc_dir);
        std::fs::create_dir_all(&unenc_dir).expect("create unenc_dir failed");
        std::fs::create_dir_all(&enc_dir).expect("create enc_dir failed");

        let unenc_src = unenc_dir.join("src");
        std::fs::write(&unenc_src, "unenc").expect("write unenc_src failed");

        let (ret, arg_vec) = add_encryption_key(&root_dir);
        assert_eq!(ret, 0, "add encryption key failed: {:?}", std::io::Error::last_os_error());
        let (arg_struct, raw_key) = fscrypt_add_key_arg::read_from_prefix(&arg_vec).unwrap();
        // SAFETY: `add_encryption_key` sets `key_spec.type_` to `FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER`.
        let identifier = unsafe { arg_struct.key_spec.u.identifier.value };

        let dir = std::fs::File::open(&enc_dir).expect("open enc_dir failed");
        assert_eq!(set_encryption_policy(&dir, identifier), 0);
        drop(dir);

        let plain_a = enc_dir.join("a");
        let plain_b = enc_dir.join("b");
        std::fs::write(&plain_a, "file_a").expect("write plain_a failed");
        std::fs::write(&plain_b, "file_b").expect("write plain_b failed");
        let ino_a = std::fs::metadata(&plain_a).expect("stat plain_a failed").ino();
        let ino_b = std::fs::metadata(&plain_b).expect("stat plain_b failed").ino();

        // Lock `enc_dir` and map each file's inode number to its ciphertext no-key path.
        assert_eq!(remove_encryption_key(&root_dir, identifier), 0);
        std::fs::write("/proc/sys/vm/drop_caches", "2").expect("drop_caches failed");

        let mut nokey_a = None;
        let mut nokey_b = None;
        for entry in std::fs::read_dir(&enc_dir).expect("read_dir while locked failed") {
            let entry = entry.expect("dir entry failed");
            let path = entry.path();
            let ino = std::fs::metadata(&path).expect("stat nokey entry failed").ino();
            if ino == ino_a {
                nokey_a = Some(path);
            } else if ino == ino_b {
                nokey_b = Some(path);
            }
        }
        let nokey_a = nokey_a.expect("missing ciphertext entry for a");
        let nokey_b = nokey_b.expect("missing ciphertext entry for b");

        // While locked, renames involving ciphertext no-key names fail with `ENOKEY`.
        assert_eq!(
            std::fs::rename(&nokey_a, enc_dir.join("new_name")).unwrap_err().raw_os_error(),
            Some(libc::ENOKEY),
        );
        assert_eq!(
            std::fs::rename(&nokey_a, &nokey_b).unwrap_err().raw_os_error(),
            Some(libc::ENOKEY)
        );
        assert_eq!(
            std::fs::rename(&unenc_src, &nokey_b).unwrap_err().raw_os_error(),
            Some(libc::ENOKEY),
        );

        // Once unlocked before lookup, stale ciphertext source/unlink paths fail with `ENOENT`,
        // while renaming an unencrypted file to a ciphertext name in the unlocked directory treats
        // the destination as a missing plaintext name and fails with `EXDEV`.
        assert_eq!(add_encryption_key_with_key_bytes(&root_dir, raw_key).0, 0);
        assert_eq!(std::fs::remove_file(&nokey_a).unwrap_err().raw_os_error(), Some(libc::ENOENT));
        assert_eq!(
            std::fs::rename(&nokey_a, enc_dir.join("new_name")).unwrap_err().raw_os_error(),
            Some(libc::ENOENT),
        );
        assert_eq!(
            std::fs::rename(&nokey_a, &nokey_b).unwrap_err().raw_os_error(),
            Some(libc::ENOENT)
        );
        assert_eq!(
            std::fs::rename(&unenc_src, &nokey_b).unwrap_err().raw_os_error(),
            Some(libc::EXDEV),
        );
        assert!(plain_a.exists() && plain_b.exists());

        struct SetOnDrop<'a>(&'a AtomicBool);
        impl Drop for SetOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        // Race `rename(&unenc_src, &nokey_b)` (into an existing file in `enc_dir`) against a
        // concurrent `FS_IOC_ADD_ENCRYPTION_KEY`: if `nokey_b` is looked up while `enc_dir` is
        // locked, the rename must fail with `ENOKEY` (never `ENOENT`), transitioning to `EXDEV`
        // once `enc_dir` is unlocked before target lookup.
        for round in 0..32usize {
            assert_eq!(remove_encryption_key(&root_dir, identifier), 0);
            std::fs::write("/proc/sys/vm/drop_caches", "2").expect("drop_caches failed");

            let renamer_hot = AtomicBool::new(false);
            let unlocked = AtomicBool::new(false);
            let stop = AtomicBool::new(false);
            let unlocker_done = AtomicBool::new(false);
            std::thread::scope(|s| {
                s.spawn(|| {
                    let _done_on_drop = SetOnDrop(&unlocker_done);
                    while !renamer_hot.load(Ordering::Acquire) {
                        if stop.load(Ordering::Acquire) {
                            return;
                        }
                        std::thread::yield_now();
                    }
                    let delay = Duration::from_micros((round as u64 % 16) * 20);
                    let start = Instant::now();
                    while start.elapsed() < delay && !stop.load(Ordering::Acquire) {
                        std::hint::spin_loop();
                    }
                    assert_eq!(add_encryption_key_with_key_bytes(&root_dir, raw_key).0, 0);
                    unlocked.store(true, Ordering::Release);
                });

                let _stop_on_drop = SetOnDrop(&stop);
                let warmup_calls = (round % 4) + 1;
                let mut calls = 0usize;
                loop {
                    let was_unlocked = unlocked.load(Ordering::Acquire);
                    let done_without_unlock =
                        unlocker_done.load(Ordering::Acquire) && !unlocked.load(Ordering::Acquire);
                    let err = std::fs::rename(&unenc_src, &nokey_b)
                        .expect_err("cross-policy rename must fail");
                    let code = err.raw_os_error();
                    assert!(
                        code == Some(libc::ENOKEY) || code == Some(libc::EXDEV),
                        "round {round}: expected ENOKEY or EXDEV during concurrent unlock, got {err:?}",
                    );
                    calls += 1;
                    if calls == warmup_calls {
                        renamer_hot.store(true, Ordering::Release);
                    }
                    if (was_unlocked && code == Some(libc::EXDEV)) || done_without_unlock {
                        break;
                    }
                }
            });
        }

        assert_eq!(std::fs::read_to_string(&plain_a).unwrap(), "file_a");
        assert_eq!(std::fs::read_to_string(&plain_b).unwrap(), "file_b");
        assert_eq!(std::fs::read_to_string(&unenc_src).unwrap(), "unenc");

        std::fs::remove_dir_all(&unenc_dir).expect("remove unenc_dir failed");
        std::fs::remove_dir_all(&enc_dir).expect("remove enc_dir failed");
        assert_eq!(remove_encryption_key(&root_dir, identifier), 0);
    }
}
