// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use cipher::inout::InOut;
use cipher::typenum::consts::U16;
use cipher::{
    Array, BlockCipherDecBackend, BlockCipherDecClosure, BlockCipherEncBackend,
    BlockCipherEncClosure, BlockSizeUser,
};
use static_assertions::assert_cfg;
use storage_ptr_slice::{MutPtrByteSlice, PtrByteSlice};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// This assumes little-endianness which is likely to always be the case.
assert_cfg!(target_endian = "little");

#[derive(IntoBytes, KnownLayout, FromBytes, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Tweak(pub u128);

impl Tweak {
    pub fn new(val: u128) -> Self {
        Self(val)
    }

    fn update(&mut self) {
        self.0 = (self.0 << 1) ^ ((self.0 as i128 >> 127) as u128 & 0x87);
    }
}

#[cfg(target_arch = "aarch64")]
fn assert_dcz_block_size_is_64() {
    static CHECK: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    CHECK.get_or_init(|| {
        let dczid: u64;
        unsafe {
            core::arch::asm!("mrs {0}, dczid_el0", out(reg) dczid);
        }
        let dzp = (dczid >> 4) & 1;
        assert_eq!(dzp, 0, "DC ZVA is prohibited on this CPU");
        let bs_bytes = (1usize << (dczid & 0xf)) * 4;
        assert_eq!(bs_bytes, 64, "Expected DC ZVA block size to be 64 bytes, found {bs_bytes}");
    });
}

/// To be used with encrypt|decrypt_with_backend for out-of-place operation.
/// Conforms to IEEE 1619-2007.
pub struct XtsProcessor<'a, 'b> {
    tweak: Tweak,
    src: PtrByteSlice<'a>,
    dst: MutPtrByteSlice<'b>,
}

/// To be used with encrypt|decrypt_with_backend for in-place operation.
/// Conforms to IEEE 1619-2007.
pub struct XtsInPlaceProcessor<'a> {
    tweak: Tweak,
    buf: MutPtrByteSlice<'a>,
}

fn xts_encrypt_chunk<B: BlockCipherEncBackend<BlockSize = U16>>(
    backend: &B,
    mut val: u128,
    tweak: &Tweak,
) -> u128 {
    // XOR plaintext with tweak.
    val ^= tweak.0;

    let arr: &mut Array<u8, U16> = val.as_mut_bytes().try_into().unwrap();
    backend.encrypt_block(InOut::from(arr));

    // XOR ciphertext with tweak.
    val ^ tweak.0
}

fn xts_encrypt_buffer_out_of_place<B: BlockCipherEncBackend<BlockSize = U16>>(
    backend: &B,
    src: PtrByteSlice<'_>,
    mut dst: MutPtrByteSlice<'_>,
    tweak: &mut Tweak,
) {
    debug_assert_eq!(src.len(), dst.len());
    debug_assert_eq!(src.as_ptr() as usize % 64, 0);
    debug_assert_eq!(dst.as_ptr() as usize % 64, 0);
    debug_assert_eq!(src.len() % 64, 0);

    let mut src_chunks = src.iter_as::<u128>();
    let mut dst_chunks = dst.iter_as_mut::<u128>();

    while let Some(s0) = src_chunks.next() {
        let d0 = dst_chunks.next().unwrap();
        // Zero the 64-byte destination cache line on ARM64 ("DC ZVA") to avoid fetching
        // stale destination cache lines from RAM before writing out-of-place outputs.
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!(
                "dc zva, {0}",
                in(reg) d0.as_ptr(),
                options(nostack, preserves_flags),
            );
        }
        let val0 = xts_encrypt_chunk(backend, s0, tweak);
        d0.write(val0);
        tweak.update();

        let s1 = src_chunks.next().unwrap();
        let d1 = dst_chunks.next().unwrap();
        let val1 = xts_encrypt_chunk(backend, s1, tweak);
        d1.write(val1);
        tweak.update();

        let s2 = src_chunks.next().unwrap();
        let d2 = dst_chunks.next().unwrap();
        let val2 = xts_encrypt_chunk(backend, s2, tweak);
        d2.write(val2);
        tweak.update();

        let s3 = src_chunks.next().unwrap();
        let d3 = dst_chunks.next().unwrap();
        let val3 = xts_encrypt_chunk(backend, s3, tweak);
        d3.write(val3);
        tweak.update();
    }
}

fn xts_encrypt_buffer<B: BlockCipherEncBackend<BlockSize = U16>>(
    backend: &B,
    src: PtrByteSlice<'_>,
    mut dst: MutPtrByteSlice<'_>,
    tweak: &mut Tweak,
) {
    debug_assert_eq!(src.len(), dst.len());
    debug_assert!(src.as_ptr().cast::<u128>().is_aligned());
    debug_assert!(dst.as_ptr().cast::<u128>().is_aligned());

    let src_chunks = src.iter_as::<u128>();
    let dst_chunks = dst.iter_as_mut::<u128>();

    for (src_chunk, dst_chunk) in src_chunks.zip(dst_chunks) {
        let val = xts_encrypt_chunk(backend, src_chunk, tweak);
        dst_chunk.write(val);
        tweak.update();
    }
}

