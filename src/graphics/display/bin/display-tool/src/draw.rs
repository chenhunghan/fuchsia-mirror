// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result, format_err};
use display_utils::{Image, ImageId};
use fuchsia_image_format::{
    image_format_minimum_row_bytes_2, image_format_stride_bytes_per_width_pixel_2,
};

use mapped_vmo::Mapping;
use std::cell::Cell;
#[cfg(test)]
use std::cell::RefCell;
use std::cmp::{max, min};
use std::iter::Sum;
use std::ops::{Add, Range};

// TODO(armansito): Extend this to support different patterns and tiled image formats.

pub struct MappedImage {
    image: Image,
    surface: Surface,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Frame {
    pub pos_x: u32,
    pub pos_y: u32,
    pub width: u32,
    pub height: u32,
}

impl Frame {
    /// Returns the smallest frame that contains both `self` and `other`.
    pub fn bounding_box(&self, other: &Frame) -> Frame {
        let left = min(self.pos_x, other.pos_x);
        let top = min(self.pos_y, other.pos_y);
        let right = max(self.right(), other.right());
        let bottom = max(self.bottom(), other.bottom());
        Frame { pos_x: left, pos_y: top, width: right - left, height: bottom - top }
    }

    /// Returns the pixels that are in both `self` and `other`, or None if there are none.
    pub fn intersection(&self, other: &Frame) -> Option<Frame> {
        let left = max(self.pos_x, other.pos_x);
        let top = max(self.pos_y, other.pos_y);
        let right = min(self.right(), other.right());
        let bottom = min(self.bottom(), other.bottom());
        if left < right && top < bottom {
            Some(Frame { pos_x: left, pos_y: top, width: right - left, height: bottom - top })
        } else {
            None
        }
    }

    fn right(&self) -> u32 {
        self.pos_x.saturating_add(self.width)
    }

    fn bottom(&self) -> u32 {
        self.pos_y.saturating_add(self.height)
    }
}

/// A CPU access to a [`Surface`]'s bytes, recorded by tests.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq)]
pub enum SurfaceOp {
    Write(Range<usize>),
    Clean(Range<usize>),
}

/// The bytes that drawing on one or more [`Surface`]s wrote and cleaned from the caches.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ByteCounts {
    /// Bytes written by drawing, including zeroing.
    pub written: u64,
    /// Bytes cleaned (written back) from the data caches.
    pub cleaned: u64,
}

impl Add for ByteCounts {
    type Output = ByteCounts;

    fn add(self, other: ByteCounts) -> ByteCounts {
        ByteCounts { written: self.written + other.written, cleaned: self.cleaned + other.cleaned }
    }
}

impl Sum for ByteCounts {
    fn sum<I: Iterator<Item = ByteCounts>>(iter: I) -> ByteCounts {
        iter.fold(ByteCounts::default(), Add::add)
    }
}

/// The CPU-writable pixels of a linear image, mapped into memory.
///
/// Holds no display resources, so drawing can be tested over any VMO.
pub struct Surface {
    mapping: Mapping,
    /// The VMO behind `mapping`.
    vmo: zx::Vmo,
    width: u32,
    height: u32,
    pixel_width: u32,
    row_bytes: u32,

    /// Time spent cleaning caches since the last [`Surface::take_clean_time`] call.
    clean_time: Cell<zx::MonotonicDuration>,
    /// Bytes written by drawing (including zeroing) and cleaned from the caches since the last
    /// [`Surface::take_byte_counts`] call.
    byte_counts: Cell<ByteCounts>,

    /// Every write and clean, in order, since the last [`Surface::take_log`] call.
    #[cfg(test)]
    log: RefCell<Vec<SurfaceOp>>,
}

