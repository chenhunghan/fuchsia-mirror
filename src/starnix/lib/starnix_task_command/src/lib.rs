// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![warn(missing_docs)]

//! The `TaskCommand` type and associated functions.

use flyweights::FlyByteStr;
use fuchsia_rcu::RcuDroppable;
use std::ops::Range;

/// The command for a task.
///
/// Linux task commands are limited to 15 bytes, but Fuchsia allows longer names in places. It's
/// useful to store longer names diagnostics and debugging information.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct TaskCommand {
    name: FlyByteStr,
    linux_name_range: Option<Range<usize>>,
}

// SAFETY: TaskCommand contains FlyByteStr (interned string without drop side effects) and Option<Range<usize>>.
unsafe impl RcuDroppable for TaskCommand {}

impl TaskCommand {
    fn truncate_at_nul(bytes: &[u8]) -> &[u8] {
        if let Some(idx) = memchr::memchr(b'\0', bytes) { &bytes[..idx] } else { bytes }
    }

    fn path_basename(path: &[u8]) -> &[u8] {
        let path = Self::truncate_at_nul(path);
        if let Some(idx) = memchr::memrchr(b'/', path) { &path[idx + 1..] } else { path }
    }

    /// Create a new `TaskCommand` from a byte slice. The byte slice is truncated at the first null
    /// byte if any.
    pub fn new(name: &[u8]) -> Self {
        Self { name: FlyByteStr::new(Self::truncate_at_nul(name)), linux_name_range: None }
    }

    /// Create a new `TaskCommand` from a path. The basename of the path is used as the name.
    pub fn from_path_bytes(path: &[u8]) -> Self {
        Self { name: FlyByteStr::new(Self::path_basename(path)), linux_name_range: None }
    }

    /// Returns the name truncated to 15 bytes.
    pub fn comm_name(&self) -> &[u8] {
        let bytes = self.linux_name_bytes();
        &bytes[..std::cmp::min(bytes.len(), 15)]
    }

    /// Returns the name as a 16-byte array, null-terminated if shorter than 16 bytes,
    /// as expected by `prctl(PR_GET_NAME)`.
    pub fn prctl_name(&self) -> [u8; 16] {
        let mut name = [0u8; 16];
        let comm = self.comm_name();
        name[..comm.len()].copy_from_slice(comm);
        name
    }

    /// Returns the entire name as a byte slice.
    pub fn as_bytes(&self) -> &[u8] {
        self.name.as_bytes()
    }

    /// Returns the Linux name as a byte slice, without truncation.
    fn linux_name_bytes(&self) -> &[u8] {
        if let Some(range) = &self.linux_name_range {
            &self.name.as_bytes()[range.clone()]
        } else {
            self.name.as_bytes()
        }
    }

    /// Returns a `TaskCommand` extended with the basename of `argv0` if this command's Linux name
    /// is a substring of it, or `self` unchanged otherwise (to avoid clobbering a custom name that
    /// is unrelated to `argv[0]`).
    pub fn with_argv0(self, argv0: &[u8]) -> Self {
        use bstr::ByteSlice;
        let argv0 = Self::path_basename(argv0);
        if argv0 == self.as_bytes() {
            return self;
        }
        let linux_name = self.linux_name_bytes();
        match argv0.find(linux_name) {
            Some(offset) => Self {
                name: FlyByteStr::new(argv0),
                linux_name_range: Some(offset..offset + linux_name.len()),
            },
            None => self,
        }
    }
}

impl Default for TaskCommand {
    fn default() -> Self {
        Self::new(b"")
    }
}

impl std::fmt::Debug for TaskCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.name.fmt(f)
    }
}

impl std::fmt::Display for TaskCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.name.fmt(f)
    }
}

