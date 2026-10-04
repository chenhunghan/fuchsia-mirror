// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::{
    BLOCK_SIZE, Cipher, MAX_FILENAME_LEN, MAX_SYMLINK_LEN, Tweak, UnwrappedKey,
    XtsInPlaceProcessor, decrypt_filename_cts, encrypt_filename_cts,
};
use aes::Aes256;
use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use anyhow::{Context, Error};
use siphasher::sip::SipHasher;
use std::hash::Hasher;
use storage_ptr_slice::MutPtrByteSlice;
use zerocopy::IntoBytes;

#[derive(Debug)]
pub(crate) struct FscryptInoLblk32DirCipher {
    cts_key: [u8; 32],
    ino_hash_key: [u8; 16],
    dir_hash_key: [u8; 16],
}
impl FscryptInoLblk32DirCipher {
    pub fn new(key: &UnwrappedKey) -> Self {
        Self {
            cts_key: key[..32].try_into().unwrap(),
            ino_hash_key: key[32..48].try_into().unwrap(),
            dir_hash_key: key[48..64].try_into().unwrap(),
        }
    }
}
impl Cipher for FscryptInoLblk32DirCipher {
    fn encrypt(
        &self,
        _ino: u64,
        _attribute_id: u64,
        _device_offset: u64,
        _file_offset: u64,
        _buffer: MutPtrByteSlice<'_>,
    ) -> Result<(), Error> {
        Err(zx_status::Status::NOT_SUPPORTED).context("encrypt not supported for InoLblk32Dir")
    }

    fn decrypt(
        &self,
        _ino: u64,
        _attribute_id: u64,
        _device_offset: u64,
        _file_offset: u64,
        _buffer: MutPtrByteSlice<'_>,
    ) -> Result<(), Error> {
        Err(zx_status::Status::NOT_SUPPORTED).context("decrypt not supported for InoLblk32Dir")
    }

    fn encrypt_filename(&self, object_id: u64, buffer: &mut Vec<u8>) -> Result<(), Error> {
        self.encrypt_filename_with_max_len(object_id, buffer, MAX_FILENAME_LEN)
    }

    fn decrypt_filename(&self, object_id: u64, buffer: &mut Vec<u8>) -> Result<(), Error> {
        self.decrypt_filename_with_max_len(object_id, buffer, MAX_FILENAME_LEN)
    }

    fn encrypt_symlink(&self, object_id: u64, buffer: &mut Vec<u8>) -> Result<(), Error> {
        self.encrypt_filename_with_max_len(object_id, buffer, MAX_SYMLINK_LEN)
    }