impl Surface {
    /// `mapping` must map all of `vmo`. `pixel_width` is the size of one pixel in bytes, and
    /// `row_bytes` the distance between the starts of two consecutive rows.
    pub fn new(
        mapping: Mapping,
        vmo: zx::Vmo,
        width: u32,
        height: u32,
        pixel_width: u32,
        row_bytes: u32,
    ) -> Self {
        Surface {
            mapping,
            vmo,
            width,
            height,
            pixel_width,
            row_bytes,
            clean_time: Cell::new(zx::MonotonicDuration::ZERO),
            byte_counts: Cell::new(ByteCounts::default()),
            #[cfg(test)]
            log: RefCell::new(Vec::new()),
        }
    }

    /// Clips `frame` to the surface bounds, the way [`Surface::fill_region`] does: an origin past
    /// the right or bottom edge moves to the last column or row.
    pub fn clip(&self, frame: &Frame) -> Frame {
        let pos_x = min(self.width - 1, frame.pos_x);
        let pos_y = min(self.height - 1, frame.pos_y);
        Frame {
            pos_x,
            pos_y,
            width: min(frame.width, self.width - pos_x),
            height: min(frame.height, self.height - pos_y),
        }
    }

    fn record_write(&self, range: Range<usize>) {
        self.byte_counts
            .update(|counts| ByteCounts { written: counts.written + range.len() as u64, ..counts });
        #[cfg(test)]
        self.log.borrow_mut().push(SurfaceOp::Write(range));
    }

    fn record_clean(&self, range: Range<usize>) {
        self.byte_counts
            .update(|counts| ByteCounts { cleaned: counts.cleaned + range.len() as u64, ..counts });
        #[cfg(test)]
        self.log.borrow_mut().push(SurfaceOp::Clean(range));
    }

    /// Sets every byte of the surface to zero.
    pub fn zero(&self) -> Result<()> {
        let size = self.vmo.get_size()?;
        self.vmo.op_range(zx::VmoOp::ZERO, 0, size)?;
        self.record_write(0..usize::try_from(size)?);
        Ok(())
    }

    /// Fill the specified region of this surface with the specified color. The size and
    /// interpretation of the given color must match the underlying image format.
    pub fn fill_region(&self, color: &[u8], frame: &Frame) -> Result<()> {
        if self.pixel_width != u32::try_from(color.len())? {
            return Err(format_err!(
                "provided color width ({}) does not match expected pixel format width ({})",
                color.len(),
                self.pixel_width
            ));
        }

        // First clip the frame to the image bounds.
        let frame = self.clip(frame);

        // Color the pixels within the frame.
        let pixel_stride = self.row_bytes / self.width;
        if pixel_stride == self.pixel_width {
            // The pixels of each row of the frame are contiguous, so each row is written with a
            // single copy. This is much faster than one `write_at()` call per pixel.
            let row = color.repeat(usize::try_from(frame.width)?);
            let row_bytes = usize::try_from(self.row_bytes)?;
            let first_row_offset = row_bytes * usize::try_from(frame.pos_y)?
                + usize::try_from(pixel_stride)? * usize::try_from(frame.pos_x)?;
            for row_index in 0..usize::try_from(frame.height)? {
                let offset = first_row_offset + row_bytes * row_index;
                self.mapping.write_at(offset, &row);
                self.record_write(offset..offset + row.len());
            }
        } else {
            for row in 0..frame.height {
                for col in 0..frame.width {
                    let idx =
                        self.row_bytes * (row + frame.pos_y) + pixel_stride * (col + frame.pos_x);
                    let idx: usize = idx.try_into()?;
                    self.mapping.write_at(idx, color);
                    self.record_write(idx..idx + color.len());
                }
            }
        }

        Ok(())
    }

    /// Clean (write back) data caches for the whole surface.
    pub fn cache_clean(&self) -> Result<()> {
        let start = zx::MonotonicInstant::get();
        let size = self.vmo.get_size()?;
        self.vmo.op_range(zx::VmoOp::CACHE_CLEAN, 0, size)?;
        self.record_clean(0..usize::try_from(size)?);
        self.clean_time.set(self.clean_time.get() + (zx::MonotonicInstant::get() - start));
        Ok(())
    }

