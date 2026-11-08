// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// Error returned when a read or write on a [`BlobProperty`](crate::writer::BlobProperty) or
/// [`BlobReaderWriter`] exceeds its payload capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BoundsError {
    #[error(
        "access at offset {offset} with length {len} exceeds payload capacity {payload_capacity}"
    )]
    OutOfBounds { offset: usize, len: usize, payload_capacity: usize },
}

/// A scoped reader and writer providing read-modify-write access to the payload section of a
/// [`BlobProperty`](crate::writer::BlobProperty) while its 32-byte `"BLOB"` header seqlock is held
/// during [`BlobProperty::atomic_write`](crate::writer::BlobProperty::atomic_write).
///
/// All offsets on `BlobReaderWriter` are relative to the **payload section**: offset `0` refers
/// to the first byte immediately *after* the 32-byte `"BLOB"` header (VMO byte offset 32).
///
/// `payload` is sliced strictly to the caller-requested `payload_size` (`32..32 + payload_size`),
/// excluding both the 32-byte `"BLOB"` header and any trailing padding added to round the
/// underlying VMO up to a 4 KiB page boundary. All methods on `BlobReaderWriter` check bounds
/// against `payload.len()`, so callers can never read or write into the header or trailing page
/// padding.
#[derive(Debug, PartialEq, Eq)]
pub struct BlobReaderWriter<'a> {
    /// Mutable slice over VMO bytes `32..32 + payload_size` (excluding header and page padding).
    payload: &'a mut [u8],
}

impl<'a> BlobReaderWriter<'a> {
    #[inline]
    pub(crate) fn new(payload: &'a mut [u8]) -> Self {
        Self { payload }
    }

    /// Returns the payload capacity in bytes (excluding the 32-byte `"BLOB"` header and any
    /// trailing page-alignment padding).
    #[inline]
    pub fn payload_capacity(&self) -> usize {
        self.payload.len()
    }

    /// Reads `dst.len()` bytes from the payload section starting at `offset` (where `0` is the
    /// first byte after the 32-byte `"BLOB"` header) into `dst`.
    ///
    /// Enforces strict boundary checks against `self.payload_capacity()` (`payload_size` without
    /// page padding) via [`Self::slice_at`], returning [`BoundsError::OutOfBounds`] if
    /// `offset + dst.len() > self.payload_capacity()`.
    #[inline]
    pub fn read_at(&self, offset: usize, dst: &mut [u8]) -> Result<(), BoundsError> {
        let slice = self.slice_at(offset, dst.len())?;
        dst.copy_from_slice(slice);
        Ok(())
    }

    /// Returns a shared slice of `len` bytes starting at payload `offset` (where `0` is the
    /// first byte after the 32-byte `"BLOB"` header).
    ///
    /// Checks for integer overflow and verifies `offset + len <= self.payload.len()` (which is
    /// strictly `payload_size`, excluding the 32-byte header and any 4 KiB page padding).
    #[inline]
    pub fn slice_at(&self, offset: usize, len: usize) -> Result<&[u8], BoundsError> {
        let payload_capacity = self.payload.len();
        let end = offset
            .checked_add(len)
            .filter(|&end| end <= payload_capacity)
            .ok_or(BoundsError::OutOfBounds { offset, len, payload_capacity })?;
        Ok(&self.payload[offset..end])
    }

