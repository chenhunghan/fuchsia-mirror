// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::{
    BLOCK_SIZE, Cipher, MAX_FILENAME_LEN, MAX_SYMLINK_LEN, UnwrappedKey, decrypt_filename_cts,
    encrypt_filename_cts,
};
use anyhow::{Context, Error};
use storage_ptr_slice::MutPtrByteSlice;

/// Directory and symlink cipher for `FSCRYPT_POLICY_FLAG_IV_INO_LBLK_64`.
///
/// Per the Linux fscrypt specification (`fs/crypto/keysetup.c` and `fs/crypto/crypto.c`),
/// `IV_INO_LBLK_64` derives a shared per-mode `cts_key` using `HKDF_CONTEXT_IV_INO_LBLK_64_KEY`
/// (`4`) and constructs the 16-byte AES-256-CBC-CTS IV by placing the unhashed 32-bit inode
/// number in bits `32..63` (`[0, object_id as u32, 0, 0]`, with `lblk_num = 0` in bits `0..31`).
#[derive(Debug)]
pub struct FscryptInoLblk64DirCipher {
    cts_key: [u8; 32],
    dir_hash_key: [u8; 16],
}

impl FscryptInoLblk64DirCipher {
    pub fn new(key: &UnwrappedKey) -> Self {
        assert_eq!(key.len(), 48, "Expected 48-byte unwrapped key for FscryptInoLblk64DirCipher");
        Self {
            cts_key: key[..32].try_into().unwrap(),
            dir_hash_key: key[32..48].try_into().unwrap(),
        }
    }

    #[inline(always)]
    fn iv(object_id: u64) -> Result<[u32; 4], Error> {
        let ino = u32::try_from(object_id).with_context(|| {
            format!("IV_INO_LBLK_64 requires 32-bit inode numbers, got {object_id:#x}")
        })?;
        Ok([0, ino.to_le(), 0, 0])
    }
}

impl Cipher for FscryptInoLblk64DirCipher {
    fn encrypt(
        &self,
        _ino: u64,
        _attribute_id: u64,
        _device_offset: u64,
        _file_offset: u64,
        _buffer: MutPtrByteSlice<'_>,
    ) -> Result<(), Error> {
        Err(zx_status::Status::NOT_SUPPORTED).context("encrypt not supported for InoLblk64Dir")
    }

    fn decrypt(
        &self,
        _ino: u64,
        _attribute_id: u64,
        _device_offset: u64,
        _file_offset: u64,
        _buffer: MutPtrByteSlice<'_>,
    ) -> Result<(), Error> {
        Err(zx_status::Status::NOT_SUPPORTED).context("decrypt not supported for InoLblk64Dir")
    }

    fn encrypt_filename(&self, object_id: u64, buffer: &mut Vec<u8>) -> Result<(), Error> {
        let iv = Self::iv(object_id)?;
        encrypt_filename_cts(&self.cts_key, &iv, buffer, MAX_FILENAME_LEN)
    }

    fn decrypt_filename(&self, object_id: u64, buffer: &mut Vec<u8>) -> Result<(), Error> {
        let iv = Self::iv(object_id)?;
        decrypt_filename_cts(&self.cts_key, &iv, buffer, MAX_FILENAME_LEN)
    }

    fn encrypt_symlink(&self, object_id: u64, buffer: &mut Vec<u8>) -> Result<(), Error> {
        let iv = Self::iv(object_id)?;
        encrypt_filename_cts(&self.cts_key, &iv, buffer, MAX_SYMLINK_LEN)
    }

    fn decrypt_symlink(&self, object_id: u64, buffer: &mut Vec<u8>) -> Result<(), Error> {
        let iv = Self::iv(object_id)?;
        decrypt_filename_cts(&self.cts_key, &iv, buffer, MAX_SYMLINK_LEN)
    }

    fn hash_code(&self, _raw_filename: &[u8], _filename: &str) -> Option<u32> {
        None
    }

    fn hash_code_casefold(&self, filename: &str) -> u32 {
        fscrypt::direntry::casefold_encrypt_hash_filename(filename.into(), &self.dir_hash_key)
    }

    fn supports_inline_encryption(&self) -> bool {
        false
    }

    fn crypt_ctx(&self, _ino: u64, _attribute_id: u64, _file_offset: u64) -> Option<(u64, u8)> {
        None
    }
}

