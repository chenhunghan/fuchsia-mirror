// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::errors::map_to_status;
use async_trait::async_trait;
use fidl::endpoints::ClientEnd;
use fidl_fuchsia_fxfs::{CryptMarker, CryptProxy, KeyPurpose as FidlKeyPurpose};
use fxfs_crypto::{
    Crypt, EncryptionKey, FxfsKey, KeyPurpose, ObjectType, UnwrappedKey, WrappedKey,
    WrappedKeyBytes, WrappingKeyId,
};

use fuchsia_sync::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use storage_device::Device;

pub struct RemoteCrypt {
    client: CryptProxy,
    device: Option<Arc<dyn Device>>,
    registered_keys: Mutex<HashMap<zx::Koid, u8>>,
}

impl RemoteCrypt {
    pub fn new(client: ClientEnd<CryptMarker>) -> Self {
        Self { client: client.into_proxy(), device: None, registered_keys: Mutex::default() }
    }

    /// Creates a `RemoteCrypt` that registers any returned inline encryption `key_token` handles
    /// with `device`.
    pub fn new_with_device(client: ClientEnd<CryptMarker>, device: Arc<dyn Device>) -> Self {
        Self {
            client: client.into_proxy(),
            device: Some(device),
            registered_keys: Mutex::default(),
        }
    }

    async fn register_key(&self, key_token: zx::EventPair) -> Result<u8, zx::Status> {
        let koid = key_token.koid()?;
        if let Some(&slot) = self.registered_keys.lock().get(&koid) {
            return Ok(slot);
        }
        let slot =
            self.device.as_ref().ok_or(zx::Status::NOT_SUPPORTED)?.register_key(key_token).await?;
        self.registered_keys.lock().insert(koid, slot);
        Ok(slot)
    }
}

trait IntoFidlKeyPurpose {
    fn into_fidl(self) -> FidlKeyPurpose;
}

impl IntoFidlKeyPurpose for KeyPurpose {
    fn into_fidl(self) -> FidlKeyPurpose {
        match self {
            KeyPurpose::Data => FidlKeyPurpose::Data,
            KeyPurpose::Metadata => FidlKeyPurpose::Metadata,
        }
    }
}

#[async_trait]
impl Crypt for RemoteCrypt {
    async fn create_key(
        &self,
        owner: u64,
        purpose: KeyPurpose,
    ) -> Result<(FxfsKey, UnwrappedKey), zx::Status> {
        let (wrapping_key_id, key, unwrapped_key) = self
            .client
            .create_key(owner, purpose.into_fidl())
            .await
            .map_err(|e| map_to_status(e.into()))?
            .map_err(zx::Status::err_from_raw)?;
        Ok((
            FxfsKey {
                wrapping_key_id,
                key: WrappedKeyBytes::try_from(key).map_err(map_to_status)?,
            },
            UnwrappedKey::new(unwrapped_key),
        ))
    }

    async fn create_key_with_id(
        &self,
        owner: u64,
        wrapping_key_id: WrappingKeyId,
        object_type: ObjectType,
        flags: fidl_fuchsia_io::FscryptPolicyFlags,
    ) -> Result<(EncryptionKey, UnwrappedKey), zx::Status> {
        let (key, unwrapped_key, key_token) = self
            .client
            .create_key_with_id(owner, &wrapping_key_id, object_type, flags)
            .await
            .map_err(|e| map_to_status(e.into()))?
            .map_err(zx::Status::err_from_raw)?;
        let slot = if let Some(key_token) = key_token {
            Some(self.register_key(key_token).await?)
        } else {
            None
        };
        Ok((key.try_into()?, UnwrappedKey::new_with_slot(unwrapped_key, slot)))
    }