    /// Returns the bytes from the first to the last byte that [`Surface::fill_region`] writes for
    /// `frame`, or None if it writes nothing.
    fn byte_range(&self, frame: &Frame) -> Option<Range<usize>> {
        let frame = self.clip(frame);
        if frame.width == 0 || frame.height == 0 {
            return None;
        }
        let pixel_stride = (self.row_bytes / self.width) as usize;
        let row_bytes = self.row_bytes as usize;
        let start = row_bytes * frame.pos_y as usize + pixel_stride * frame.pos_x as usize;
        let len = (frame.height as usize - 1) * row_bytes
            + (frame.width as usize - 1) * pixel_stride
            + self.pixel_width as usize;
        Some(start..start + len)
    }

    /// Clean (write back) data caches for every pixel that [`Surface::fill_region`] writes for
    /// any of `frames`.
    ///
    /// Each frame cleans the whole band of rows from its first to its last pixel, as one range.
    /// Overlapping ranges are merged, so no byte is cleaned twice.
    pub fn cache_clean_regions(&self, frames: &[Frame]) -> Result<()> {
        let start = zx::MonotonicInstant::get();
        let mut ranges: Vec<Range<usize>> =
            frames.iter().filter_map(|frame| self.byte_range(frame)).collect();
        ranges.sort_by_key(|range| range.start);
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
        for range in ranges {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => last.end = max(last.end, range.end),
                _ => merged.push(range),
            }
        }
        for range in merged {
            self.vmo.op_range(zx::VmoOp::CACHE_CLEAN, range.start as u64, range.len() as u64)?;
            self.record_clean(range);
        }
        self.clean_time.set(self.clean_time.get() + (zx::MonotonicInstant::get() - start));
        Ok(())
    }

    /// Returns the time spent cleaning caches since the previous call, and resets the count.
    pub fn take_clean_time(&self) -> zx::MonotonicDuration {
        self.clean_time.replace(zx::MonotonicDuration::ZERO)
    }

    /// Returns the bytes written and the bytes cleaned since the previous call, and resets the
    /// counts.
    pub fn take_byte_counts(&self) -> ByteCounts {
        self.byte_counts.take()
    }

    /// Returns the writes and cleans since the previous call, and clears the log.
    #[cfg(test)]
    pub fn take_log(&self) -> Vec<SurfaceOp> {
        self.log.take()
    }

    /// Returns a copy of all the surface's bytes.
    #[cfg(test)]
    pub fn bytes(&self) -> Vec<u8> {
        let mut bytes = vec![0; self.mapping.len()];
        self.mapping.read_at(0, &mut bytes);
        bytes
    }
}

impl MappedImage {
    pub fn create(image: Image) -> Result<MappedImage> {
        let size: usize = *image.buffer_settings.size_bytes.as_ref().unwrap() as usize;
        let vmo = image
            .vmo
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .context("failed to duplicate VMO handle")?;
        let mapping = Mapping::create_from_vmo(
            &vmo,
            size,
            zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
        )
        .context("failed to map VMO")?;

        let constraints = &image.format_constraints;
        let pixel_width = image_format_stride_bytes_per_width_pixel_2(
            *constraints.pixel_format.as_ref().unwrap(),
        )
        .unwrap();
        let row_bytes = image_format_minimum_row_bytes_2(constraints, image.parameters.width)?;
        let surface = Surface::new(
            mapping,
            vmo,
            image.parameters.width,
            image.parameters.height,
            pixel_width,
            row_bytes,
        );
        Ok(MappedImage { image, surface })
    }

    pub fn id(&self) -> ImageId {
        self.image.id
    }

    /// The image's pixels.
    pub fn surface(&self) -> &Surface {
        &self.surface
    }

    pub fn zero(&self) -> Result<()> {
        self.surface.zero()
    }