/// File cipher for `FSCRYPT_POLICY_FLAG_IV_INO_LBLK_64`.
///
/// Produces a 64-bit DUN with the 32-bit inode number in bits `32..63` and the 32-bit file logical
/// block number (`file_offset / 4096`) in bits `0..31`: `(ino << 32) | block_num`.
#[derive(Debug)]
pub struct FscryptInoLblk64FileCipher {
    slot: u8,
}

impl FscryptInoLblk64FileCipher {
    pub fn new(key: &UnwrappedKey) -> Self {
        Self { slot: key.slot().unwrap() }
    }

    pub fn new_with_slot(slot: u8) -> Self {
        Self { slot }
    }

    #[inline(always)]
    fn tweak(&self, ino: u64, block_num: u64) -> u64 {
        let ino = u32::try_from(ino).unwrap_or_else(|_| {
            panic!("IV_INO_LBLK_64 requires 32-bit inode numbers, got {ino:#x}")
        }) as u64;
        let block_num = u32::try_from(block_num).unwrap_or_else(|_| {
            panic!("IV_INO_LBLK_64 requires 32-bit logical block numbers, got {block_num:#x}")
        }) as u64;
        (ino << 32) | block_num
    }
}

impl Cipher for FscryptInoLblk64FileCipher {
    fn encrypt(
        &self,
        _ino: u64,
        _attribute_id: u64,
        _device_offset: u64,
        _file_offset: u64,
        _buffer: MutPtrByteSlice<'_>,
    ) -> Result<(), Error> {
        let e: Error = zx_status::Status::NOT_SUPPORTED.into();
        Err(e.context("encrypt not supported for InoLblk64File"))
    }

    fn decrypt(
        &self,
        _ino: u64,
        _attribute_id: u64,
        _device_offset: u64,
        _file_offset: u64,
        _buffer: MutPtrByteSlice<'_>,
    ) -> Result<(), Error> {
        let e: Error = zx_status::Status::NOT_SUPPORTED.into();
        Err(e.context("decrypt not supported for InoLblk64File"))
    }

    fn encrypt_filename(&self, _object_id: u64, _buffer: &mut Vec<u8>) -> Result<(), Error> {
        let e: Error = zx_status::Status::NOT_SUPPORTED.into();
        Err(e.context("encrypt_filename not supported for InoLblk64File"))
    }

    fn decrypt_filename(&self, _object_id: u64, _buffer: &mut Vec<u8>) -> Result<(), Error> {
        let e: Error = zx_status::Status::NOT_SUPPORTED.into();
        Err(e.context("decrypt_filename not supported for InoLblk64File"))
    }

    fn encrypt_symlink(&self, _object_id: u64, _buffer: &mut Vec<u8>) -> Result<(), Error> {
        let e: Error = zx_status::Status::NOT_SUPPORTED.into();
        Err(e.context("encrypt_symlink not supported for InoLblk64File"))
    }

    fn decrypt_symlink(&self, _object_id: u64, _buffer: &mut Vec<u8>) -> Result<(), Error> {
        let e: Error = zx_status::Status::NOT_SUPPORTED.into();
        Err(e.context("decrypt_symlink not supported for InoLblk64File"))
    }

    fn hash_code(&self, _raw_filename: &[u8], _filename: &str) -> Option<u32> {
        debug_assert!(false, "hash_code called on file cipher");
        None
    }

    fn hash_code_casefold(&self, _filename: &str) -> u32 {
        debug_assert!(false, "hash_code_casefold called on file cipher");
        0
    }

    fn supports_inline_encryption(&self) -> bool {
        true
    }

    fn crypt_ctx(&self, ino: u64, _attribute_id: u64, file_offset: u64) -> Option<(u64, u8)> {
        assert_eq!(file_offset % BLOCK_SIZE as u64, 0);
        let block_num = file_offset / BLOCK_SIZE as u64;
        let tweak = self.tweak(ino, block_num);
        Some((tweak, self.slot))
    }
}

#[cfg(test)]
mod tests {
    use super::{FscryptInoLblk64DirCipher, FscryptInoLblk64FileCipher};
    use crate::cipher::fscrypt_test_data;
    use crate::{Cipher, UnwrappedKey};

    #[test]
    fn test_fscrypt_ino_lblk64_file_cipher_dun() {
        let ino = 0x1234_5678u64;
        let file_offset = 10 * 4096;

        let cipher = FscryptInoLblk64FileCipher::new(&UnwrappedKey::new_with_slot(vec![], Some(4)));
        assert_eq!(cipher.crypt_ctx(ino, 0, file_offset), Some(((ino << 32) | 10, 4)));
    }