#[inline]
fn xts_encrypt_buffer_in_place<B: BlockCipherEncBackend<BlockSize = U16>>(
    backend: &B,
    mut buf: MutPtrByteSlice<'_>,
    tweak: &mut Tweak,
) {
    debug_assert!(buf.as_ptr().cast::<u128>().is_aligned());

    let mut chunks = buf.iter_as_mut::<u128>();
    while chunks.len() >= 4 {
        let c0 = chunks.next().unwrap();
        let c1 = chunks.next().unwrap();
        let c2 = chunks.next().unwrap();
        let c3 = chunks.next().unwrap();

        let s0 = c0.read();
        let s1 = c1.read();
        let s2 = c2.read();
        let s3 = c3.read();

        let t0 = *tweak;
        tweak.update();
        let t1 = *tweak;
        tweak.update();
        let t2 = *tweak;
        tweak.update();
        let t3 = *tweak;
        tweak.update();

        let v0 = xts_encrypt_chunk(backend, s0, &t0);
        let v1 = xts_encrypt_chunk(backend, s1, &t1);
        let v2 = xts_encrypt_chunk(backend, s2, &t2);
        let v3 = xts_encrypt_chunk(backend, s3, &t3);

        c0.write(v0);
        c1.write(v1);
        c2.write(v2);
        c3.write(v3);
    }
    for chunk in chunks {
        let val = xts_encrypt_chunk(backend, chunk.read(), tweak);
        chunk.write(val);
        tweak.update();
    }
}

fn xts_decrypt_chunk<B: BlockCipherDecBackend<BlockSize = U16>>(
    backend: &B,
    mut val: u128,
    tweak: &Tweak,
) -> u128 {
    // XOR ciphertext with tweak.
    val ^= tweak.0;

    let arr: &mut Array<u8, U16> = val.as_mut_bytes().try_into().unwrap();
    backend.decrypt_block(InOut::from(arr));

    // XOR plaintext with tweak.
    val ^ tweak.0
}

fn xts_decrypt_buffer_out_of_place<B: BlockCipherDecBackend<BlockSize = U16>>(
    backend: &B,
    src: PtrByteSlice<'_>,
    mut dst: MutPtrByteSlice<'_>,
    tweak: &mut Tweak,
) {
    debug_assert_eq!(src.len(), dst.len());
    debug_assert_eq!(src.as_ptr() as usize % 64, 0);
    debug_assert_eq!(dst.as_ptr() as usize % 64, 0);
    debug_assert_eq!(src.len() % 64, 0);

    let mut src_chunks = src.iter_as::<u128>();
    let mut dst_chunks = dst.iter_as_mut::<u128>();

    while let Some(s0) = src_chunks.next() {
        let d0 = dst_chunks.next().unwrap();
        // Zero the 64-byte destination cache line on ARM64 ("DC ZVA") to avoid fetching
        // stale destination cache lines from RAM before writing out-of-place outputs.
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!(
                "dc zva, {0}",
                in(reg) d0.as_ptr(),
                options(nostack, preserves_flags),
            );
        }
        let val0 = xts_decrypt_chunk(backend, s0, tweak);
        d0.write(val0);
        tweak.update();

        let s1 = src_chunks.next().unwrap();
        let d1 = dst_chunks.next().unwrap();
        let val1 = xts_decrypt_chunk(backend, s1, tweak);
        d1.write(val1);
        tweak.update();

        let s2 = src_chunks.next().unwrap();
        let d2 = dst_chunks.next().unwrap();
        let val2 = xts_decrypt_chunk(backend, s2, tweak);
        d2.write(val2);
        tweak.update();

        let s3 = src_chunks.next().unwrap();
        let d3 = dst_chunks.next().unwrap();
        let val3 = xts_decrypt_chunk(backend, s3, tweak);
        d3.write(val3);
        tweak.update();
    }
}

fn xts_decrypt_buffer<B: BlockCipherDecBackend<BlockSize = U16>>(
    backend: &B,
    src: PtrByteSlice<'_>,
    mut dst: MutPtrByteSlice<'_>,
    tweak: &mut Tweak,
) {
    debug_assert_eq!(src.len(), dst.len());
    debug_assert!(src.as_ptr().cast::<u128>().is_aligned());
    debug_assert!(dst.as_ptr().cast::<u128>().is_aligned());

    let src_chunks = src.iter_as::<u128>();
    let dst_chunks = dst.iter_as_mut::<u128>();

    for (src_chunk, dst_chunk) in src_chunks.zip(dst_chunks) {
        let val = xts_decrypt_chunk(backend, src_chunk, tweak);
        dst_chunk.write(val);
        tweak.update();
    }
}

#[inline]
fn xts_decrypt_buffer_in_place<B: BlockCipherDecBackend<BlockSize = U16>>(
    backend: &B,
    mut buf: MutPtrByteSlice<'_>,
    tweak: &mut Tweak,
) {
    debug_assert!(buf.as_ptr().cast::<u128>().is_aligned());

    let mut chunks = buf.iter_as_mut::<u128>();
    while chunks.len() >= 4 {
        let c0 = chunks.next().unwrap();
        let c1 = chunks.next().unwrap();
        let c2 = chunks.next().unwrap();
        let c3 = chunks.next().unwrap();

        let s0 = c0.read();
        let s1 = c1.read();
        let s2 = c2.read();
        let s3 = c3.read();

        let t0 = *tweak;
        tweak.update();
        let t1 = *tweak;
        tweak.update();
        let t2 = *tweak;
        tweak.update();
        let t3 = *tweak;
        tweak.update();

        let v0 = xts_decrypt_chunk(backend, s0, &t0);
        let v1 = xts_decrypt_chunk(backend, s1, &t1);
        let v2 = xts_decrypt_chunk(backend, s2, &t2);
        let v3 = xts_decrypt_chunk(backend, s3, &t3);

        c0.write(v0);
        c1.write(v1);
        c2.write(v2);
        c3.write(v3);
    }
    for chunk in chunks {
        let val = xts_decrypt_chunk(backend, chunk.read(), tweak);
        chunk.write(val);
        tweak.update();
    }
}

