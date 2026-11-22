// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::RemoteObjectStore;
use anyhow::Error;
use fidl_fuchsia_io as fio;
use futures::executor::block_on;
use fxfs::errors::FxfsError;
use fxfs::object_store::{
    AttributeId, AttributeKey, FSCRYPT_KEY_ID, HandleOptions, ObjectAttributes, ObjectKey,
    ObjectKind, ObjectStoreTrait, ObjectValue, StoreObjectHandle, SymlinkTarget, resolve_symlink,
};
use std::sync::Arc;
use vfs::attributes;

/// The operations a symlink supports. The volume is read-only, so there are no write abilities.
// TODO(https://fxbug.dev/570053552): Add `UPDATE_ATTRIBUTES` once remote volumes support writes.
const SYMLINK_ABILITIES: fio::Operations = fio::Operations::GET_ATTRIBUTES;

#[derive(Debug)]
pub struct RemoteSymlink {
    handle: StoreObjectHandle<RemoteObjectStore>,
}

impl RemoteSymlink {
    pub fn new(store: Arc<RemoteObjectStore>, object_id: u64) -> Arc<Self> {
        Arc::new(Self {
            handle: StoreObjectHandle::new(
                store,
                object_id,
                /* permanent_keys: */ false,
                HandleOptions::default(),
                /* trace: */ false,
            ),
        })
    }

    /// Returns the target, like the local-only `StoreObjectHandle::read_symlink`.
    pub fn target(&self) -> Result<Vec<u8>, Error> {
        // TODO(https://fxbug.dev/570053552): Take a read lock on the symlink via `LockManager` to
        // prevent concurrent modification or removal.
        match block_on(resolve_symlink(self.handle.store(), self.handle.object_id()))? {
            SymlinkTarget::Inline(target) => Ok(target),
            SymlinkTarget::Encrypted(_) | SymlinkTarget::NonInline => {
                // TODO(https://fxbug.dev/570053552): FIDL fallback. Long targets are stored out of
                // line in the data attribute, and fscrypt targets need the crypt service to
                // decrypt.
                Err(FxfsError::NotSupported.into())
            }
        }
    }

    /// Reads the symlink's target and, if `query` is non-empty, its attributes from a single
    /// `ObjectKey::object` lookup.
    pub fn read_target_and_attributes(
        &self,
        query: fio::NodeAttributesQuery,
    ) -> Result<(Vec<u8>, Option<fio::NodeAttributes2>), Error> {
        if query.is_empty() {
            return Ok((self.target()?, None));
        }
        // TODO(https://fxbug.dev/570053552): Take a read lock on the symlink via `LockManager` to
        // prevent concurrent modification or removal.
        let (kind, object_attrs) = self.read_object_record()?;
        let (refs, target) = match kind {
            ObjectKind::Symlink { refs, link } => (refs, link.into_vec()),
            ObjectKind::EncryptedSymlink { .. } | ObjectKind::NonInlineSymlink { .. } => {
                // TODO(https://fxbug.dev/570053552): FIDL fallback. Long targets are stored out of
                // line in the data attribute, and fscrypt targets need the crypt service to
                // decrypt.
                return Err(FxfsError::NotSupported.into());
            }
            _ => return Err(FxfsError::Inconsistent.into()),
        };
        let target_len = target.len() as u64;
        let attributes = self.build_attributes(query, refs, object_attrs, || Ok(target_len))?;
        Ok((target, Some(attributes)))
    }

    pub fn get_attributes(
        &self,
        query: fio::NodeAttributesQuery,
    ) -> Result<fio::NodeAttributes2, Error> {
        let (kind, object_attrs) = self.read_object_record()?;
        let (ObjectKind::Symlink { refs, .. }
        | ObjectKind::EncryptedSymlink { refs, .. }
        | ObjectKind::NonInlineSymlink { refs }) = kind
        else {
            return Err(FxfsError::Inconsistent.into());
        };
        self.build_attributes(query, refs, object_attrs, || self.target_len(&kind))
    }