    /// Fill the image with the specified color. The size and interpretation of the given color
    /// must match the underlying image format.
    pub fn fill(&self, color: &[u8]) -> Result<()> {
        self.fill_region(
            color,
            &Frame { pos_x: 0, pos_y: 0, width: self.width(), height: self.height() },
        )
    }

    /// Fill the specified region of this image with the specified color. The size and
    /// interpretation of the given color must match the underlying image format.
    pub fn fill_region(&self, color: &[u8], frame: &Frame) -> Result<()> {
        self.surface.fill_region(color, frame)
    }

    /// Clean (write back) data caches, so previous writes are visible in main memory. This
    /// operation must be called at once before this image buffer gets presented to avoid artifacts
    /// during scanout in height refresh rates if the image is changing frequently. This operation
    /// can be expensive on large images and should be performed sparingly.
    pub fn cache_clean(&self) -> Result<()> {
        self.surface.cache_clean()
    }

    /// Returns the time spent cleaning caches since the previous call, and resets the count.
    pub fn take_clean_time(&self) -> zx::MonotonicDuration {
        self.surface.take_clean_time()
    }

    /// Returns the bytes written and the bytes cleaned since the previous call, and resets the
    /// counts.
    pub fn take_byte_counts(&self) -> ByteCounts {
        self.surface.take_byte_counts()
    }

    fn width(&self) -> u32 {
        self.image.parameters.width
    }

    fn height(&self) -> u32 {
        self.image.parameters.height
    }
}

/// Helpers for tests of code that draws on a [`Surface`].
#[cfg(test)]
pub mod testing {
    use super::*;

    /// Creates a surface of 4-byte pixels without row padding, all zero.
    pub fn new_surface(width: u32, height: u32) -> Surface {
        let page_size = zx::system_get_page_size() as usize;
        let size = ((width * height * 4) as usize).next_multiple_of(page_size);
        let (mapping, vmo) = Mapping::allocate(size).expect("allocate test VMO");
        Surface::new(mapping, vmo, width, height, 4, width * 4)
    }

