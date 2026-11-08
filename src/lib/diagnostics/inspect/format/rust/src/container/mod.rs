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
    fn read_only_clamps_to_max_vmo_size() {
        let size = 2 * constants::MAX_VMO_SIZE;
        #[cfg(target_os = "fuchsia")]
        let container = {
            let vmo = zx::Vmo::create(size as u64).unwrap();
            Container::read_only(&vmo).unwrap()
        };
        #[cfg(not(target_os = "fuchsia"))]
        let container = {
            let buffer = vec![0u8; size];
            Container::read_only(&buffer)
        };
        assert_eq!(container.len(), constants::MAX_VMO_SIZE);
    }
}
