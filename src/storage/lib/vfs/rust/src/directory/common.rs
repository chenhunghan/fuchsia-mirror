// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Common utilities used by several directory implementations.

#[cfg(any(fuchsia_api_level_at_least = "PLATFORM", not(fuchsia_api_level_at_least = "32")))]
use crate::common::stricter_or_same_rights;
use crate::directory::entry::EntryInfo;

use flex_fuchsia_io as fio;
use static_assertions::assert_eq_size;
use std::mem::size_of;
#[cfg(any(fuchsia_api_level_at_least = "PLATFORM", not(fuchsia_api_level_at_least = "32")))]
use zx_status::Status;

/// Directories need to make sure that connections to child entries do not receive more rights than
/// the connection to the directory itself.  Plus there is special handling of the OPEN_FLAG_POSIX_*
/// flags. This function should be called before calling [`new_connection_validate_flags`] if both
/// are needed.
#[cfg(any(fuchsia_api_level_at_least = "PLATFORM", not(fuchsia_api_level_at_least = "32")))]
pub(crate) fn check_child_connection_flags(
    parent_flags: fio::OpenFlags,
    mut flags: fio::OpenFlags,
) -> Result<fio::OpenFlags, Status> {
    if flags & (fio::OpenFlags::NOT_DIRECTORY | fio::OpenFlags::DIRECTORY)
        == fio::OpenFlags::NOT_DIRECTORY | fio::OpenFlags::DIRECTORY
    {
        return Err(Status::INVALID_ARGS);
    }

    // Can only specify OPEN_FLAG_CREATE_IF_ABSENT if OPEN_FLAG_CREATE is also specified.
    if flags.intersects(fio::OpenFlags::CREATE_IF_ABSENT)
        && !flags.intersects(fio::OpenFlags::CREATE)
    {
        return Err(Status::INVALID_ARGS);
    }

    // Can only use CLONE_FLAG_SAME_RIGHTS when calling Clone.
    if flags.intersects(fio::OpenFlags::CLONE_SAME_RIGHTS) {
        return Err(Status::INVALID_ARGS);
    }

    // Remove POSIX flags when the respective rights are not available ("soft fail").
    if !parent_flags.intersects(fio::OpenFlags::RIGHT_EXECUTABLE) {
        flags &= !fio::OpenFlags::POSIX_EXECUTABLE;
    }
    if !parent_flags.intersects(fio::OpenFlags::RIGHT_WRITABLE) {
        flags &= !fio::OpenFlags::POSIX_WRITABLE;
    }

    // Can only use CREATE flags if the parent connection is writable.
    if flags.intersects(fio::OpenFlags::CREATE)
        && !parent_flags.intersects(fio::OpenFlags::RIGHT_WRITABLE)
    {
        return Err(Status::ACCESS_DENIED);
    }

    if stricter_or_same_rights(parent_flags, flags) {
        Ok(flags)
    } else {
        Err(Status::ACCESS_DENIED)
    }
}