impl PartialOrd for TaskCommand {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TaskCommand {
    /// This comparison ignores the linux rendering of the name and provides a total ordering
    /// based on the full name.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

impl Into<FlyByteStr> for TaskCommand {
    fn into(self) -> FlyByteStr {
        self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        assert_eq!(TaskCommand::new(b"foo").as_bytes(), b"foo");
        assert_eq!(TaskCommand::new(b"foo\0bar").as_bytes(), b"foo");
    }

    #[test]
    fn test_from_path_bytes() {
        assert_eq!(TaskCommand::from_path_bytes(b"/foo/bar").as_bytes(), b"bar");
        assert_eq!(TaskCommand::from_path_bytes(b"bar").as_bytes(), b"bar");
        assert_eq!(TaskCommand::from_path_bytes(b"/bar").as_bytes(), b"bar");
    }

    #[test]
    fn test_comm_name() {
        assert_eq!(TaskCommand::new(b"short").comm_name(), b"short");
        assert_eq!(TaskCommand::new(b"0123456789abcdef").comm_name(), b"0123456789abcde");
        assert_eq!(TaskCommand::new(b"0123456789abcdefg").comm_name(), b"0123456789abcde");
    }

    #[test]
    fn test_prctl_name() {
        assert_eq!(TaskCommand::new(b"short").prctl_name(), *b"short\0\0\0\0\0\0\0\0\0\0\0");
        assert_eq!(TaskCommand::new(b"0123456789abcdef").prctl_name(), *b"0123456789abcde\0");
        assert_eq!(TaskCommand::new(b"0123456789abcdefg").prctl_name(), *b"0123456789abcde\0");
    }

    #[test]
    fn test_prctl_name_16_bytes() {
        let name = b"0123456789abcdef"; // 16 bytes
        assert_eq!(TaskCommand::new(name).prctl_name(), *b"0123456789abcde\0");
        assert_eq!(TaskCommand::new(name).comm_name(), b"0123456789abcde"); // 15 bytes
    }

    #[test]
    fn test_debug() {
        assert_eq!(format!("{:?}", TaskCommand::new(b"foo")), "\"foo\"");
    }

    #[test]
    fn test_display() {
        assert_eq!(TaskCommand::new(b"foo").to_string(), "foo");
    }

    #[test]
    fn test_sniffing() {
        let embedded = TaskCommand::new(b"binary").with_argv0(b"/path/to/my_binary");
        assert_eq!(embedded.as_bytes(), b"my_binary");
        assert_eq!(embedded.comm_name(), b"binary");

        let other = TaskCommand::new(b"other").with_argv0(b"/path/to/my_binary");
        assert_eq!(other.as_bytes(), b"other");
        assert_eq!(other.comm_name(), b"other");
    }

    #[test]
    fn test_comm_name_sniffed() {
        let embedded =
            TaskCommand::new(b"short_name").with_argv0(b"/path/to/short_name_with_suffix");
        // comm_name should be "short_name" (len 10), not truncated version of full basename
        assert_eq!(embedded.as_bytes(), b"short_name_with_suffix");
        assert_eq!(embedded.comm_name(), b"short_name");
    }

    #[test]
    fn test_with_argv0() {
        let cmd = TaskCommand::from_path_bytes(b"/bin/my_long_command_name")
            .with_argv0(b"/custom/my_long_command_name_with_suffix");
        assert_eq!(cmd.as_bytes(), b"my_long_command_name_with_suffix");
        assert_eq!(cmd.comm_name(), b"my_long_command");

        let same = TaskCommand::from_path_bytes(b"/bin/my_long_command_name")
            .with_argv0(b"/bin/my_long_command_name");
        assert_eq!(same.as_bytes(), b"my_long_command_name");
        assert_eq!(same.comm_name(), b"my_long_command");

        let unrelated = TaskCommand::new(b"custom_name").with_argv0(b"/bin/my_long_command_name");
        assert_eq!(unrelated.as_bytes(), b"custom_name");
        assert_eq!(unrelated.comm_name(), b"custom_name");
    }
}