impl<'a, 'b> XtsProcessor<'a, 'b> {
    /// `tweak` should be encrypted. `src` and `dst` must have the same length, be 64-byte
    /// aligned, and length must be a multiple of 64 bytes.
    pub fn new(tweak: Tweak, src: PtrByteSlice<'a>, dst: MutPtrByteSlice<'b>) -> Self {
        assert_eq!(src.len(), dst.len(), "Source and destination lengths must match");
        assert_eq!(src.len() % 64, 0, "length must be a multiple of 64 bytes");
        assert_eq!(src.as_ptr() as usize % 64, 0, "src must be 64-byte aligned");
        assert_eq!(dst.as_ptr() as usize % 64, 0, "dst must be 64-byte aligned");
        #[cfg(target_arch = "aarch64")]
        assert_dcz_block_size_is_64();
        Self { tweak, src, dst }
    }
}

impl BlockSizeUser for XtsProcessor<'_, '_> {
    type BlockSize = U16;
}

impl BlockCipherEncClosure for XtsProcessor<'_, '_> {
    fn call<B: BlockCipherEncBackend<BlockSize = Self::BlockSize>>(self, backend: &B) {
        let Self { mut tweak, src, dst } = self;
        xts_encrypt_buffer_out_of_place(backend, src, dst, &mut tweak);
    }
}

impl BlockCipherDecClosure for XtsProcessor<'_, '_> {
    fn call<B: BlockCipherDecBackend<BlockSize = Self::BlockSize>>(self, backend: &B) {
        let Self { mut tweak, src, dst } = self;
        xts_decrypt_buffer_out_of_place(backend, src, dst, &mut tweak);
    }
}

impl<'a> XtsInPlaceProcessor<'a> {
    /// Creates an XtsInPlaceProcessor for in-place operation on a single buffer.
    pub fn new(tweak: Tweak, buf: MutPtrByteSlice<'a>) -> Self {
        assert!(buf.as_ptr().cast::<u128>().is_aligned(), "buf must be 16 byte aligned");
        Self { tweak, buf }
    }
}

impl BlockSizeUser for XtsInPlaceProcessor<'_> {
    type BlockSize = U16;
}

impl BlockCipherEncClosure for XtsInPlaceProcessor<'_> {
    fn call<B: BlockCipherEncBackend<BlockSize = Self::BlockSize>>(self, backend: &B) {
        let Self { mut tweak, buf } = self;
        xts_encrypt_buffer_in_place(backend, buf, &mut tweak);
    }
}

impl BlockCipherDecClosure for XtsInPlaceProcessor<'_> {
    fn call<B: BlockCipherDecBackend<BlockSize = Self::BlockSize>>(self, backend: &B) {
        let Self { mut tweak, buf } = self;
        xts_decrypt_buffer_in_place(backend, buf, &mut tweak);
    }
}

/// Handles ciphertext stealing in order to allow non-BlockSize lengths to be used as long as
/// they are >= BlockSize. To be used with encrypt|decrypt_with_backend for out-of-place
/// operation. Conforms to IEEE 1619-2007.
pub struct XtsCtsProcessor<'a, 'b> {
    tweak: Tweak,
    src: PtrByteSlice<'a>,
    dst: MutPtrByteSlice<'b>,
}

/// Handles ciphertext stealing in order to allow non-BlockSize lengths to be used as long as
/// they are >= BlockSize. To be used with encrypt|decrypt_with_backend for in-place operation.
/// Conforms to IEEE 1619-2007.
pub struct XtsCtsInPlaceProcessor<'a> {
    tweak: Tweak,
    buf: MutPtrByteSlice<'a>,
}

impl BlockSizeUser for XtsCtsProcessor<'_, '_> {
    type BlockSize = U16;
}

impl BlockSizeUser for XtsCtsInPlaceProcessor<'_> {
    type BlockSize = U16;
}

impl<'a, 'b> XtsCtsProcessor<'a, 'b> {
    /// `tweak` should be encrypted. `src` and `dst` must have the same length and be 16 byte
    /// aligned.
    #[inline]
    pub fn new(tweak: Tweak, src: PtrByteSlice<'a>, dst: MutPtrByteSlice<'b>) -> Self {
        assert_eq!(src.len(), dst.len(), "Source and destination lengths must match");
        assert!(src.len() >= size_of::<u128>());
        assert!(src.as_ptr().cast::<u128>().is_aligned(), "src must be 16 byte aligned");
        assert!(dst.as_ptr().cast::<u128>().is_aligned(), "dst must be 16 byte aligned");
        Self { tweak, src, dst }
    }