    fn decrypt_symlink(&self, object_id: u64, buffer: &mut Vec<u8>) -> Result<(), Error> {
        self.decrypt_filename_with_max_len(object_id, buffer, MAX_SYMLINK_LEN)
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

impl FscryptInoLblk32DirCipher {
    fn iv(&self, object_id: u64) -> [u32; 4] {
        let mut hasher = SipHasher::new_with_key(&self.ino_hash_key);
        hasher.write(object_id.as_bytes());
        [hasher.finish() as u32, 0, 0, 0]
    }

    fn encrypt_filename_with_max_len(
        &self,
        object_id: u64,
        buffer: &mut Vec<u8>,
        max_len: usize,
    ) -> Result<(), Error> {
        let iv = self.iv(object_id);
        encrypt_filename_cts(&self.cts_key, &iv, buffer, max_len)
    }

    fn decrypt_filename_with_max_len(
        &self,
        object_id: u64,
        buffer: &mut Vec<u8>,
        max_len: usize,
    ) -> Result<(), Error> {
        let iv = self.iv(object_id);
        decrypt_filename_cts(&self.cts_key, &iv, buffer, max_len)
    }
}

#[derive(Debug)]
pub struct FscryptInoLblk32FileCipher {
    slot: u8,
    ino_hash_key: [u8; 16],
}

impl FscryptInoLblk32FileCipher {
    pub fn new(key: &UnwrappedKey) -> Self {
        Self { slot: key.slot().unwrap(), ino_hash_key: key[..16].try_into().unwrap() }
    }

    #[inline(always)]
    fn tweak(&self, ino: u64, block_num: u64) -> u32 {
        let mut hasher = SipHasher::new_with_key(&self.ino_hash_key);
        hasher.write(ino.as_bytes());
        (hasher.finish().wrapping_add(block_num)) as u32
    }
}

// TODO(https://fxbug.dev/436902004): Remove encrypt/decrypt support once this cipher supports
// inline encryption.
impl Cipher for FscryptInoLblk32FileCipher {
    fn encrypt(
        &self,
        _ino: u64,
        _attribute_id: u64,
        _device_offset: u64,
        _file_offset: u64,
        _buffer: MutPtrByteSlice<'_>,
    ) -> Result<(), Error> {
        let e: Error = zx_status::Status::NOT_SUPPORTED.into();
        Err(e.context("encrypt not supported for InoLblk32File"))
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
        Err(e.context("decrypt not supported for InoLblk32File"))
    }

    fn encrypt_filename(&self, _object_id: u64, _buffer: &mut Vec<u8>) -> Result<(), Error> {
        let e: Error = zx_status::Status::NOT_SUPPORTED.into();
        Err(e.context("encrypt_filename not supported for InoLblk32File"))
    }

    fn decrypt_filename(&self, _object_id: u64, _buffer: &mut Vec<u8>) -> Result<(), Error> {
        let e: Error = zx_status::Status::NOT_SUPPORTED.into();
        Err(e.context("decrypt_filename not supported for InoLblk32File"))
    }

    fn encrypt_symlink(&self, _object_id: u64, _buffer: &mut Vec<u8>) -> Result<(), Error> {
        let e: Error = zx_status::Status::NOT_SUPPORTED.into();
        Err(e.context("encrypt_symlink not supported for InoLblk32File"))
    }

    fn decrypt_symlink(&self, _object_id: u64, _buffer: &mut Vec<u8>) -> Result<(), Error> {
        let e: Error = zx_status::Status::NOT_SUPPORTED.into();
        Err(e.context("decrypt_symlink not supported for InoLblk32File"))
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
        Some((tweak as u64, self.slot))
    }
}

// Software-fallback for the lblk32 file cipher.
#[derive(Debug)]
pub struct FscryptSoftwareInoLblk32FileCipher {
    xts_key1: Aes256,
    xts_key2: Aes256,
}

impl FscryptSoftwareInoLblk32FileCipher {
    pub fn new(key: &UnwrappedKey) -> Self {
        Self {
            xts_key1: Aes256::new((&key[..32]).try_into().unwrap()),
            xts_key2: Aes256::new((&key[32..64]).try_into().unwrap()),
        }
    }

    pub fn encrypt(&self, buffer: &mut [u8], mut tweak: u128) -> Result<(), Error> {
        fxfs_trace::duration!("encrypt", "len" => buffer.len());
        assert_eq!(buffer.len() % BLOCK_SIZE, 0);

        for block in buffer.chunks_exact_mut(BLOCK_SIZE) {
            let mut block_tweak = tweak;
            self.xts_key2.encrypt_block(block_tweak.as_mut_bytes().try_into().unwrap());
            self.xts_key1.encrypt_with_backend(XtsInPlaceProcessor::new(
                Tweak(block_tweak),
                MutPtrByteSlice::from(&mut block[..]),
            ));
            tweak += 1;
        }
        Ok(())
    }

    pub fn decrypt(&self, buffer: &mut [u8], mut tweak: u128) -> Result<(), Error> {
        fxfs_trace::duration!("decrypt", "len" => buffer.len());
        assert_eq!(buffer.len() % BLOCK_SIZE, 0);
        for block in buffer.chunks_exact_mut(BLOCK_SIZE) {
            let mut block_tweak = tweak;
            self.xts_key2.encrypt_block(block_tweak.as_mut_bytes().try_into().unwrap());
            self.xts_key1.decrypt_with_backend(XtsInPlaceProcessor::new(
                Tweak(block_tweak),
                MutPtrByteSlice::from(&mut block[..]),
            ));
            tweak += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BLOCK_SIZE, FscryptInoLblk32DirCipher, FscryptInoLblk32FileCipher,
        FscryptSoftwareInoLblk32FileCipher, UnwrappedKey,
    };
    use crate::Cipher;
    use crate::cipher::fscrypt_test_data;
    use fscrypt::proxy_filename::ProxyFilename;
    use std::sync::Arc;

    #[test]
    fn test_encrypt_filename() {
        let mut unwrapped_key = UnwrappedKey::new([0; 64].to_vec());
        unwrapped_key[0] = 0x10;
        let cipher: Arc<dyn Cipher> = Arc::new(FscryptInoLblk32DirCipher::new(&unwrapped_key));
        let object_id = 2;

        // One block case.
        // ```shell
        // echo -n filename > in.txt ; truncate -s 16 in.txt
        // openssl aes-256-cbc -e -iv 014ae2cc000000000000000000000000 -nosalt -K 1000000000000000000000000000000000000000000000000000000000000000  -in in.txt -out out.txt -nopad
        // hexdump out.txt -e "16/1 \"%02x\" \"\n\"" -v
        // ```
        let mut text = b"filename".to_vec();
        cipher.encrypt_filename(object_id, &mut text).expect("encrypt filename failed");
        assert_eq!(text, hex::decode("2b7c885165f090393fcbb15f5018f18a").expect("decode failed"));

        // Two block case.
        // ```shell
        // echo -n "0123456789abcdef_filename" > in.txt ; truncate -s 16 in.txt
        // openssl aes-256-cbc -e -iv 014ae2cc000000000000000000000000 -nosalt -K 1000000000000000000000000000000000000000000000000000000000000000  -in in.txt -out out.txt -nopad
        // hexdump out.txt -e "16/1 \"%02x\" \"\n\"" -v
        // 3da06c8fc2e54065f391531affeae1fb
        // d6bad68cc11eb87719735fc50b7efbb3
        // <Swap the last two blocks and concatenate>
        // ``````
        let mut text = b"0123456789abcdef_filename".to_vec();
        cipher.encrypt_filename(object_id, &mut text).expect("encrypt filename failed");
        assert_eq!(
            text,
            hex::decode("d6bad68cc11eb87719735fc50b7efbb33da06c8fc2e54065f391531affeae1fb")
                .expect("decode failed")
        );

        // Test a 192 byte filename -- same as in test image (known to decrypt successfully).
        // ```shell
        // export LONG_NAME_16=xxxxxxxxyyyyyyyy
        // export LONG_NAME_32=${LONG_NAME_16}${LONG_NAME_16}
        // export LONG_NAME_64=${LONG_NAME_32}${LONG_NAME_32}
        // export LONG_NAME_128=${LONG_NAME_64}${LONG_NAME_64}
        // export LONG_NAME_192=${LONG_NAME_128}${LONG_NAME_64}
        // echo -n "${LONG_NAME_192}" > in.txt
        // openssl aes-256-cbc -e -iv 014ae2cc000000000000000000000000 -nosalt -K 1000000000000000000000000000000000000000000000000000000000000000  -in in.txt -out out.txt -nopad
        // hexdump out.txt -e "16/1 \"%02x\" \"\n\"" -v
        // f59d083c16915d5d3479b9dbf7b7f053
        // 1905bde71624f4ba1ab416b15831ca87
        // c2d99e43f97bd2fc18f2ad03da252715
        // abf9d0cd9bde4215bfeeec7d07dbcf89
        // 0bcc4a230faaaf73cabdfc3ca8b20a06
        // 84847f7f3991d55b6b30859dfc662c1a
        // ef03c7d16830ef7df367a3392a82e588
        // 629b89feffe49036e420686598545b20
        // 119c346af4f80fdbd225a625aa0f45ce
        // 393cfff0bd9971b6782d8768dbd13587
        // 38e3a65f8ef14612881e6cbd38cf4bcf
        // 08a75c38d9fb681304fdaa1e85a091ce
        // <Swap the last two blocks and concatenate>
        // ``````
        let long_name_64 = b"xxxxxxxxyyyyyyyyxxxxxxxxyyyyyyyyxxxxxxxxyyyyyyyyxxxxxxxxyyyyyyyy";
        let mut text = vec![];
        for _ in 0..3 {
            text.extend_from_slice(long_name_64);
        }

        let raw = hex::decode(concat!(
            "f59d083c16915d5d3479b9dbf7b7f0531905bde71624f4ba1ab416b15831ca87",
            "c2d99e43f97bd2fc18f2ad03da252715abf9d0cd9bde4215bfeeec7d07dbcf89",
            "0bcc4a230faaaf73cabdfc3ca8b20a0684847f7f3991d55b6b30859dfc662c1a",
            "ef03c7d16830ef7df367a3392a82e588629b89feffe49036e420686598545b20",
            "119c346af4f80fdbd225a625aa0f45ce393cfff0bd9971b6782d8768dbd13587",
            "08a75c38d9fb681304fdaa1e85a091ce38e3a65f8ef14612881e6cbd38cf4bcf"
        ))
        .expect("decode failed");
        cipher.encrypt_filename(object_id, &mut text).expect("encrypt filename failed");
        assert_eq!(text, raw);
    }

    #[test]
    fn test_decrypt_filename() {
        // Should be equivalent to:
        // ```shell
        // openssl aes-256-cbc -d -iv 014ae2cc000000000000000000000000 -nosalt -K 1000000000000000000000000000000000000000000000000000000000000000  -in in.txt -out out.txt -nopad
        // cat in.txt
        // ```
        let mut unwrapped_key = UnwrappedKey::new([0; 64].to_vec());
        unwrapped_key[0] = 0x10;
        let cipher: Arc<dyn Cipher> = Arc::new(FscryptInoLblk32DirCipher::new(&unwrapped_key));
        let object_id = 2;

        // One block case.
        let mut text = hex::decode("2b7c885165f090393fcbb15f5018f18a").expect("decode failed");
        cipher.decrypt_filename(object_id, &mut text).expect("encrypt filename failed");
        assert_eq!(text, b"filename".to_vec());

        // Two block case.
        let mut text =
            hex::decode("d6bad68cc11eb87719735fc50b7efbb33da06c8fc2e54065f391531affeae1fb")
                .expect("decode failed");
        cipher.decrypt_filename(object_id, &mut text).expect("encrypt filename failed");
        assert_eq!(text, b"0123456789abcdef_filename".to_vec());
    }

    #[test]
    fn test_generated_filenames() {
        let cipher: Arc<dyn Cipher> = Arc::new(FscryptInoLblk32DirCipher::new(&UnwrappedKey::new(
            fscrypt::to_directory_keys(
                fscrypt_test_data::KEY,
                fscrypt_test_data::UUID,
                fscrypt_test_data::DIR_NONCE,
            )
            .to_unwrapped_key(),
        )));

        for file in fscrypt_test_data::FILES {
            let mut buffer = file.unencrypted_name.as_bytes().to_vec();
            cipher.encrypt_filename(fscrypt_test_data::DIR_INODE, &mut buffer).unwrap();
            let proxy_name = ProxyFilename::new(&buffer);
            let proxy_name_str: String = proxy_name.into();
            assert_eq!(
                proxy_name_str,
                file.proxy_name,
                "Proxy name mismatch for (len {}) {}",
                file.unencrypted_name.len(),
                file.unencrypted_name
            );
            cipher.decrypt_filename(fscrypt_test_data::DIR_INODE, &mut buffer).unwrap();
            assert_eq!(String::from_utf8(buffer).unwrap(), file.unencrypted_name);
        }
    }

    #[test]
    fn test_generated_casefold_filenames() {
        let unwrapped = UnwrappedKey::new(
            fscrypt::to_directory_keys(
                fscrypt_test_data::KEY,
                fscrypt_test_data::UUID,
                fscrypt_test_data::CASEFOLD_DIR_NONCE,
            )
            .to_unwrapped_key(),
        );
        let cipher_struct = FscryptInoLblk32DirCipher::new(&unwrapped);
        let cipher: Arc<dyn Cipher> = Arc::new(cipher_struct);

        for file in fscrypt_test_data::CASEFOLD_FILES {
            let mut buffer = file.unencrypted_name.as_bytes().to_vec();
            cipher.encrypt_filename(fscrypt_test_data::CASEFOLD_DIR_INODE, &mut buffer).unwrap();

            let expected_proxy: ProxyFilename = file.proxy_name.try_into().unwrap();
            let mut hash_code = cipher.hash_code_casefold(file.unencrypted_name);
            if file.unencrypted_name.len() == 255 {
                // There's an f2fs bug for filenames that are 255 bytes long.  The bug means that
                // the name isn't case folded before the hash is computed.  For now, we just copy
                // f2fs's hash code computation.
                hash_code = expected_proxy.hash_code as u32;
            }
            let actual_proxy = ProxyFilename::new_with_hash_code(hash_code as u64, &buffer);

            assert_eq!(
                actual_proxy,
                expected_proxy,
                "Proxy name mismatch for (len {}) {}",
                file.unencrypted_name.len(),
                file.unencrypted_name
            );
            cipher.decrypt_filename(fscrypt_test_data::CASEFOLD_DIR_INODE, &mut buffer).unwrap();
            assert_eq!(String::from_utf8(buffer).unwrap(), file.unencrypted_name);
        }
    }

    #[test]
    fn test_generated_casefold_symlinks() {
        let unwrapped = UnwrappedKey::new(
            fscrypt::to_directory_keys(
                fscrypt_test_data::KEY,
                fscrypt_test_data::UUID,
                fscrypt_test_data::CASEFOLD_DIR_NONCE,
            )
            .to_unwrapped_key(),
        );
        let cipher_struct = FscryptInoLblk32DirCipher::new(&unwrapped);
        let cipher: Arc<dyn Cipher> = Arc::new(cipher_struct);

        for file in fscrypt_test_data::SYMLINKS {
            // Verify symlink target encryption/decryption
            // Symlink targets are encrypted using the same mechanism as filenames,
            // using the symlink's own inode as the IV.
            let mut target_buffer = file.target.as_bytes().to_vec();
            cipher.encrypt_symlink(file.inode, &mut target_buffer).unwrap();

            let expected_proxy: ProxyFilename =
                file.encrypted_target_proxy_name.try_into().unwrap();
            // Symlinks don't have a hash code, so we use 0.
            let actual_proxy = ProxyFilename::new_with_hash_code(0, &target_buffer);

            assert_eq!(
                actual_proxy,
                expected_proxy,
                "Proxy name mismatch for symlink length {}",
                file.target.len()
            );

            cipher.decrypt_symlink(file.inode, &mut target_buffer).unwrap();
            assert_eq!(
                String::from_utf8(target_buffer).unwrap(),
                file.target,
                "Decrypted target mismatch for symlink {}",
                file.target
            );
        }
    }

    #[test]
    fn test_software_file_cipher_multi_block() {
        let key = UnwrappedKey::new((0..64).collect());
        let cipher = FscryptSoftwareInoLblk32FileCipher::new(&key);
        let base_tweak: u128 = 0x1234_5678;

        // Create a 3-block buffer with distinct data per block.
        let mut multi_block_buf = Vec::with_capacity(3 * BLOCK_SIZE);
        for i in 0..3u8 {
            multi_block_buf.extend(std::iter::repeat_n(i + 1, BLOCK_SIZE));
        }
        let original_plaintext = multi_block_buf.clone();

        // Encrypt all 3 blocks in a single call.
        cipher.encrypt(&mut multi_block_buf, base_tweak).expect("multi-block encrypt failed");

        // Encrypting each block individually with its respective tweak (base_tweak + i) must
        // produce identical ciphertext for every block. Note that testing encrypt followed by
        // decrypt on the same multi-block buffer would NOT catch a bug where both encrypt and
        // decrypt compute the wrong tweak sequence across loop iterations.
        for i in 0..3 {
            let mut single_block =
                original_plaintext[i * BLOCK_SIZE..(i + 1) * BLOCK_SIZE].to_vec();
            cipher
                .encrypt(&mut single_block, base_tweak + i as u128)
                .expect("single-block encrypt failed");
            assert_eq!(
                &multi_block_buf[i * BLOCK_SIZE..(i + 1) * BLOCK_SIZE],
                &single_block[..],
                "Ciphertext mismatch at block {i}"
            );
        }

        // Verify that decrypting each block individually from the multi-block ciphertext restores
        // the original plaintext.
        for i in 0..3 {
            let mut single_block = multi_block_buf[i * BLOCK_SIZE..(i + 1) * BLOCK_SIZE].to_vec();
            cipher
                .decrypt(&mut single_block, base_tweak + i as u128)
                .expect("single-block decrypt failed");
            assert_eq!(
                &single_block[..],
                &original_plaintext[i * BLOCK_SIZE..(i + 1) * BLOCK_SIZE],
                "Single-block decrypt mismatch at block {i}"
            );
        }

        // Verify multi-block decrypt restores all blocks at once.
        cipher.decrypt(&mut multi_block_buf, base_tweak).expect("multi-block decrypt failed");
        assert_eq!(multi_block_buf, original_plaintext);
    }

    #[test]
    fn test_file_cipher_uses_registered_slot() {
        let ino_hash_key = [0xab; 16];
        let unwrapped_with_slot = UnwrappedKey::new_with_slot(ino_hash_key.to_vec(), Some(42));
        let cipher = FscryptInoLblk32FileCipher::new(&unwrapped_with_slot);

        let (dun, slot) = cipher.crypt_ctx(7, 0, 3 * BLOCK_SIZE as u64).unwrap();
        assert_eq!(slot, 42);
        assert_eq!(dun, cipher.tweak(7, 3).into());
    }

    #[test]
    fn test_fscrypt_ino_lblk32_file_cipher_dun() {
        let ino = 0x1234_5678u64;
        let file_offset = 10 * 4096;

        let cipher_lblk32 = FscryptInoLblk32FileCipher::new(&UnwrappedKey::new_with_slot(
            vec![0x42u8; 16],
            Some(5),
        ));
        let (dun32, slot32) = cipher_lblk32.crypt_ctx(ino, 0, file_offset).unwrap();
        assert_eq!(slot32, 5);
        assert!(dun32 <= u32::MAX as u64);
    }
}