    #[test]
    fn test_fscrypt_ino_lblk64_dir_cipher_roundtrip() {
        let mut unwrapped = vec![0x11u8; 48];
        unwrapped[0] = 0x42;
        let cipher = FscryptInoLblk64DirCipher::new(&UnwrappedKey::new(unwrapped));

        let original = b"example_encrypted_filename.txt".to_vec();
        let mut buf1 = original.clone();
        let mut buf2 = original.clone();

        cipher.encrypt_filename(100, &mut buf1).expect("encrypt_filename failed");
        cipher.encrypt_filename(200, &mut buf2).expect("encrypt_filename failed");
        // Different directory object_ids must produce distinct ciphertexts under IV_INO_LBLK_64.
        assert_ne!(buf1, buf2);

        cipher.decrypt_filename(100, &mut buf1).expect("decrypt_filename failed");
        assert_eq!(buf1, original);
    }

    #[test]
    fn test_encrypt_and_decrypt_filename_golden() {
        // Golden test vectors generated with OpenSSL 3.6.3.
        // Under IV_INO_LBLK_64, the 16-byte IV has `lblk_num = 0` in bits 0..31 and the unhashed
        // 32-bit `object_id` in bits 32..63 (little-endian), so `object_id = 2` produces
        // IV `00000000020000000000000000000000`.
        let mut unwrapped_key = UnwrappedKey::new([0; 48].to_vec());
        unwrapped_key[0] = 0x10;
        let cipher = FscryptInoLblk64DirCipher::new(&unwrapped_key);
        let object_id = 2;

        // 1. One-block case (16 bytes after zero-padding):
        // ```shell
        // echo -n filename > in.txt ; truncate -s 16 in.txt
        // openssl aes-256-cbc -e -iv 00000000020000000000000000000000 -nosalt \
        //   -K 1000000000000000000000000000000000000000000000000000000000000000 \
        //   -in in.txt -out out.txt -nopad
        // hexdump out.txt -e "16/1 \"%02x\" \"\n\"" -v
        // ```
        let mut text = b"filename".to_vec();
        let expected_1block =
            hex::decode("23cf0ebf11b011f471c3e6be7b2d7d4a").expect("decode failed");
        cipher.encrypt_filename(object_id, &mut text).expect("encrypt filename failed");
        assert_eq!(text, expected_1block);
        cipher.decrypt_filename(object_id, &mut text).expect("decrypt filename failed");
        assert_eq!(text, b"filename".to_vec());

        // 2. Two-block case (32 bytes after zero-padding, last two 16B blocks swapped for CTS):
        // ```shell
        // echo -n "0123456789abcdef_filename" > in.txt ; truncate -s 32 in.txt
        // openssl aes-256-cbc -e -iv 00000000020000000000000000000000 -nosalt \
        //   -K 1000000000000000000000000000000000000000000000000000000000000000 \
        //   -in in.txt -out out.txt -nopad
        // hexdump out.txt -e "16/1 \"%02x\" \"\n\"" -v
        // 4869c980dcb984074c3b977100cc9334
        // 1029e8d0cf2440b023f24e080d207704
        // <Swap the last two blocks and concatenate>
        // ```
        let mut text = b"0123456789abcdef_filename".to_vec();
        let expected_2block =
            hex::decode("1029e8d0cf2440b023f24e080d2077044869c980dcb984074c3b977100cc9334")
                .expect("decode failed");
        cipher.encrypt_filename(object_id, &mut text).expect("encrypt filename failed");
        assert_eq!(text, expected_2block);
        cipher.decrypt_filename(object_id, &mut text).expect("decrypt filename failed");
        assert_eq!(text, b"0123456789abcdef_filename".to_vec());

        // 3. Multi-block (192-byte) filename case:
        // ```shell
        // export LONG_NAME_16=xxxxxxxxyyyyyyyy
        // export LONG_NAME_32=${LONG_NAME_16}${LONG_NAME_16}
        // export LONG_NAME_64=${LONG_NAME_32}${LONG_NAME_32}
        // export LONG_NAME_192=${LONG_NAME_64}${LONG_NAME_64}${LONG_NAME_64}
        // echo -n "${LONG_NAME_192}" > in.txt
        // openssl aes-256-cbc -e -iv 00000000020000000000000000000000 -nosalt \
        //   -K 1000000000000000000000000000000000000000000000000000000000000000 \
        //   -in in.txt -out out.txt -nopad
        // <Swap the last two 16-byte blocks and concatenate>
        // ```
        let long_name_64 = b"xxxxxxxxyyyyyyyyxxxxxxxxyyyyyyyyxxxxxxxxyyyyyyyyxxxxxxxxyyyyyyyy";
        let mut text = vec![];
        for _ in 0..3 {
            text.extend_from_slice(long_name_64);
        }
        let original_192 = text.clone();
        let expected_192 = hex::decode(concat!(
            "7b379f5e2cdb7f89bd3ee46fb7bac8302c0c9c1008a7080d3bd606aa16192d7c",
            "373dad8f8a9ae6229472be9cce1e3326e338afbec4b55d1ea789d42da2375ecf",
            "2711452440dc6264f0093c5554a44d85c45c2cb23dfa1638882c63684f858c7a",
            "4f0f77bc5ccf62b5eb2f63e7d7b713667fc546cbb83e81b97009f611af738e52",
            "1fdd34a3fc92954875dfa2837e904177e9f013fe9374ec0d963e7711473cc271",
            "9d2b35196eb4af074ae22db4334813fecceb130b0d3108d5c6f96bb73fdcd419"
        ))
        .expect("decode failed");
        cipher.encrypt_filename(object_id, &mut text).expect("encrypt filename failed");
        assert_eq!(text, expected_192);
        cipher.decrypt_filename(object_id, &mut text).expect("decrypt filename failed");
        assert_eq!(text, original_192);
    }