    /// Checks that every byte written in `ops` is cleaned by a later op in `ops`.
    ///
    /// Returns the offset of the first byte that is not, if any.
    pub fn first_uncleaned_byte(ops: &[SurfaceOp]) -> Option<usize> {
        let size = ops
            .iter()
            .map(|op| match op {
                SurfaceOp::Write(range) | SurfaceOp::Clean(range) => range.end,
            })
            .max()
            .unwrap_or(0);
        let mut cleaned = vec![false; size];
        let mut first_uncleaned: Option<usize> = None;
        for op in ops.iter().rev() {
            match op {
                SurfaceOp::Clean(range) => cleaned[range.clone()].fill(true),
                SurfaceOp::Write(range) => {
                    if let Some(offset) = range.clone().find(|offset| !cleaned[*offset]) {
                        first_uncleaned =
                            Some(first_uncleaned.map_or(offset, |first| min(first, offset)));
                    }
                }
            }
        }
        first_uncleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use googletest::{expect_eq, expect_true, gtest};

    /// The per-pixel algorithm `fill_region` used before rows were copied at once, over a plain
    /// buffer. Results must match it byte for byte.
    fn reference_fill_region(
        buffer: &mut [u8],
        width: u32,
        height: u32,
        row_bytes: u32,
        color: &[u8],
        frame: &Frame,
    ) {
        let pos_x = min(width - 1, frame.pos_x);
        let pos_y = min(height - 1, frame.pos_y);
        let frame_width = min(frame.width, width - pos_x);
        let frame_height = min(frame.height, height - pos_y);
        let pixel_stride = row_bytes / width;
        for row in 0..frame_height {
            for col in 0..frame_width {
                let idx = (row_bytes * (row + pos_y) + pixel_stride * (col + pos_x)) as usize;
                buffer[idx..idx + color.len()].copy_from_slice(color);
            }
        }
    }

    /// A [`Surface`] over a test VMO, plus the bytes it is expected to hold.
    struct Fixture {
        surface: Surface,
        expected: Vec<u8>,
    }

    impl Fixture {
        /// Creates a surface whose bytes start out as a non-zero pattern, so writes to the wrong
        /// place and missing writes both show up as differences.
        fn new(width: u32, height: u32, pixel_width: u32, row_bytes: u32) -> Fixture {
            let page_size = zx::system_get_page_size() as usize;
            let size = ((row_bytes * height) as usize).next_multiple_of(page_size);
            let (mapping, vmo) = Mapping::allocate(size).expect("allocate test VMO");
            let expected: Vec<u8> = (0..size).map(|i| (i * 7 + 3) as u8).collect();
            mapping.write_at(0, &expected);
            Fixture {
                surface: Surface::new(mapping, vmo, width, height, pixel_width, row_bytes),
                expected,
            }
        }

        /// Fills `frame` in the surface and in the expected bytes.
        fn fill_region(&mut self, color: &[u8], frame: Frame) {
            self.surface.fill_region(color, &frame).expect("fill_region");
            let surface = &self.surface;
            reference_fill_region(
                &mut self.expected,
                surface.width,
                surface.height,
                surface.row_bytes,
                color,
                &frame,
            );
        }

        fn actual(&self) -> Vec<u8> {
            let mut actual = vec![0; self.surface.mapping.len()];
            self.surface.mapping.read_at(0, &mut actual);
            actual
        }

        /// Returns the offset of the first byte that differs from the reference, if any.
        fn first_difference(&self) -> Option<usize> {
            self.actual().iter().zip(self.expected.iter()).position(|(a, e)| a != e)
        }
    }

    const COLOR: [u8; 4] = [0x10, 0x20, 0x30, 0xff];

    #[gtest]
    #[fuchsia::test]
    fn reference_fills_exactly_the_frame() {
        // Checks the reference itself, by hand, on a 4x3 surface with 2 bytes of padding per row.
        let mut buffer = vec![0u8; 18 * 3];
        reference_fill_region(
            &mut buffer,
            4,
            3,
            18,
            &COLOR,
            &Frame { pos_x: 1, pos_y: 1, width: 2, height: 1 },
        );
        let mut expected = vec![0u8; 18 * 3];
        expected[22..26].copy_from_slice(&COLOR);
        expected[26..30].copy_from_slice(&COLOR);
        expect_eq!(buffer, expected);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_region_writes_only_the_frame() {
        let mut fixture = Fixture::new(4, 3, 4, 16);
        let before = fixture.actual();
        fixture.fill_region(&COLOR, Frame { pos_x: 1, pos_y: 1, width: 2, height: 2 });
        let actual = fixture.actual();
        for (offset, (byte, old)) in actual.iter().zip(before.iter()).enumerate() {
            let row = offset / 16;
            let col = (offset % 16) / 4;
            if (1..3).contains(&row) && (1..3).contains(&col) {
                expect_eq!(*byte, COLOR[offset % 4], "offset {}", offset);
            } else {
                expect_eq!(*byte, *old, "offset {}", offset);
            }
        }
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_whole_surface() {
        let mut fixture = Fixture::new(64, 48, 4, 256);
        fixture.fill_region(&COLOR, Frame { pos_x: 0, pos_y: 0, width: 64, height: 48 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_interior_rect() {
        let mut fixture = Fixture::new(64, 48, 4, 256);
        fixture.fill_region(&COLOR, Frame { pos_x: 5, pos_y: 7, width: 20, height: 11 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_rect_clipped_at_right_edge() {
        let mut fixture = Fixture::new(64, 48, 4, 256);
        fixture.fill_region(&COLOR, Frame { pos_x: 50, pos_y: 3, width: 30, height: 10 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_rect_clipped_at_bottom_edge() {
        let mut fixture = Fixture::new(64, 48, 4, 256);
        fixture.fill_region(&COLOR, Frame { pos_x: 3, pos_y: 40, width: 10, height: 30 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_rect_clipped_at_bottom_right_corner() {
        let mut fixture = Fixture::new(64, 48, 4, 256);
        fixture
            .fill_region(&COLOR, Frame { pos_x: 60, pos_y: 45, width: u32::MAX, height: u32::MAX });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_origin_past_edge_draws_last_column_and_row() {
        let mut fixture = Fixture::new(64, 48, 4, 256);
        fixture.fill_region(&COLOR, Frame { pos_x: 100, pos_y: 5, width: 10, height: 3 });
        fixture.fill_region(&[1, 2, 3, 4], Frame { pos_x: 5, pos_y: 100, width: 3, height: 10 });
        fixture.fill_region(&[5, 6, 7, 8], Frame { pos_x: 64, pos_y: 48, width: 2, height: 2 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_one_pixel() {
        let mut fixture = Fixture::new(64, 48, 4, 256);
        fixture.fill_region(&COLOR, Frame { pos_x: 17, pos_y: 9, width: 1, height: 1 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_one_pixel_surface() {
        let mut fixture = Fixture::new(1, 1, 4, 64);
        fixture.fill_region(&COLOR, Frame { pos_x: 0, pos_y: 0, width: 1, height: 1 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_empty_frame_writes_nothing() {
        let mut fixture = Fixture::new(64, 48, 4, 256);
        fixture.fill_region(&COLOR, Frame { pos_x: 5, pos_y: 5, width: 0, height: 10 });
        fixture.fill_region(&COLOR, Frame { pos_x: 5, pos_y: 5, width: 10, height: 0 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_three_byte_pixels() {
        // Rows without padding.
        let mut fixture = Fixture::new(10, 6, 3, 30);
        fixture.fill_region(&[1, 2, 3], Frame { pos_x: 2, pos_y: 1, width: 5, height: 3 });
        fixture.fill_region(&[4, 5, 6], Frame { pos_x: 8, pos_y: 4, width: 5, height: 5 });
        expect_eq!(fixture.first_difference(), None);

        // Rows padded by less than a pixel per pixel, so the pixel stride is still 3 bytes.
        let mut fixture = Fixture::new(10, 6, 3, 32);
        fixture.fill_region(&[1, 2, 3], Frame { pos_x: 2, pos_y: 1, width: 5, height: 3 });
        fixture.fill_region(&[4, 5, 6], Frame { pos_x: 8, pos_y: 4, width: 5, height: 5 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_padded_rows_with_pixel_stride_wider_than_pixel() {
        // 64-byte rows of 10 pixels give a pixel stride of 6 bytes for 4-byte pixels.
        let mut fixture = Fixture::new(10, 6, 4, 64);
        fixture.fill_region(&COLOR, Frame { pos_x: 0, pos_y: 0, width: 10, height: 6 });
        fixture.fill_region(&[1, 2, 3, 4], Frame { pos_x: 3, pos_y: 2, width: 4, height: 3 });
        fixture.fill_region(&[5, 6, 7, 8], Frame { pos_x: 8, pos_y: 5, width: 4, height: 4 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn later_fills_paint_over_earlier_ones() {
        // A scaled-down multilayer-squares frame: clear, then overlapping squares.
        let mut fixture = Fixture::new(168, 374, 4, 168 * 4);
        fixture.fill_region(&[0, 0, 0, 0], Frame { pos_x: 0, pos_y: 0, width: 168, height: 374 });
        fixture.fill_region(&COLOR, Frame { pos_x: 10, pos_y: 20, width: 84, height: 84 });
        fixture
            .fill_region(&[59, 150, 0, 150], Frame { pos_x: 50, pos_y: 60, width: 21, height: 21 });
        fixture.fill_region(&[1, 2, 3, 4], Frame { pos_x: 120, pos_y: 300, width: 84, height: 84 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_with_wrong_color_size_fails_and_writes_nothing() {
        let fixture = Fixture::new(64, 48, 4, 256);
        let frame = Frame { pos_x: 0, pos_y: 0, width: 64, height: 48 };
        expect_true!(fixture.surface.fill_region(&[1, 2, 3], &frame).is_err());
        expect_true!(fixture.surface.fill_region(&[1, 2, 3, 4, 5], &frame).is_err());
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn frame_intersection() {
        let a = Frame { pos_x: 10, pos_y: 20, width: 30, height: 40 };
        expect_eq!(
            a.intersection(&Frame { pos_x: 30, pos_y: 50, width: 100, height: 5 }),
            Some(Frame { pos_x: 30, pos_y: 50, width: 10, height: 5 })
        );
        expect_eq!(a.intersection(&a), Some(a));
        // Frames that only touch share no pixel.
        expect_eq!(a.intersection(&Frame { pos_x: 40, pos_y: 20, width: 5, height: 5 }), None);
        expect_eq!(a.intersection(&Frame { pos_x: 10, pos_y: 60, width: 5, height: 5 }), None);
        expect_eq!(a.intersection(&Frame { pos_x: 15, pos_y: 25, width: 0, height: 5 }), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn frame_bounding_box() {
        let a = Frame { pos_x: 10, pos_y: 20, width: 30, height: 40 };
        let b = Frame { pos_x: 25, pos_y: 5, width: 100, height: 10 };
        let expected = Frame { pos_x: 10, pos_y: 5, width: 115, height: 55 };
        expect_eq!(a.bounding_box(&b), expected);
        expect_eq!(b.bounding_box(&a), expected);
        expect_eq!(a.bounding_box(&a), a);
    }

    /// Returns the smallest range that contains all the writes in `ops`.
    fn written_span(ops: &[SurfaceOp]) -> Option<Range<usize>> {
        ops.iter().fold(None, |span, op| match (span, op) {
            (None, SurfaceOp::Write(range)) => Some(range.clone()),
            (Some(span), SurfaceOp::Write(range)) => {
                Some(min(span.start, range.start)..max(span.end, range.end))
            }
            (span, SurfaceOp::Clean(_)) => span,
        })
    }

    #[gtest]
    #[fuchsia::test]
    fn clean_region_covers_exactly_the_written_span() {
        let frames = [
            Frame { pos_x: 5, pos_y: 7, width: 20, height: 11 },
            Frame { pos_x: 50, pos_y: 3, width: 30, height: 10 },
            Frame { pos_x: 3, pos_y: 40, width: 10, height: 30 },
            Frame { pos_x: 60, pos_y: 45, width: u32::MAX, height: u32::MAX },
            Frame { pos_x: 100, pos_y: 5, width: 10, height: 3 },
            Frame { pos_x: 64, pos_y: 48, width: 2, height: 2 },
            Frame { pos_x: 17, pos_y: 9, width: 1, height: 1 },
            Frame { pos_x: 0, pos_y: 0, width: 64, height: 48 },
        ];
        // Rows without padding, rows with padding, and a pixel stride wider than the pixel.
        for (width, row_bytes) in [(64, 256), (64, 320), (10, 64)] {
            for frame in &frames {
                let fixture = Fixture::new(width, 48, 4, row_bytes);
                fixture.surface.fill_region(&COLOR, frame).expect("fill_region");
                fixture.surface.cache_clean_regions(&[*frame]).expect("cache_clean_regions");
                let ops = fixture.surface.take_log();
                let span = written_span(&ops).expect("frame writes pixels");
                expect_eq!(
                    ops.last(),
                    Some(&SurfaceOp::Clean(span)),
                    "width {} row_bytes {} {:?}",
                    width,
                    row_bytes,
                    frame
                );
                expect_eq!(testing::first_uncleaned_byte(&ops), None);
            }
        }
    }

    #[gtest]
    #[fuchsia::test]
    fn clean_regions_skips_empty_frames() {
        let fixture = Fixture::new(64, 48, 4, 256);
        fixture
            .surface
            .cache_clean_regions(&[
                Frame { pos_x: 5, pos_y: 5, width: 0, height: 10 },
                Frame { pos_x: 5, pos_y: 5, width: 10, height: 0 },
            ])
            .expect("cache_clean_regions");
        expect_eq!(fixture.surface.take_log(), vec![]);
    }

    #[gtest]
    #[fuchsia::test]
    fn clean_regions_merges_overlapping_ranges() {
        let fixture = Fixture::new(64, 48, 4, 256);
        fixture
            .surface
            .cache_clean_regions(&[
                Frame { pos_x: 40, pos_y: 10, width: 4, height: 2 },
                Frame { pos_x: 0, pos_y: 0, width: 2, height: 1 },
                Frame { pos_x: 0, pos_y: 11, width: 2, height: 3 },
            ])
            .expect("cache_clean_regions");
        expect_eq!(
            fixture.surface.take_log(),
            vec![SurfaceOp::Clean(0..8), SurfaceOp::Clean(10 * 256 + 160..13 * 256 + 8),]
        );
        expect_eq!(
            fixture.surface.take_byte_counts(),
            ByteCounts { written: 0, cleaned: 8 + 3 * 256 + 8 - 160 }
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn byte_counts_track_writes_and_cleans() {
        let fixture = Fixture::new(64, 48, 4, 256);
        fixture
            .surface
            .fill_region(&COLOR, &Frame { pos_x: 1, pos_y: 2, width: 3, height: 4 })
            .expect("fill_region");
        fixture.surface.cache_clean().expect("cache_clean");
        let size = fixture.surface.mapping.len() as u64;
        expect_eq!(
            fixture.surface.take_byte_counts(),
            ByteCounts { written: 3 * 4 * 4, cleaned: size }
        );
        expect_eq!(fixture.surface.take_byte_counts(), ByteCounts { written: 0, cleaned: 0 });
        fixture.surface.zero().expect("zero");
        expect_eq!(fixture.surface.take_byte_counts(), ByteCounts { written: size, cleaned: 0 });
    }

    #[gtest]
    #[fuchsia::test]
    fn byte_counts_sum_adds_written_and_cleaned_separately() {
        let counts = [
            ByteCounts { written: 1, cleaned: 20 },
            ByteCounts { written: 300, cleaned: 4000 },
            ByteCounts { written: 50000, cleaned: 0 },
        ];
        expect_eq!(
            counts.into_iter().sum::<ByteCounts>(),
            ByteCounts { written: 50301, cleaned: 4020 }
        );
        expect_eq!(
            std::iter::empty::<ByteCounts>().sum::<ByteCounts>(),
            ByteCounts { written: 0, cleaned: 0 }
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn zero_clears_every_byte() {
        let fixture = Fixture::new(64, 48, 4, 256);
        fixture.surface.zero().expect("zero");
        expect_true!(fixture.surface.bytes().iter().all(|byte| *byte == 0));
    }

    #[gtest]
    #[fuchsia::test]
    fn first_uncleaned_byte_finds_writes_not_cleaned_later() {
        use SurfaceOp::{Clean, Write};
        expect_eq!(testing::first_uncleaned_byte(&[]), None);
        expect_eq!(testing::first_uncleaned_byte(&[Write(4..8), Clean(0..16)]), None);
        expect_eq!(testing::first_uncleaned_byte(&[Write(4..8)]), Some(4));
        // A clean before the write does not count.
        expect_eq!(testing::first_uncleaned_byte(&[Clean(0..16), Write(4..8)]), Some(4));
        // Partially cleaned writes.
        expect_eq!(
            testing::first_uncleaned_byte(&[Write(4..8), Write(20..24), Clean(0..22)]),
            Some(22)
        );
        expect_eq!(testing::first_uncleaned_byte(&[Write(4..8), Clean(5..8)]), Some(4));
    }
}
