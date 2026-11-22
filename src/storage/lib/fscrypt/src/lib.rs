// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
pub mod direntry;
pub mod hkdf;
pub mod proxy_filename;

use aes::cipher::inout::InOutBuf;
use aes::cipher::{
    Block, BlockCipherDecrypt, BlockModeDecrypt, BlockModeEncrypt, KeyInit, KeyIvInit,
};
use anyhow::{Error, anyhow, ensure};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

pub const POLICY_FLAGS_PAD_16: u8 = 0x02;
pub const POLICY_FLAGS_INO_LBLK_32: u8 = 0x10;
const SUPPORTED_POLICY_FLAGS: u8 = POLICY_FLAGS_PAD_16 | POLICY_FLAGS_INO_LBLK_32;

pub const ENCRYPTION_MODE_AES_256_XTS: u8 = 1;
pub const ENCRYPTION_MODE_AES_256_CTS: u8 = 4;

// TODO(https://fxbug.dev/375700939): Support different padding sizes based on
// SET_ENCRYPTION_POLICY flags.
pub const FSCRYPT_PADDING: usize = 16;
pub const MAX_FILENAME_LEN: usize = 255;
pub const MAX_SYMLINK_LEN: usize = 4093;

/// An encryption context is written as an xattr to the directory root of each FBE hierarchy.
/// For f2fs, this is stored with index 9 and name "c".
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, Immutable, KnownLayout, FromBytes, IntoBytes, Unaligned)]
pub struct Context {
    pub version: u8,                   // = 2
    pub contents_encryption_mode: u8,  // = ENCRYPTION_MODE_AES_256_XTS
    pub filenames_encryption_mode: u8, // = ENCRYPTION_MODE_AES_256_CTS
    pub flags: u8,                     // = POLICY_FLAGS_*
    pub log2_data_unit_size: u8,       // = 0
    _reserved: [u8; 3],
    pub main_key_identifier: [u8; 16],
    pub nonce: [u8; 16],
}

impl Context {
    pub fn try_from_bytes(raw_context: &[u8]) -> Result<Option<Self>, Error> {
        let this = Context::read_from_bytes(raw_context)
            .map_err(|_| anyhow!("Bad sized crypto context"))?;
        ensure!(this.version == 2, "Bad version number in crypto context");
        ensure!(
            this.contents_encryption_mode == ENCRYPTION_MODE_AES_256_XTS,
            "Unsupported contents_encryption_mode",
        );
        ensure!(
            this.filenames_encryption_mode == ENCRYPTION_MODE_AES_256_CTS,
            "Unsupported filenames_encryption_mode"
        );
        // We assume 16 byte zero padding.
        // We also only support standard key derivation and INO_LBLK_32, no INO_LBLK_64 or DIRECT.
        ensure!(this.flags & !SUPPORTED_POLICY_FLAGS == 0, "Unsupported flags in crypto context");
        // This controls the data unit size used for encryption blocks. We only support the default.
        ensure!(this.log2_data_unit_size == 0, "Unsupported custom DUN size");
        Ok(Some(this))
    }
}

/// Returns the identifier for a given main key.
pub fn main_key_to_identifier(main_key: &[u8; 64]) -> [u8; 16] {
    hkdf::fscrypt_hkdf::<16>(main_key, &[], 1)
}

pub struct DirectoryKeys {
    cts_key: [u8; 32],
    ino_hash_key: [u8; 16],
    dir_hash_key: [u8; 16],
}

impl DirectoryKeys {
    /// Returns the keys in concatenated form (as found in Fxfs's crypt protocol).
    pub fn to_unwrapped_key(&self) -> Vec<u8> {
        let mut keys = Vec::with_capacity(64);
        keys.extend_from_slice(&self.cts_key);
        keys.extend_from_slice(&self.ino_hash_key);
        keys.extend_from_slice(&self.dir_hash_key);
        keys
    }
}