    async fn unwrap_key(
        &self,
        wrapped_key: &WrappedKey,
        owner: u64,
    ) -> Result<UnwrappedKey, zx::Status> {
        let (unwrapped, key_token) = self
            .client
            .unwrap_key(owner, &wrapped_key)
            .await
            .map_err(|e| map_to_status(e.into()))?
            .map_err(zx::Status::err_from_raw)?;
        let slot = if let Some(key_token) = key_token {
            Some(self.register_key(key_token).await?)
        } else {
            None
        };
        Ok(UnwrappedKey::new_with_slot(unwrapped, slot))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Error;
    use fidl::endpoints::create_endpoints;
    use fidl_fuchsia_fxfs::{CryptRequest, FscryptKeyIdentifier};
    use fuchsia_async as fasync;
    use futures::TryStreamExt as _;
    use std::ops::Range;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use storage_device::buffer::{BufferFuture, BufferRef, MutableBufferRef};
    use storage_device::fake_device::FakeDevice;
    use storage_device::{ReadOptions, WriteOptions};

    struct MockDevice {
        inner: FakeDevice,
        expected_server_koid: zx::Koid,
        slot: u8,
        register_key_calls: AtomicUsize,
    }

    #[async_trait]
    impl Device for MockDevice {
        fn allocate_buffer(&self, size: usize) -> BufferFuture<'_> {
            self.inner.allocate_buffer(size)
        }
        fn block_size(&self) -> u32 {
            self.inner.block_size()
        }
        fn block_count(&self) -> u64 {
            self.inner.block_count()
        }
        async fn read_with_opts(
            &self,
            offset: u64,
            buffer: MutableBufferRef<'_>,
            read_opts: ReadOptions,
        ) -> Result<(), Error> {
            self.inner.read_with_opts(offset, buffer, read_opts).await
        }
        async fn write_with_opts(
            &self,
            offset: u64,
            buffer: BufferRef<'_>,
            write_opts: WriteOptions,
        ) -> Result<(), Error> {
            self.inner.write_with_opts(offset, buffer, write_opts).await
        }
        async fn trim(&self, range: Range<u64>) -> Result<(), Error> {
            self.inner.trim(range).await
        }
        async fn close(&self) -> Result<(), Error> {
            self.inner.close().await
        }
        async fn flush(&self) -> Result<(), Error> {
            self.inner.flush().await
        }
        fn is_read_only(&self) -> bool {
            self.inner.is_read_only()
        }
        fn supports_trim(&self) -> bool {
            self.inner.supports_trim()
        }
        async fn register_key(&self, key_token: zx::EventPair) -> Result<u8, zx::Status> {
            self.register_key_calls.fetch_add(1, Ordering::Relaxed);
            let info = key_token.basic_info()?;
            if info.related_koid == self.expected_server_koid {
                Ok(self.slot)
            } else {
                Err(zx::Status::ACCESS_DENIED)
            }
        }
    }

    #[fuchsia::test]
    async fn test_remote_crypt_registers_key_token() {
        let (server_ep, client_ep) = zx::EventPair::create();
        let server_koid = server_ep.koid().unwrap();

        let (crypt_client, crypt_server) = create_endpoints::<CryptMarker>();
        let scope = fasync::Scope::new();
        scope.spawn(async move {
            let mut stream = crypt_server.into_stream();
            while let Some(request) = stream.try_next().await.unwrap() {
                match request {
                    CryptRequest::CreateKeyWithId { wrapping_key_id, responder, .. } => {
                        let wrapped = WrappedKey::FscryptInoLblk32File(FscryptKeyIdentifier {
                            key_identifier: wrapping_key_id,
                        });
                        let token = client_ep.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
                        responder.send(Ok((&wrapped, &[0x11; 16], Some(token)))).unwrap();
                    }
                    CryptRequest::UnwrapKey { responder, .. } => {
                        let token = client_ep.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
                        responder.send(Ok((&[0x22; 16], Some(token)))).unwrap();
                    }
                    _ => unreachable!(),
                }
            }
        });

        let mock_device = Arc::new(MockDevice {
            inner: FakeDevice::new(64, 4096),
            expected_server_koid: server_koid,
            slot: 9,
            register_key_calls: AtomicUsize::new(0),
        });
        let device: Arc<dyn Device> = mock_device.clone();
        let remote_crypt = RemoteCrypt::new_with_device(crypt_client, device);

        let (_wrapped, unwrapped) = remote_crypt
            .create_key_with_id(
                1,
                [0xaa; 16],
                ObjectType::File,
                fidl_fuchsia_io::FscryptPolicyFlags::empty(),
            )
            .await
            .unwrap();
        assert_eq!(unwrapped.slot(), Some(9));
        assert_eq!(&*unwrapped, &[0x11; 16]);
        assert_eq!(mock_device.register_key_calls.load(Ordering::Relaxed), 1);

        let wrapped =
            WrappedKey::FscryptInoLblk32File(FscryptKeyIdentifier { key_identifier: [0xaa; 16] });
        let unwrapped = remote_crypt.unwrap_key(&wrapped, 1).await.unwrap();
        assert_eq!(unwrapped.slot(), Some(9));
        assert_eq!(&*unwrapped, &[0x22; 16]);
        assert_eq!(mock_device.register_key_calls.load(Ordering::Relaxed), 1);
    }
}
