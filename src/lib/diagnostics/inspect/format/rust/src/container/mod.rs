// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(target_os = "fuchsia")]
mod fuchsia;
#[cfg(target_os = "fuchsia")]
use self::fuchsia as implementation;

#[cfg(not(target_os = "fuchsia"))]
mod portable;
#[cfg(not(target_os = "fuchsia"))]
use self::portable as implementation;

mod common;

pub use common::*;
pub use implementation::Container;

#[cfg(test)]
mod tests {
    use super::{BlockContainer, Container};
    use crate::constants;

    #[::fuchsia::test]
    fn read_only_with_max_size_respects_optional_max_size() {
        // Test clamping vs. unclamped behavior using small multiples of `PAGE_SIZE_BYTES` rather
        // than `MAX_VMO_SIZE` so host tests do not allocate >128 MiB of memory at test time.
        let size = 2 * constants::PAGE_SIZE_BYTES;
        let max_size = constants::PAGE_SIZE_BYTES;
        #[cfg(target_os = "fuchsia")]
        let (clamped, unclamped) = {
            let vmo = zx::Vmo::create(size as u64).unwrap();
            assert_eq!(
                Container::read_only_with_max_size(&vmo, Some(0)).unwrap_err(),
                zx::Status::INVALID_ARGS
            );
            assert_eq!(
                Container::read_only_with_max_size(&vmo, Some(max_size + 1)).unwrap_err(),
                zx::Status::INVALID_ARGS
            );
            (
                Container::read_only_with_max_size(&vmo, Some(max_size)).unwrap(),
                Container::read_only_with_max_size(&vmo, None).unwrap(),
            )
        };
        #[cfg(not(target_os = "fuchsia"))]
        let (clamped, unclamped) = {
            let buffer = vec![0u8; size];
            (
                Container::read_only_with_max_size(&buffer, Some(max_size)),
                Container::read_only_with_max_size(&buffer, None),
            )
        };
        assert_eq!(clamped.len(), max_size);
        assert_eq!(unclamped.len(), size);
    }
}
