// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_hash::Hash;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Eq, PartialEq, Default)]
pub struct PackageSizeInfo {
    pub name: String,
    /// Space used by this package in blobfs if each blob is counted fully.
    pub used_space_in_blobfs: u64,
    /// Size of the package in blobfs if each blob is divided equally among all the packages that reference it.
    pub proportional_size: u64,
    /// Blobs in this package and information about their size.
    pub blobs: Vec<PackageBlobSizeInfo>,
}

#[derive(Debug, Serialize, Eq, PartialEq, Clone)]
pub struct PackageBlobSizeInfo {
    pub merkle: Hash,
    pub path_in_package: String,
    /// Space used by this blob in blobfs
    pub used_space_in_blobfs: u64,
    /// Number of packages that contain this blob.
    pub share_count: u64,
    /// Count of all occurrences of the blob within and across all packages.
    pub absolute_share_count: u64,
}

pub fn wrap_text(indent: usize, length: usize, path: &String) -> String {
    if path.len() <= length {
        return path.clone();
    }

    let mut wrapped = String::new();

    // Add the first line.
    let first_length = length - 3;
    wrapped += &format!("{}...\n", &path[..first_length]);

    // Add the following lines.
    let following_length = length - 6;
    let following_chars = path[first_length..].chars().collect::<Vec<char>>();
    let following = following_chars
        .chunks(following_length)
        .map(|c| c.iter().collect::<String>())
        .map(|s| format!("{:indent$}   {}", "", s, indent = indent))
        .collect::<Vec<String>>();
    wrapped += &following.join("...\n");

    // Add any remaining spaces to line back up to length.
    let remaining = length + indent - following.last().map(|s| s.len()).unwrap_or(0);
    wrapped += &format!("{:r$}", "", r = remaining);
    wrapped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_text() {
        let path = "abcdefghijklmnopqrstuvwxyz".to_string();
        let expected_path = r"abcdefg...
      hijk...
      lmno...
      pqrs...
      tuvw...
      xyz    ";
        assert_eq!(expected_path, wrap_text(3, 10, &path));
    }
}