/// Returns fscrypt directory keys (for the ino-lblk32 algorithm).  These are all the keys
/// required to encrypt file names using fscrypt.
pub fn to_directory_keys(main_key: &[u8], uuid: &[u8], nonce: &[u8]) -> DirectoryKeys {
    let mut hdkf_info = [0; 17];
    hdkf_info[0] = ENCRYPTION_MODE_AES_256_CTS;
    hdkf_info[1..17].copy_from_slice(&uuid);
    DirectoryKeys {
        cts_key: hkdf::fscrypt_hkdf(main_key, &hdkf_info, hkdf::HKDF_CONTEXT_IV_INO_LBLK_32_KEY),
        ino_hash_key: hkdf::fscrypt_hkdf(main_key, &[], hkdf::HKDF_CONTEXT_INODE_HASH_KEY),
        dir_hash_key: hkdf::fscrypt_hkdf(main_key, &nonce, hkdf::HKDF_CONTEXT_DIRHASH_KEY),
    }
}

pub struct DirectoryKeysLblk64 {
    cts_key: [u8; 32],
    dir_hash_key: [u8; 16],
}

impl DirectoryKeysLblk64 {
    /// Returns the keys in concatenated form (32-byte `cts_key` + 16-byte `dir_hash_key`).
    pub fn to_unwrapped_key(&self) -> Vec<u8> {
        let mut keys = Vec::with_capacity(48);
        keys.extend_from_slice(&self.cts_key);
        keys.extend_from_slice(&self.dir_hash_key);
        keys
    }
}

/// Returns fscrypt directory keys for the `IV_INO_LBLK_64` algorithm
/// (`HKDF_CONTEXT_IV_INO_LBLK_64_KEY = 4`).
pub fn to_directory_keys_lblk64(main_key: &[u8], uuid: &[u8], nonce: &[u8]) -> DirectoryKeysLblk64 {
    let mut hkdf_info = [0; 17];
    hkdf_info[0] = ENCRYPTION_MODE_AES_256_CTS;
    hkdf_info[1..17].copy_from_slice(uuid);
    DirectoryKeysLblk64 {
        cts_key: hkdf::fscrypt_hkdf(main_key, &hkdf_info, hkdf::HKDF_CONTEXT_IV_INO_LBLK_64_KEY),
        dir_hash_key: hkdf::fscrypt_hkdf(main_key, nonce, hkdf::HKDF_CONTEXT_DIRHASH_KEY),
    }
}

pub fn to_xts_key(main_key: &[u8], uuid: [u8; 16]) -> [u8; 64] {
    let mut hdkf_info = [0; 17];
    hdkf_info[0] = ENCRYPTION_MODE_AES_256_XTS;
    hdkf_info[1..17].copy_from_slice(&uuid);
    hkdf::fscrypt_hkdf(&main_key, &hdkf_info, hkdf::HKDF_CONTEXT_IV_INO_LBLK_32_KEY)
}

/// Encrypts a filename or symlink target in place using AES-256-CTS.
pub fn encrypt_filename_cts(
    cts_key: &[u8; 32],
    iv: &[u32; 4],
    buffer: &mut Vec<u8>,
    max_len: usize,
) -> Result<(), Error> {
    ensure!(buffer.len() <= max_len, "Filename too long");
    buffer.resize(buffer.len().next_multiple_of(FSCRYPT_PADDING), 0);

    let mut cbc = cbc::Encryptor::<aes::Aes256>::new(
        cts_key.try_into().unwrap(),
        iv.as_bytes().try_into().unwrap(),
    );
    let inout = InOutBuf::<'_, '_, u8>::from(&mut buffer[..]);
    let (mut blocks, _): (InOutBuf<'_, '_, Block<aes::Aes256>>, _) = inout.into_chunks();
    let mut chunks = blocks.get_out();
    cbc.encrypt_blocks(&mut chunks);
    if chunks.len() >= 2 {
        // We are encrypting with CTS.  In most cases, the padding will mean it's a multiple of
        // FSCRYPT_PADDING bytes, so all we need to do is swap the last two chunks.  There is one
        // exception: when the filename ends up being longer than max_len after padding.  In
        // that case, all we have to do is trim the end after swapping the last two chunks.
        chunks.swap(chunks.len() - 1, chunks.len() - 2);
        buffer.truncate(max_len);
    }
    Ok(())
}