    #[test]
    fn test_hkdf_and_filename_encryption_golden() {
        // End-to-end HKDF + AES-256-CBC-CTS golden vector generated with OpenSSL 3.6.3
        // using `fscrypt_test_data::{KEY, UUID, DIR_NONCE, DIR_INODE}`:
        // ```shell
        // # HKDF-SHA512 with info = "fscrypt\0" (6673637279707400) ||
        // #   HKDF_CONTEXT_IV_INO_LBLK_64_KEY (04) || ENCRYPTION_MODE_AES_256_CTS (04) || UUID
        // KEY=9cdfd86c8d7023a0c11653501d81a21ee318eee2318ac17d2a44350758a2d47b\
        // f15ff524cb07478ef5dec4df7b63cef46bd466c0909e243f23454780a7616a93
        // INFO=667363727970740004048409849be0894765acc1d6d0cc2f2c97
        // openssl kdf -keylen 32 -kdfopt digest:SHA2-512 \
        //   -kdfopt "hexkey:${KEY}" -kdfopt "hexinfo:${INFO}" HKDF
        // ```
        let unwrapped = UnwrappedKey::new(
            fscrypt::to_directory_keys_lblk64(
                fscrypt_test_data::KEY,
                fscrypt_test_data::UUID,
                fscrypt_test_data::DIR_NONCE,
            )
            .to_unwrapped_key(),
        );
        assert_eq!(
            &unwrapped[..32],
            &hex::decode("9f5d59cdaac1960b57edcbd102ab97f58bf921975a0280a20bc41f649152d196")
                .unwrap()[..]
        );

        let cipher = FscryptInoLblk64DirCipher::new(&unwrapped);

        let mut short_name = b"A".to_vec();
        cipher.encrypt_filename(fscrypt_test_data::DIR_INODE, &mut short_name).unwrap();
        assert_eq!(short_name, hex::decode("882c820a8063b71194dafebb0e8c9858").unwrap());
        cipher.decrypt_filename(fscrypt_test_data::DIR_INODE, &mut short_name).unwrap();
        assert_eq!(short_name, b"A");

        let mut two_block_name = b"AAAAAAAAAAAAAAAAA".to_vec();
        cipher.encrypt_filename(fscrypt_test_data::DIR_INODE, &mut two_block_name).unwrap();
        assert_eq!(
            two_block_name,
            hex::decode("b3bac148afddc13adeb7fd8a324d2aeafbe599ec70a6be84e0769c00689ca7f9")
                .unwrap()
        );
        cipher.decrypt_filename(fscrypt_test_data::DIR_INODE, &mut two_block_name).unwrap();
        assert_eq!(two_block_name, b"AAAAAAAAAAAAAAAAA");
    }
}