/// A helper to generate binary encodings for the ReadDirents response.  This function will append
/// an entry description as specified by `entry` and `name` to the `buf`, and would return `true`.
/// In case this would cause the buffer size to exceed `max_bytes`, the buffer is then left
/// untouched and a `false` value is returned.
pub(crate) fn encode_dirent(
    buf: &mut Vec<u8>,
    max_bytes: u64,
    entry: &EntryInfo,
    name: &str,
) -> bool {
    const HEADER_SIZE: usize = size_of::<u64>() + size_of::<u8>() + size_of::<u8>();

    assert_eq_size!(u64, usize);

    if buf.len() + HEADER_SIZE + name.len() > max_bytes as usize {
        return false;
    }

    // TODO(https://fxbug.dev/293948129): `Sink` implementations should take a type that enforces
    // this constraint. "." is valid here, so taking [`crate::Name`] directly isn't sufficient.
    assert!(
        name.len() <= fio::MAX_NAME_LENGTH as usize,
        "Entry names are expected to be no longer than MAX_FILENAME ({}) bytes.\n\
         Got entry: '{}'\n\
         Length: {} bytes",
        fio::MAX_NAME_LENGTH,
        name,
        name.len()
    );

    assert!(
        fio::MAX_NAME_LENGTH <= u8::MAX as u64,
        "Expecting to be able to store MAX_FILENAME ({}) in one byte.",
        fio::MAX_NAME_LENGTH
    );
    buf.reserve(HEADER_SIZE + name.len());
    buf.extend_from_slice(&entry.inode().to_le_bytes());
    buf.push(name.len() as u8);
    buf.push(entry.type_().into_primitive());
    buf.extend_from_slice(name.as_bytes());

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(fuchsia_api_level_at_least = "PLATFORM", not(fuchsia_api_level_at_least = "32")))]
    #[fuchsia::test]
    fn test_check_child_connection_flags() {
        let rw_parent = fio::OpenFlags::RIGHT_READABLE | fio::OpenFlags::RIGHT_WRITABLE;
        let ro_parent = fio::OpenFlags::RIGHT_READABLE;

        // Conflicting DIRECTORY and NOT_DIRECTORY flags.
        assert_eq!(
            check_child_connection_flags(
                rw_parent,
                fio::OpenFlags::DIRECTORY | fio::OpenFlags::NOT_DIRECTORY
            ),
            Err(Status::INVALID_ARGS)
        );

        // CREATE_IF_ABSENT without CREATE.
        assert_eq!(
            check_child_connection_flags(rw_parent, fio::OpenFlags::CREATE_IF_ABSENT),
            Err(Status::INVALID_ARGS)
        );

        // CLONE_SAME_RIGHTS is not allowed on Open.
        assert_eq!(
            check_child_connection_flags(rw_parent, fio::OpenFlags::CLONE_SAME_RIGHTS),
            Err(Status::INVALID_ARGS)
        );

        // POSIX flags stripped when parent lacks corresponding rights.
        assert_eq!(
            check_child_connection_flags(
                ro_parent,
                fio::OpenFlags::RIGHT_READABLE
                    | fio::OpenFlags::POSIX_WRITABLE
                    | fio::OpenFlags::POSIX_EXECUTABLE
            ),
            Ok(fio::OpenFlags::RIGHT_READABLE)
        );

        // CREATE fails when parent is not writable.
        assert_eq!(
            check_child_connection_flags(ro_parent, fio::OpenFlags::CREATE),
            Err(Status::ACCESS_DENIED)
        );

        // Child requesting more rights than parent fails with ACCESS_DENIED.
        assert_eq!(
            check_child_connection_flags(ro_parent, fio::OpenFlags::RIGHT_WRITABLE),
            Err(Status::ACCESS_DENIED)
        );

        // Valid CREATE with writable parent succeeds.
        assert_eq!(
            check_child_connection_flags(
                rw_parent,
                fio::OpenFlags::CREATE
                    | fio::OpenFlags::CREATE_IF_ABSENT
                    | fio::OpenFlags::RIGHT_WRITABLE
            ),
            Ok(fio::OpenFlags::CREATE
                | fio::OpenFlags::CREATE_IF_ABSENT
                | fio::OpenFlags::RIGHT_WRITABLE)
        );
    }

    #[fuchsia::test]
    fn test_encode_dirent() {
        let entry = EntryInfo::new(42, fio::DirentType::File);
        let mut buf = Vec::new();

        // Buffer too small returns false and leaves buf empty.
        assert!(!encode_dirent(&mut buf, 10, &entry, "hello"));
        assert!(buf.is_empty());

        // Exact size (10 header bytes + 5 name bytes = 15) succeeds.
        assert!(encode_dirent(&mut buf, 15, &entry, "hello"));
        assert_eq!(buf.len(), 15);
        assert_eq!(&buf[0..8], &42u64.to_le_bytes());
        assert_eq!(buf[8], 5);
        assert_eq!(buf[9], fio::DirentType::File.into_primitive());
        assert_eq!(&buf[10..15], b"hello");
    }
}