/// Decrypts a filename or symlink target in place using AES-256-CTS.
pub fn decrypt_filename_cts(
    cts_key: &[u8; 32],
    iv: &[u32; 4],
    buffer: &mut Vec<u8>,
    max_len: usize,
) -> Result<(), Error> {
    ensure!(buffer.len() <= max_len, "Filename too long");
    let alignment = buffer.len() % FSCRYPT_PADDING;
    if alignment != 0 {
        // For CTS, the only case we need to care about is when the encrypted filename is
        // max_len bytes. In all other cases, the filename should be a multiple of FSCRYPT_PADDING
        // bytes.
        ensure!(
            buffer.len() == max_len && buffer.len() > FSCRYPT_PADDING,
            "Unexpected filename length"
        );

        // Decrypt the second to last block.
        let cipher = aes::Aes256::new(cts_key.try_into().unwrap());
        let mut out: Block<aes::Aes256> =
            buffer[max_len - alignment - FSCRYPT_PADDING..max_len - alignment].try_into().unwrap();
        cipher.decrypt_block(&mut out);

        // Copy the extra bytes we need.
        buffer.extend_from_slice(&out[alignment..]);
    }

    let mut cbc = cbc::Decryptor::<aes::Aes256>::new(
        cts_key.try_into().unwrap(),
        iv.as_bytes().try_into().unwrap(),
    );
    let inout = InOutBuf::<'_, '_, u8>::from(&mut buffer[..]);
    let (mut blocks, _): (InOutBuf<'_, '_, Block<aes::Aes256>>, _) = inout.into_chunks();
    let mut chunks = blocks.get_out();
    if chunks.len() >= 2 {
        chunks.swap(chunks.len() - 1, chunks.len() - 2);
    }
    cbc.decrypt_blocks(&mut chunks);

    // Strip padding
    while let Some(0) = buffer.last() {
        buffer.pop();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_main_key_to_identifier() {
        // Nb: Hard coded test vector from an fscrypt instance.
        let key_digest = "dc34d175ba21b27e2e92829b0dc12666ce8bfbcbae387014c6bb0d8b7678dafa6466bd7565b1a5999cd3f8a39a470528fa6816768e6985f0b10804af7d657810";
        let key: [u8; 64] = hex::decode(&key_digest).unwrap().try_into().unwrap();
        assert_eq!(hex::encode(main_key_to_identifier(&key)), "fc7f69a149f89a7529374cf9e96a6d13");
    }

    #[test]
    fn test_filename_cts_roundtrip() {
        let key = [0x42; 32];
        let iv = [0x11; 4];
        for (len, max_len) in [
            (0, MAX_FILENAME_LEN),
            (1, MAX_FILENAME_LEN),
            (16, MAX_FILENAME_LEN),
            (17, MAX_FILENAME_LEN),
            (32, MAX_FILENAME_LEN),
            (MAX_FILENAME_LEN, MAX_FILENAME_LEN),
            (MAX_SYMLINK_LEN - 8, MAX_SYMLINK_LEN),
            (MAX_SYMLINK_LEN, MAX_SYMLINK_LEN),
        ] {
            let expected: Vec<u8> = (0..len).map(|i| b'a' + (i % 26) as u8).collect();
            let mut buffer = expected.clone();
            encrypt_filename_cts(&key, &iv, &mut buffer, max_len).unwrap();
            // Short names are padded to a 16 byte multiple, then truncated to `max_len` by CTS.
            assert_eq!(buffer.len(), len.next_multiple_of(FSCRYPT_PADDING).min(max_len));
            decrypt_filename_cts(&key, &iv, &mut buffer, max_len).unwrap();
            assert_eq!(buffer, expected, "len {len}");
        }
    }

    #[test]
    fn test_filename_cts_bad_lengths() {
        let key = [0x42; 32];
        let iv = [0x11; 4];
        let mut too_long = vec![b'a'; MAX_FILENAME_LEN + 1];
        assert!(encrypt_filename_cts(&key, &iv, &mut too_long, MAX_FILENAME_LEN).is_err());
        // Inputs must fit in `max_len`, and partial blocks are only valid at exactly `max_len`
        // with a full block to steal from.
        for (len, max_len) in [
            (1, MAX_FILENAME_LEN),
            (17, MAX_FILENAME_LEN),
            (5, 5),
            (MAX_FILENAME_LEN + 1, MAX_FILENAME_LEN),
        ] {
            let mut buffer = vec![0xab; len];
            let result = decrypt_filename_cts(&key, &iv, &mut buffer, max_len);
            assert!(result.is_err(), "len {len} max_len {max_len}");
        }
    }
}