    fn read_object_record(&self) -> Result<(ObjectKind, ObjectAttributes), Error> {
        let value = block_on(
            self.handle.store().tree().find_value(&ObjectKey::object(self.handle.object_id())),
        )?
        .ok_or(FxfsError::NotFound)?;
        match value {
            ObjectValue::Object { kind, attributes } => Ok((kind, attributes)),
            _ => Err(FxfsError::Inconsistent.into()),
        }
    }

    fn build_attributes(
        &self,
        query: fio::NodeAttributesQuery,
        refs: u64,
        object_attrs: ObjectAttributes,
        content_size: impl FnOnce() -> Result<u64, Error>,
    ) -> Result<fio::NodeAttributes2, Error> {
        let ObjectAttributes {
            creation_time,
            modification_time,
            posix_attributes,
            allocated_size,
            access_time,
            change_time,
            ..
        } = object_attrs;
        Ok(attributes!(
            query,
            Mutable {
                creation_time: creation_time.as_nanos(),
                modification_time: modification_time.as_nanos(),
                access_time: access_time.as_nanos(),
                mode: posix_attributes.map(|a| a.mode),
                uid: posix_attributes.map(|a| a.uid),
                gid: posix_attributes.map(|a| a.gid),
                rdev: posix_attributes.map(|a| a.rdev),
                selinux_context: crate::get_inline_selinux_context(&self.handle)?,
            },
            Immutable {
                protocols: fio::NodeProtocolKinds::SYMLINK,
                abilities: SYMLINK_ABILITIES,
                // For POSIX compatibility, symlinks report the length of their target.
                content_size: content_size()?,
                storage_size: allocated_size,
                // TODO(https://fxbug.dev/570053552): Report 0 for open unlinked symlinks (like
                // `FxSymlink`'s `to_be_purged` check) once write/unlink support lands.
                link_count: refs,
                id: self.handle.object_id(),
                change_time: change_time.as_nanos(),
                verity_enabled: false,
            }
        ))
    }

    /// Returns the length of the target, given the kind from the symlink's object record. Mirrors
    /// `FxSymlink::get_properties`, but only for links whose length is in the tree.
    fn target_len(&self, kind: &ObjectKind) -> Result<u64, Error> {
        match kind {
            ObjectKind::Symlink { link, .. } => Ok(link.len() as u64),
            // The size of the data attribute is the length of the target, unless the target is
            // stored padded and encrypted.
            ObjectKind::NonInlineSymlink { .. } if !self.has_fscrypt_key()? => {
                let key = ObjectKey::attribute(
                    self.handle.object_id(),
                    AttributeId::DATA,
                    AttributeKey::Attribute,
                );
                match block_on(self.handle.store().tree().find_value(&key))? {
                    Some(ObjectValue::Attribute { size, .. }) => Ok(size),
                    _ => Err(FxfsError::Inconsistent.into()),
                }
            }
            ObjectKind::EncryptedSymlink { .. } | ObjectKind::NonInlineSymlink { .. } => {
                // TODO(https://fxbug.dev/570053552): FIDL fallback. The length of an fscrypt
                // target is the length of its decrypted name (or its proxy name while locked),
                // which needs the crypt service.
                Err(FxfsError::NotSupported.into())
            }
            _ => Err(FxfsError::Inconsistent.into()),
        }
    }

    /// Returns whether the symlink is encrypted with fscrypt, which its keys record shows.
    fn has_fscrypt_key(&self) -> Result<bool, Error> {
        let key = ObjectKey::keys(self.handle.object_id());
        match block_on(self.handle.store().tree().find_value(&key))? {
            None => Ok(false),
            Some(ObjectValue::Keys(keys)) => Ok(keys.get(FSCRYPT_KEY_ID).is_some()),
            Some(_) => Err(FxfsError::Inconsistent.into()),
        }
    }

    pub fn get_extended_attribute(&self, name: Vec<u8>) -> Result<Vec<u8>, Error> {
        RemoteObjectStore::get_extended_attribute(&self.handle, name)
    }

    pub fn list_extended_attributes(&self) -> Result<Vec<Vec<u8>>, Error> {
        block_on(self.handle.list_extended_attributes())
    }
}