    /// Creates a processor for in-place CTS operation on a single buffer.
    #[inline]
    pub fn new_in_place(tweak: Tweak, buf: MutPtrByteSlice<'a>) -> XtsCtsInPlaceProcessor<'a> {
        XtsCtsInPlaceProcessor::new(tweak, buf)
    }
}

impl<'a> XtsCtsInPlaceProcessor<'a> {
    /// `tweak` should be encrypted. `buf` must have length >= 16 bytes and be 16 byte aligned.
    #[inline]
    pub fn new(tweak: Tweak, buf: MutPtrByteSlice<'a>) -> Self {
        assert!(buf.len() >= size_of::<u128>());
        assert!(buf.as_ptr().cast::<u128>().is_aligned(), "buf must be 16 byte aligned");
        Self { tweak, buf }
    }
}

/// Encrypts the final `16 + extra` bytes (`1 <= extra < 16`) in place within `tail`.
#[inline]
fn cts_encrypt_tail<B: BlockCipherEncBackend<BlockSize = U16>>(
    backend: &B,
    tail: &mut [u8; 32],
    extra: usize,
    mut tweak: Tweak,
) {
    debug_assert!((1..16).contains(&extra));
    let last_full_block = u128::from_le_bytes(tail[..16].try_into().unwrap());
    let intermediate_cipher_bytes =
        xts_encrypt_chunk(backend, last_full_block, &tweak).to_le_bytes();

    // Construct combined block from the partial plaintext followed by the tail of the
    // intermediate ciphertext, and write the stolen ciphertext prefix to the partial slot.
    let mut combined_block_bytes = intermediate_cipher_bytes;
    combined_block_bytes[..extra].copy_from_slice(&tail[16..16 + extra]);
    tail[16..16 + extra].copy_from_slice(&intermediate_cipher_bytes[..extra]);

    tweak.update();
    let full_ciphertext =
        xts_encrypt_chunk(backend, u128::from_le_bytes(combined_block_bytes), &tweak);
    tail[..16].copy_from_slice(&full_ciphertext.to_le_bytes());
}

/// Decrypts the final `16 + extra` bytes (`1 <= extra < 16`) in place within `tail`.
#[inline]
fn cts_decrypt_tail<B: BlockCipherDecBackend<BlockSize = U16>>(
    backend: &B,
    tail: &mut [u8; 32],
    extra: usize,
    last_full_block_tweak: Tweak,
) {
    debug_assert!((1..16).contains(&extra));
    let mut tweak = last_full_block_tweak;
    tweak.update();

    let full_ciphertext = u128::from_le_bytes(tail[..16].try_into().unwrap());
    let combined_bytes = xts_decrypt_chunk(backend, full_ciphertext, &tweak).to_le_bytes();

    // Reconstruct the intermediate ciphertext from the partial ciphertext followed by the tail of
    // `combined_bytes`, and write the recovered partial plaintext to the partial slot.
    let mut intermediate_bytes = combined_bytes;
    intermediate_bytes[..extra].copy_from_slice(&tail[16..16 + extra]);
    tail[16..16 + extra].copy_from_slice(&combined_bytes[..extra]);

    let val =
        xts_decrypt_chunk(backend, u128::from_le_bytes(intermediate_bytes), &last_full_block_tweak);
    tail[..16].copy_from_slice(&val.to_le_bytes());
}

impl BlockCipherEncClosure for XtsCtsProcessor<'_, '_> {
    #[inline]
    fn call<B: BlockCipherEncBackend<BlockSize = Self::BlockSize>>(self, backend: &B) {
        let Self { mut tweak, src, mut dst } = self;
        let len = src.len();
        let base_size = len & (!0x0F);

        // Fast path for non-CTS (len is an exact multiple of 16):
        if len == base_size {
            xts_encrypt_buffer(backend, src, dst, &mut tweak);
            return;
        }

        // All but the last two blocks are normal.
        let tail_offset = base_size - 16;
        if tail_offset > 0 {
            xts_encrypt_buffer(
                backend,
                src.subslice(0..tail_offset),
                dst.subslice_mut(0..tail_offset),
                &mut tweak,
            );
        }

        let tail_len = len - tail_offset;
        let mut tail = [0u8; 32];
        src.subslice(tail_offset..len).copy_to_slice(&mut tail[..tail_len]);
        cts_encrypt_tail(backend, &mut tail, len & 0x0F, tweak);
        dst.subslice_mut(tail_offset..len).copy_from_slice(&tail[..tail_len]);
    }
}

impl BlockCipherDecClosure for XtsCtsProcessor<'_, '_> {
    #[inline]
    fn call<B: BlockCipherDecBackend<BlockSize = Self::BlockSize>>(self, backend: &B) {
        let Self { mut tweak, src, mut dst } = self;
        let len = src.len();
        let base_size = len & (!0x0F);

        // Fast path for non-CTS (len is an exact multiple of 16):
        if len == base_size {
            xts_decrypt_buffer(backend, src, dst, &mut tweak);
            return;
        }

        // All but the last two blocks are normal.
        let tail_offset = base_size - 16;
        if tail_offset > 0 {
            xts_decrypt_buffer(
                backend,
                src.subslice(0..tail_offset),
                dst.subslice_mut(0..tail_offset),
                &mut tweak,
            );
        }

        let tail_len = len - tail_offset;
        let mut tail = [0u8; 32];
        src.subslice(tail_offset..len).copy_to_slice(&mut tail[..tail_len]);
        cts_decrypt_tail(backend, &mut tail, len & 0x0F, tweak);
        dst.subslice_mut(tail_offset..len).copy_from_slice(&tail[..tail_len]);
    }
}

impl BlockCipherEncClosure for XtsCtsInPlaceProcessor<'_> {
    #[inline]
    fn call<B: BlockCipherEncBackend<BlockSize = Self::BlockSize>>(self, backend: &B) {
        let Self { mut tweak, mut buf } = self;
        let len = buf.len();
        let base_size = len & (!0x0F);

        // Fast path for non-CTS (len is an exact multiple of 16):
        if len == base_size {
            xts_encrypt_buffer_in_place(backend, buf, &mut tweak);
            return;
        }

        // All but the last two blocks are normal.
        let tail_offset = base_size - 16;
        if tail_offset > 0 {
            xts_encrypt_buffer_in_place(backend, buf.subslice_mut(0..tail_offset), &mut tweak);
        }

        let tail_len = len - tail_offset;
        let mut tail = [0u8; 32];
        buf.subslice_mut(tail_offset..len).copy_to_slice(&mut tail[..tail_len]);
        cts_encrypt_tail(backend, &mut tail, len & 0x0F, tweak);
        buf.subslice_mut(tail_offset..len).copy_from_slice(&tail[..tail_len]);
    }
}

