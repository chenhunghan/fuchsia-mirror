// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{BlockContainer, CopyBytes, ReadBytes, WriteBytes, constants};

#[derive(Debug)]
pub struct Container {
    buffer_addr: usize,
    vmo_size: usize,
}

impl Container {
    pub fn read_and_write(size: usize) -> Result<(Self, zx::Vmo), zx::Status> {
        let vmo = zx::Vmo::create(size as u64)?;
        let flags = zx::VmarFlags::PERM_READ
            | zx::VmarFlags::MAP_RANGE
            | zx::VmarFlags::REQUIRE_NON_RESIZABLE
            | zx::VmarFlags::PERM_WRITE;
        let buffer_addr = Self::map_vmo(&vmo, size, flags)?;
        Ok((Self { buffer_addr, vmo_size: size }, vmo))
    }

    /// Maps `vmo` read-only into the root VMAR, optionally clamping the mapped length to
    /// `max_size`.
    ///
    /// On Fuchsia, `max_size` (when `Some`) must be a non-zero multiple of
    /// [`constants::PAGE_SIZE_BYTES`]; returns `Err(zx::Status::INVALID_ARGS)` otherwise rather
    /// than implicitly rounding up or down, because `zx_vmar_unmap` in [`Container::drop`] requires
    /// a non-zero page-aligned length.
    pub fn read_only_with_max_size(
        vmo: &zx::Vmo,
        max_size: Option<usize>,
    ) -> Result<Self, zx::Status> {
        let raw_size = vmo.get_size()? as usize;
        let vmo_size = match max_size {
            Some(max_size) => {
                if max_size == 0 || !max_size.is_multiple_of(constants::PAGE_SIZE_BYTES) {
                    return Err(zx::Status::INVALID_ARGS);
                }
                std::cmp::min(raw_size, max_size)
            }
            None => raw_size,
        };
        let flags = zx::VmarFlags::PERM_READ | zx::VmarFlags::REQUIRE_NON_RESIZABLE;
        let buffer_addr = Self::map_vmo(vmo, vmo_size, flags)?;
        Ok(Self { buffer_addr, vmo_size })
    }

    fn map_vmo(vmo: &zx::Vmo, vmo_size: usize, flags: zx::VmarFlags) -> Result<usize, zx::Status> {
        let buffer_addr = fuchsia_runtime::vmar_root_self().map(0, vmo, 0, vmo_size, flags)?;
        Ok(buffer_addr)
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        // SAFETY: The memory behind this `Container` is only accessible via references which
        // at this point must have been invalidated and it is safe to unmap the memory.
        unsafe {
            fuchsia_runtime::vmar_root_self()
                .unmap(self.buffer_addr, self.vmo_size)
                .expect("failed to unmap Container");
        }
    }
}

impl BlockContainer for Container {
    type Data = zx::Vmo;
    type ShareableData = zx::Vmo;

    #[inline]
    fn len(&self) -> usize {
        self.vmo_size
    }
}

impl ReadBytes for Container {
    /// Returns a slice of the given size at the given offset if one exists of the exact size.
    /// The offset is inclusive.
    #[inline]
    fn get_slice_at(&self, offset: usize, size: usize) -> Option<&[u8]> {
        if offset >= self.len() {
            return None;
        }
        let upper_bound = offset.checked_add(size)?;
        if upper_bound > self.len() {
            return None;
        }
        let ptr = (self.buffer_addr + offset) as *const u8;
        // SAFETY: the checks above guarantee we have a slice of bytes with `size` elements. Since
        // we have a shared reference to this container, we can get a shared reference to the
        // underlying mapped memory, which lifetime is tied to our Container.
        unsafe { Some(std::slice::from_raw_parts(ptr, size)) }
    }
}

impl CopyBytes for Container {
    #[inline]
    fn copy_bytes_at(&self, offset: usize, dst: &mut [u8]) {
        if let Some(slice) = self.get_slice_at(offset, dst.len()) {
            dst.copy_from_slice(slice);
        }
    }
}

impl WriteBytes for Container {
    /// Returns a slice of the given size at the given offset if one exists of the exact size.
    /// The offset is inclusive.
    #[inline]
    fn get_slice_mut_at(&mut self, offset: usize, size: usize) -> Option<&mut [u8]> {
        if offset >= self.len() {
            return None;
        }
        let upper_bound = offset.checked_add(size)?;
        if upper_bound > self.len() {
            return None;
        }
        let ptr = (self.buffer_addr + offset) as *mut u8;
        // SAFETY: the checks above guarantee we have a slice of bytes with `size` elements. Since
        // we have a exclusive reference to this container, we can get a exclusive reference to the
        // underlying mapped memory, which lifetime is tied to our Container.
        unsafe { Some(std::slice::from_raw_parts_mut(ptr, size)) }
    }
}

impl BlockContainer for zx::Vmo {
    type Data = Self;
    type ShareableData = Self;

    #[inline]
    fn len(&self) -> usize {
        self.get_size().unwrap_or(0) as usize
    }
}

impl CopyBytes for zx::Vmo {
    #[inline]
    fn copy_bytes_at(&self, offset: usize, dst: &mut [u8]) {
        self.read(dst, offset as u64).ok();
    }
}