    /// Returns a shared slice over the entire payload section (`0..payload_capacity()`).
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        self.payload
    }

    /// Writes `bytes` into the payload section starting at `offset` (where `0` is the first
    /// byte after the 32-byte `"BLOB"` header).
    ///
    /// Enforces strict boundary checks against `self.payload_capacity()` (`payload_size` without
    /// page padding) via [`Self::slice_mut_at`], returning [`BoundsError::OutOfBounds`] if
    /// `offset + bytes.len() > self.payload_capacity()`.
    #[inline]
    pub fn write_at(&mut self, offset: usize, bytes: &[u8]) -> Result<(), BoundsError> {
        let slice = self.slice_mut_at(offset, bytes.len())?;
        slice.copy_from_slice(bytes);
        Ok(())
    }

    /// Returns a mutable slice of `len` bytes starting at payload `offset` (where `0` is the
    /// first byte after the 32-byte `"BLOB"` header).
    ///
    /// Checks for integer overflow and verifies `offset + len <= self.payload.len()` (which is
    /// strictly `payload_size`, excluding the 32-byte header and any 4 KiB page padding).
    #[inline]
    pub fn slice_mut_at(&mut self, offset: usize, len: usize) -> Result<&mut [u8], BoundsError> {
        let payload_capacity = self.payload.len();
        let end = offset
            .checked_add(len)
            .filter(|&end| end <= payload_capacity)
            .ok_or(BoundsError::OutOfBounds { offset, len, payload_capacity })?;
        Ok(&mut self.payload[offset..end])
    }

    /// Returns a mutable slice over the entire payload section (`0..payload_capacity()`).
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn blob_reader_writer_empty_slice() {
        let mut buf: [u8; 0] = [];
        let mut rw = BlobReaderWriter::new(&mut buf);
        assert_eq!(rw.payload_capacity(), 0);
        assert!(rw.as_slice().is_empty());
        assert!(rw.as_mut_slice().is_empty());

        // Zero-length reads, writes, and slices at offset 0 succeed on an empty slice because
        // 0 + 0 <= 0.
        assert_eq!(rw.read_at(0, &mut []), Ok(()));
        assert_eq!(rw.slice_at(0, 0), Ok(&[][..]));
        assert_eq!(rw.write_at(0, &[]), Ok(()));
        assert_eq!(rw.slice_mut_at(0, 0), Ok(&mut [][..]));

        // Accessing 1 byte at offset 0 exceeds payload_capacity (0).
        assert_eq!(
            rw.read_at(0, &mut [0u8; 1]),
            Err(BoundsError::OutOfBounds { offset: 0, len: 1, payload_capacity: 0 })
        );
        assert_eq!(
            rw.write_at(0, &[1]),
            Err(BoundsError::OutOfBounds { offset: 0, len: 1, payload_capacity: 0 })
        );
        assert_eq!(
            rw.slice_mut_at(0, 1),
            Err(BoundsError::OutOfBounds { offset: 0, len: 1, payload_capacity: 0 })
        );

        // Even a 0-byte read or write at offset 1 is out of bounds on a 0-capacity slice.
        assert_eq!(
            rw.slice_at(1, 0),
            Err(BoundsError::OutOfBounds { offset: 1, len: 0, payload_capacity: 0 })
        );
        assert_eq!(
            rw.write_at(1, &[]),
            Err(BoundsError::OutOfBounds { offset: 1, len: 0, payload_capacity: 0 })
        );
    }

    #[fuchsia::test]
    fn blob_reader_writer_write_at_and_slice_mut_at() {
        let mut buf = [0u8; 16];
        let mut rw = BlobReaderWriter::new(&mut buf);
        assert_eq!(rw.payload_capacity(), 16);

        // Write at the start (0..4), middle (6..8), and exact end (12..16) of the payload slice.
        rw.write_at(0, &[1, 2, 3, 4]).unwrap();
        rw.write_at(6, &[0xAA, 0xBB]).unwrap();
        rw.write_at(12, &[5, 6, 7, 8]).unwrap();

        // Mutate a sub-slice (4..6) in-place via slice_mut_at.
        let mid = rw.slice_mut_at(4, 2).unwrap();
        mid.copy_from_slice(&[9, 10]);

        // Verify all written regions landed at their expected offsets while unwritten bytes (8..12)
        // remain zeroed.
        assert_eq!(rw.as_mut_slice(), &[1, 2, 3, 4, 9, 10, 0xAA, 0xBB, 0, 0, 0, 0, 5, 6, 7, 8]);
    }

    #[fuchsia::test]
    fn blob_reader_writer_write_out_of_bounds_and_overflow() {
        let mut buf = [0x11u8; 8];
        let mut rw = BlobReaderWriter::new(&mut buf);

        // A write that starts inside the buffer (offset 5) but extends 1 byte past the end
        // (5 + 4 = 9 > 8) is rejected.
        assert_eq!(
            rw.write_at(5, &[1, 2, 3, 4]),
            Err(BoundsError::OutOfBounds { offset: 5, len: 4, payload_capacity: 8 })
        );

        // A 1-byte write starting right at the end boundary (8 + 1 = 9 > 8) is rejected.
        assert_eq!(
            rw.write_at(8, &[1]),
            Err(BoundsError::OutOfBounds { offset: 8, len: 1, payload_capacity: 8 })
        );

        // Even a 0-byte write starting past the end boundary (offset 9 > 8) is rejected.
        assert_eq!(
            rw.write_at(9, &[]),
            Err(BoundsError::OutOfBounds { offset: 9, len: 0, payload_capacity: 8 })
        );

        // Integer overflow when computing `offset + len` is caught by `checked_add` and returns
        // OutOfBounds instead of wrapping around or panicking.
        assert_eq!(
            rw.write_at(usize::MAX, &[1]),
            Err(BoundsError::OutOfBounds { offset: usize::MAX, len: 1, payload_capacity: 8 })
        );
        assert_eq!(
            rw.slice_mut_at(1, usize::MAX),
            Err(BoundsError::OutOfBounds { offset: 1, len: usize::MAX, payload_capacity: 8 })
        );

        // Verify the underlying buffer contents remain untouched after all rejected writes.
        assert_eq!(rw.as_mut_slice(), &[0x11u8; 8]);
    }

    #[fuchsia::test]
    fn blob_reader_writer_read_at_and_slice_at() {
        let mut buf = [10u8, 20, 30, 40, 50, 60, 70, 80];
        let rw = BlobReaderWriter::new(&mut buf);
        assert_eq!(rw.payload_capacity(), 8);
        assert_eq!(rw.as_slice(), &[10, 20, 30, 40, 50, 60, 70, 80]);

        // Copy a 3-byte sub-range (bytes 2..5) into `scratch` via read_at.
        let mut scratch = [0u8; 3];
        rw.read_at(2, &mut scratch).unwrap();
        assert_eq!(scratch, [30, 40, 50]);

        // Borrow a 3-byte sub-slice at the end of the buffer (bytes 5..8) via slice_at.
        assert_eq!(rw.slice_at(5, 3).unwrap(), &[60, 70, 80]);
    }

    #[fuchsia::test]
    fn blob_reader_writer_read_out_of_bounds_and_overflow() {
        let mut buf = [10u8, 20, 30, 40, 50, 60, 70, 80];
        let rw = BlobReaderWriter::new(&mut buf);

        // A read that starts inside the buffer (offset 6) but extends 1 byte past the end
        // (6 + 3 = 9 > 8) is rejected and leaves `scratch` untouched.
        let mut scratch = [0xFFu8; 3];
        assert_eq!(
            rw.read_at(6, &mut scratch),
            Err(BoundsError::OutOfBounds { offset: 6, len: 3, payload_capacity: 8 })
        );
        assert_eq!(scratch, [0xFFu8; 3]);

        // A 0-byte slice starting past the end boundary (offset 9 > 8) is rejected.
        assert_eq!(
            rw.slice_at(9, 0),
            Err(BoundsError::OutOfBounds { offset: 9, len: 0, payload_capacity: 8 })
        );

        // Integer overflow in `offset + len` is caught by `checked_add` and returns OutOfBounds.
        assert_eq!(
            rw.slice_at(usize::MAX, 1),
            Err(BoundsError::OutOfBounds { offset: usize::MAX, len: 1, payload_capacity: 8 })
        );
    }

    #[fuchsia::test]
    fn blob_reader_writer_read_modify_write() {
        let mut buf = [10u8, 20, 30, 40, 50, 60, 70, 80];
        let mut rw = BlobReaderWriter::new(&mut buf);

        // Read existing bytes 2..5 directly via `rw.read_at`.
        let mut scratch = [0u8; 3];
        rw.read_at(2, &mut scratch).unwrap();
        assert_eq!(scratch, [30, 40, 50]);

        // Modify the read bytes and write them back to the same offset via `rw.write_at`.
        for b in &mut scratch {
            *b += 1;
        }
        rw.write_at(2, &scratch).unwrap();

        // Verify `rw.as_slice()` observes the updated bytes in place.
        assert_eq!(rw.as_slice(), &[10, 20, 31, 41, 51, 60, 70, 80]);
    }
}