impl BlockCipherDecClosure for XtsCtsInPlaceProcessor<'_> {
    #[inline]
    fn call<B: BlockCipherDecBackend<BlockSize = Self::BlockSize>>(self, backend: &B) {
        let Self { mut tweak, mut buf } = self;
        let len = buf.len();
        let base_size = len & (!0x0F);

        // Fast path for non-CTS (len is an exact multiple of 16):
        if len == base_size {
            xts_decrypt_buffer_in_place(backend, buf, &mut tweak);
            return;
        }

        // All but the last two blocks are normal.
        let tail_offset = base_size - 16;
        if tail_offset > 0 {
            xts_decrypt_buffer_in_place(backend, buf.subslice_mut(0..tail_offset), &mut tweak);
        }

        let tail_len = len - tail_offset;
        let mut tail = [0u8; 32];
        buf.subslice_mut(tail_offset..len).copy_to_slice(&mut tail[..tail_len]);
        cts_decrypt_tail(backend, &mut tail, len & 0x0F, tweak);
        buf.subslice_mut(tail_offset..len).copy_from_slice(&tail[..tail_len]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cipher::inout::InOut;
    use cipher::typenum::consts::U1;
    use cipher::{Block, ParBlocksSizeUser};
    use std::cell::RefCell;
    use test_case::test_case;
    use zerocopy::{FromBytes, Immutable, IntoBytes};

    struct MockCipher {
        recorded_blocks: RefCell<Vec<u128>>,
        key: u128,
    }

    impl MockCipher {
        fn new(key: u128) -> Self {
            Self { recorded_blocks: RefCell::new(Vec::new()), key }
        }
    }

    impl BlockSizeUser for MockCipher {
        type BlockSize = U16;
    }

    impl ParBlocksSizeUser for MockCipher {
        type ParBlocksSize = U1;
    }

    impl BlockCipherEncBackend for MockCipher {
        fn encrypt_block(&self, mut block: InOut<'_, '_, Block<Self>>) {
            // SAFETY: Block<Self> is Array<u8, U16>, which is 16 bytes.
            let mut val =
                unsafe { std::ptr::read_unaligned(block.get_in().as_ptr() as *const u128) };
            self.recorded_blocks.borrow_mut().push(val);
            val ^= self.key;
            // SAFETY: Block<Self> is Array<u8, U16>, which is 16 bytes.
            unsafe {
                std::ptr::write_unaligned(block.get_out().as_mut_ptr() as *mut u128, val);
            }
        }
    }

    impl BlockCipherDecBackend for MockCipher {
        fn decrypt_block(&self, mut block: InOut<'_, '_, Block<Self>>) {
            // SAFETY: Block<Self> is Array<u8, U16>, which is 16 bytes.
            let mut val =
                unsafe { std::ptr::read_unaligned(block.get_in().as_ptr() as *const u128) };
            self.recorded_blocks.borrow_mut().push(val);
            // Lazy scramble. See above for the encryption method.
            val = val.rotate_right(3);
            val ^= self.key;
            // SAFETY: Block<Self> is Array<u8, U16>, which is 16 bytes.
            unsafe {
                std::ptr::write_unaligned(block.get_out().as_mut_ptr() as *mut u128, val);
            }
        }
    }

    /// A simple fake scrambler that includes a rotate left to keep the XOR from canceling itself
    /// out.
    struct MockNonLinearCipher {
        key: u128,
    }

    impl MockNonLinearCipher {
        fn new(key: u128) -> Self {
            Self { key }
        }
    }

    impl BlockSizeUser for MockNonLinearCipher {
        type BlockSize = U16;
    }

    impl ParBlocksSizeUser for MockNonLinearCipher {
        type ParBlocksSize = U1;
    }

    impl BlockCipherEncBackend for MockNonLinearCipher {
        fn encrypt_block(&self, mut block: InOut<'_, '_, Block<Self>>) {
            let mut val =
                unsafe { std::ptr::read_unaligned(block.get_in().as_ptr() as *const u128) };
            val = val.rotate_left(11) ^ self.key;
            unsafe {
                std::ptr::write_unaligned(block.get_out().as_mut_ptr() as *mut u128, val);
            }
        }
    }

    impl BlockCipherDecBackend for MockNonLinearCipher {
        fn decrypt_block(&self, mut block: InOut<'_, '_, Block<Self>>) {
            let mut val =
                unsafe { std::ptr::read_unaligned(block.get_in().as_ptr() as *const u128) };
            val = (val ^ self.key).rotate_right(11);
            unsafe {
                std::ptr::write_unaligned(block.get_out().as_mut_ptr() as *mut u128, val);
            }
        }
    }

    #[repr(C)]
    #[derive(FromBytes, IntoBytes, Immutable)]
    struct Blocks<const N: usize>([u128; N]);

    #[repr(C, align(64))]
    struct Aligned64<T>(T);

    static_assertions::const_assert!(std::mem::align_of::<Aligned64<Blocks<1>>>() == 64);

    impl<const N: usize> Default for Blocks<N> {
        fn default() -> Self {
            Self([0u128; N])
        }
    }

    #[test]
    fn test_xts_out_of_place() {
        let mut plaintext: Aligned64<Blocks<4>> = Aligned64(Default::default());
        for (i, x) in plaintext.0.as_mut_bytes().iter_mut().enumerate() {
            *x = i as u8;
        }
        let mut ciphertext: Aligned64<Blocks<4>> = Aligned64(Default::default());

        let src = PtrByteSlice::from(plaintext.0.as_bytes());
        let dst = MutPtrByteSlice::from(ciphertext.0.as_mut_bytes());

        let tweak_val = 0x123456789abcdef0123456789abcdef0u128;
        let tweak = Tweak::new(tweak_val);
        let key = 0xffeeddccbbaa99887766554433221100u128;

        let processor = XtsProcessor::new(tweak, src, dst);
        let cipher = MockCipher::new(key);

        BlockCipherEncClosure::call(processor, &cipher);

        // Verify ciphertext.
        // Since our mock cipher is just XOR with key, the tweak should cancel out.
        // C = P ^ K.
        let expected_c0 =
            u128::from_le_bytes(plaintext.0.as_bytes()[0..16].try_into().unwrap()) ^ key;
        let expected_c1 =
            u128::from_le_bytes(plaintext.0.as_bytes()[16..32].try_into().unwrap()) ^ key;

        let actual_c0 = u128::from_le_bytes(ciphertext.0.as_bytes()[0..16].try_into().unwrap());
        let actual_c1 = u128::from_le_bytes(ciphertext.0.as_bytes()[16..32].try_into().unwrap());

        assert_eq!(actual_c0, expected_c0);
        assert_eq!(actual_c1, expected_c1);

        // Verify recorded blocks (should be P ^ T).
        assert_eq!(cipher.recorded_blocks.borrow().len(), 4);

        let p0 = u128::from_le_bytes(plaintext.0.as_bytes()[0..16].try_into().unwrap());
        let p1 = u128::from_le_bytes(plaintext.0.as_bytes()[16..32].try_into().unwrap());

        let mut t0 = tweak;
        assert_eq!(cipher.recorded_blocks.borrow()[0], p0 ^ t0.0);
        t0.update();
        assert_eq!(cipher.recorded_blocks.borrow()[1], p1 ^ t0.0);
    }

    #[test]
    fn test_xts_in_place() {
        let mut buf: Blocks<2> = Default::default();
        for (i, x) in buf.as_mut_bytes().iter_mut().enumerate() {
            *x = i as u8;
        }

        let tweak_val = 0x123456789abcdef0123456789abcdef0u128;
        let tweak = Tweak::new(tweak_val);
        let key = 0xffeeddccbbaa99887766554433221100u128;

        // Save original plaintext for verification.
        let p0 = u128::from_le_bytes(buf.as_bytes()[0..16].try_into().unwrap());
        let p1 = u128::from_le_bytes(buf.as_bytes()[16..32].try_into().unwrap());

        let slice = MutPtrByteSlice::from(buf.as_mut_bytes());
        let processor = XtsInPlaceProcessor::new(tweak, slice);
        let cipher = MockCipher::new(key);

        BlockCipherEncClosure::call(processor, &cipher);

        // Verify in-place ciphertext.
        let expected_c0 = p0 ^ key;
        let expected_c1 = p1 ^ key;

        let actual_c0 = u128::from_le_bytes(buf.as_bytes()[0..16].try_into().unwrap());
        let actual_c1 = u128::from_le_bytes(buf.as_bytes()[16..32].try_into().unwrap());

        assert_eq!(actual_c0, expected_c0);
        assert_eq!(actual_c1, expected_c1);

        // Verify recorded blocks.
        assert_eq!(cipher.recorded_blocks.borrow().len(), 2);
        let mut t0 = tweak;
        assert_eq!(cipher.recorded_blocks.borrow()[0], p0 ^ t0.0);
        t0.update();
        assert_eq!(cipher.recorded_blocks.borrow()[1], p1 ^ t0.0);

        // Round-trip encrypt + decrypt in-place with MockNonLinearCipher.
        let orig = buf.as_bytes().to_vec();
        let nl_cipher = MockNonLinearCipher::new(key);
        BlockCipherEncClosure::call(
            XtsInPlaceProcessor::new(tweak, MutPtrByteSlice::from(buf.as_mut_bytes())),
            &nl_cipher,
        );
        assert_ne!(buf.as_bytes(), &orig[..]);
        BlockCipherDecClosure::call(
            XtsInPlaceProcessor::new(tweak, MutPtrByteSlice::from(buf.as_mut_bytes())),
            &nl_cipher,
        );
        assert_eq!(buf.as_bytes(), &orig[..]);
    }

    #[test_case(16; "exact_block")]
    #[test_case(17; "one_byte_more_than_block")]
    #[test_case(31; "one_byte_less_than_two_blocks")]
    #[test_case(32; "exact_two_blocks")]
    #[test_case(80; "bunch_of_blocks_exact")]
    #[test_case(87; "bunch_of_blocks_cts")]
    fn test_cts_encrypt_decrypt(len: usize) {
        let tweak = Tweak::new(0x123456789abcdef0123456789abcdef0);
        let key = 0xffeeddccbbaa99887766554433221100;

        let mut plaintext_vec = vec![0u128; (len + 15) / 16];
        let plaintext_bytes = plaintext_vec.as_mut_bytes();
        for (i, b) in plaintext_bytes[..len].iter_mut().enumerate() {
            *b = ((i % 255) + 1) as u8;
        }
        let plaintext = &plaintext_bytes[..len];

        let mut ciphertext_vec = vec![0u128; (len + 15) / 16];
        let ciphertext_bytes = ciphertext_vec.as_mut_bytes();

        let mut decrypted_vec = vec![0u128; (len + 15) / 16];
        let decrypted_bytes = decrypted_vec.as_mut_bytes();

        let cipher = MockNonLinearCipher::new(key);

        // Encrypt out-of-place
        {
            let src = PtrByteSlice::from(plaintext);
            let dst = MutPtrByteSlice::from(&mut ciphertext_bytes[..len]);
            let processor = XtsCtsProcessor::new(tweak, src, dst);
            BlockCipherEncClosure::call(processor, &cipher);
        }

        let ciphertext = &ciphertext_bytes[..len];

        // Verify none of the original blocks/content are intact
        assert_ne!(ciphertext, plaintext, "Ciphertext should not match plaintext");
        for chunk_start in (0..len).step_by(16) {
            let chunk_end = (chunk_start + 16).min(len);
            assert_ne!(
                &ciphertext[chunk_start..chunk_end],
                &plaintext[chunk_start..chunk_end],
                "Chunk at {chunk_start}..{chunk_end} should not match plaintext"
            );
        }

        // Decrypt out-of-place
        {
            let src = PtrByteSlice::from(ciphertext);
            let dst = MutPtrByteSlice::from(&mut decrypted_bytes[..len]);
            let processor = XtsCtsProcessor::new(tweak, src, dst);
            BlockCipherDecClosure::call(processor, &cipher);
        }

        let decrypted = &decrypted_bytes[..len];

        // Verify decrypted matches original plaintext
        assert_eq!(decrypted, plaintext, "Decrypted text should match original plaintext");
    }

    #[test_case(16; "exact_block")]
    #[test_case(17; "one_byte_more_than_block")]
    #[test_case(31; "one_byte_less_than_two_blocks")]
    #[test_case(32; "exact_two_blocks")]
    #[test_case(80; "bunch_of_blocks_exact")]
    #[test_case(87; "bunch_of_blocks_cts")]
    fn test_cts_encrypt_decrypt_in_place(len: usize) {
        let tweak = Tweak::new(0x123456789abcdef0123456789abcdef0);
        let key = 0xffeeddccbbaa99887766554433221100;

        let mut buf_vec = vec![0u128; (len + 15) / 16];
        let buf_bytes = buf_vec.as_mut_bytes();
        for (i, b) in buf_bytes[..len].iter_mut().enumerate() {
            *b = ((i % 255) + 1) as u8;
        }
        let original_plaintext = buf_bytes[..len].to_vec();

        let cipher = MockNonLinearCipher::new(key);

        // Encrypt in-place
        {
            let slice = MutPtrByteSlice::from(&mut buf_bytes[..len]);
            let processor = XtsCtsProcessor::new_in_place(tweak, slice);
            BlockCipherEncClosure::call(processor, &cipher);
        }

        // Verify in-place ciphertext differs from original plaintext
        assert_ne!(
            &buf_bytes[..len],
            &original_plaintext[..],
            "In-place ciphertext should not match plaintext"
        );
        for chunk_start in (0..len).step_by(16) {
            let chunk_end = (chunk_start + 16).min(len);
            assert_ne!(
                &buf_bytes[chunk_start..chunk_end],
                &original_plaintext[chunk_start..chunk_end],
                "Chunk at {chunk_start}..{chunk_end} should not match plaintext"
            );
        }

        // Decrypt in-place
        {
            let slice = MutPtrByteSlice::from(&mut buf_bytes[..len]);
            let processor = XtsCtsProcessor::new_in_place(tweak, slice);
            BlockCipherDecClosure::call(processor, &cipher);
        }

        // Verify decrypted matches original plaintext
        assert_eq!(
            &buf_bytes[..len],
            &original_plaintext[..],
            "Decrypted in-place text should match original plaintext"
        );
    }

    #[test_case(4; "four_blocks")]
    #[test_case(8; "eight_blocks")]
    fn test_cts_matches_normal_xts_on_exact_blocks(num_blocks: usize) {
        let tweak = Tweak::new(0x9876543210abcdef9876543210abcdef);
        let key = 0x0123456789abcdef0123456789abcdef;

        let len = num_blocks * 16;
        let mut plaintext_vec = vec![0u128; num_blocks + 4];
        let addr = plaintext_vec.as_ptr() as usize;
        let offset = (64 - (addr % 64)) % 64;
        let plaintext_bytes = &mut plaintext_vec.as_mut_bytes()[offset..offset + len];
        for (i, b) in plaintext_bytes.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(17).wrapping_add(3);
        }

        let mut normal_cts_vec = vec![0u128; num_blocks + 4];
        let normal_addr = normal_cts_vec.as_ptr() as usize;
        let normal_offset = (64 - (normal_addr % 64)) % 64;
        let normal_cts_bytes =
            &mut normal_cts_vec.as_mut_bytes()[normal_offset..normal_offset + len];

        let mut cts_vec = vec![0u128; num_blocks + 4];
        let cts_addr = cts_vec.as_ptr() as usize;
        let cts_offset = (64 - (cts_addr % 64)) % 64;
        let cts_bytes = &mut cts_vec.as_mut_bytes()[cts_offset..cts_offset + len];

        let cipher = MockNonLinearCipher::new(key);

        // Normal XTS encryption
        {
            let src = PtrByteSlice::from(&*plaintext_bytes);
            let dst = MutPtrByteSlice::from(&mut *normal_cts_bytes);
            let processor = XtsProcessor::new(tweak, src, dst);
            BlockCipherEncClosure::call(processor, &cipher);
        }

        // CTS XTS encryption
        {
            let src = PtrByteSlice::from(&*plaintext_bytes);
            let dst = MutPtrByteSlice::from(&mut *cts_bytes);
            let processor = XtsCtsProcessor::new(tweak, src, dst);
            BlockCipherEncClosure::call(processor, &cipher);
        }

        assert_eq!(
            normal_cts_bytes, cts_bytes,
            "CTS XTS should match normal XTS for {num_blocks} blocks"
        );
    }

    #[test_case(1; "one_block")]
    #[test_case(2; "two_blocks")]
    #[test_case(5; "five_blocks")]
    fn test_cts_matches_inplace_xts_on_exact_blocks(num_blocks: usize) {
        let tweak = Tweak::new(0x9876543210abcdef9876543210abcdef);
        let key = 0x0123456789abcdef0123456789abcdef;

        let len = num_blocks * 16;
        let mut plaintext_vec = vec![0u128; num_blocks];
        let plaintext_bytes = plaintext_vec.as_mut_bytes();
        for (i, b) in plaintext_bytes[..len].iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(17).wrapping_add(3);
        }
        let plaintext = &plaintext_bytes[..len];

        let mut normal_cts_vec = vec![0u128; num_blocks];
        let mut cts_vec = vec![0u128; num_blocks];

        let cipher = MockNonLinearCipher::new(key);

        // In-place XTS encryption
        {
            let mut slice = MutPtrByteSlice::from(&mut normal_cts_vec.as_mut_bytes()[..len]);
            slice.copy_from_slice(plaintext);
            let processor = XtsInPlaceProcessor::new(tweak, slice);
            BlockCipherEncClosure::call(processor, &cipher);
        }

        // CTS XTS encryption
        {
            let src = PtrByteSlice::from(plaintext);
            let dst = MutPtrByteSlice::from(&mut cts_vec.as_mut_bytes()[..len]);
            let processor = XtsCtsProcessor::new(tweak, src, dst);
            BlockCipherEncClosure::call(processor, &cipher);
        }

        assert_eq!(
            normal_cts_vec, cts_vec,
            "CTS XTS should match in-place XTS for {num_blocks} blocks"
        );
    }

    #[test]
    fn test_cts_different_tweaks_different_ciphertext() {
        const LEN: usize = 87;

        let tweak1 = Tweak::new(0x123456789abcdef0123456789abcdef0);
        let tweak2 = Tweak::new(0xfeef876543210fedcba9876543210fed);
        let key = 0xffeeddccbbaa99887766554433221100;

        let mut plaintext_vec = vec![0u128; (LEN + 15) / 16];
        let plaintext_bytes = plaintext_vec.as_mut_bytes();
        for (i, b) in plaintext_bytes[..LEN].iter_mut().enumerate() {
            *b = ((i % 255) + 1) as u8;
        }
        let plaintext = &plaintext_bytes[..LEN];

        let mut ciphertext1_vec = vec![0u128; (LEN + 15) / 16];
        let ciphertext1_bytes = ciphertext1_vec.as_mut_bytes();

        let mut ciphertext2_vec = vec![0u128; (LEN + 15) / 16];
        let ciphertext2_bytes = ciphertext2_vec.as_mut_bytes();

        let cipher = MockNonLinearCipher::new(key);

        // Encrypt with tweak1
        {
            let src = PtrByteSlice::from(plaintext);
            let dst = MutPtrByteSlice::from(&mut ciphertext1_bytes[..LEN]);
            let processor = XtsCtsProcessor::new(tweak1, src, dst);
            BlockCipherEncClosure::call(processor, &cipher);
        }

        // Encrypt with tweak2
        {
            let src = PtrByteSlice::from(plaintext);
            let dst = MutPtrByteSlice::from(&mut ciphertext2_bytes[..LEN]);
            let processor = XtsCtsProcessor::new(tweak2, src, dst);
            BlockCipherEncClosure::call(processor, &cipher);
        }

        assert_ne!(
            &ciphertext1_bytes[..LEN],
            &ciphertext2_bytes[..LEN],
            "Different tweaks should produce different ciphertexts"
        );
    }

    #[test]
    #[should_panic(expected = "Source and destination lengths must match")]
    fn test_cts_panic_length_mismatch() {
        let tweak = Tweak::new(0);
        let src_buf = vec![0u128; 2];
        let mut dst_buf = vec![0u128; 3];
        let src = PtrByteSlice::from(src_buf.as_bytes());
        let dst = MutPtrByteSlice::from(dst_buf.as_mut_bytes());
        let _ = XtsCtsProcessor::new(tweak, src, dst);
    }

    #[test]
    #[should_panic]
    fn test_cts_panic_too_short() {
        let tweak = Tweak::new(0);
        let src_buf = vec![0u128; 2];
        let mut dst_buf = vec![0u128; 2];
        let src = PtrByteSlice::from(&src_buf.as_bytes()[..15]);
        let dst = MutPtrByteSlice::from(&mut dst_buf.as_mut_bytes()[..15]);
        let _ = XtsCtsProcessor::new(tweak, src, dst);
    }

    #[test]
    #[should_panic(expected = "src must be 16 byte aligned")]
    fn test_cts_panic_unaligned_src() {
        let tweak = Tweak::new(0);
        let src_buf = vec![0u128; 3];
        let mut dst_buf = vec![0u128; 2];
        let src = PtrByteSlice::from(&src_buf.as_bytes()[1..17]);
        let dst = MutPtrByteSlice::from(&mut dst_buf.as_mut_bytes()[..16]);
        let _ = XtsCtsProcessor::new(tweak, src, dst);
    }

    #[test]
    #[should_panic(expected = "dst must be 16 byte aligned")]
    fn test_cts_panic_unaligned_dst() {
        let tweak = Tweak::new(0);
        let src_buf = vec![0u128; 2];
        let mut dst_buf = vec![0u128; 3];
        let src = PtrByteSlice::from(&src_buf.as_bytes()[..16]);
        let dst = MutPtrByteSlice::from(&mut dst_buf.as_mut_bytes()[1..17]);
        let _ = XtsCtsProcessor::new(tweak, src, dst);
    }
}
