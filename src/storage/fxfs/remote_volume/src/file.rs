// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::RemoteObjectStore;
use anyhow::Error;
use fidl_fuchsia_io as fio;
use futures::executor::block_on;
use fxfs::errors::FxfsError;
use fxfs::object_store::{
    AttributeId, AttributeKey, HandleOptions, ObjectAttributes, ObjectKey, ObjectKind,
    ObjectStoreTrait, ObjectValue, StoreObjectHandle,
};
use std::sync::Arc;
use vfs::attributes;

/// The operations a file supports. The volume is read-only, so there are no write abilities.
const FILE_ABILITIES: fio::Operations =
    fio::Operations::GET_ATTRIBUTES.union(fio::Operations::READ_BYTES);

#[derive(Debug)]
pub struct RemoteFile {
    handle: StoreObjectHandle<RemoteObjectStore>,
}

impl RemoteFile {
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

    pub fn get_attributes(
        &self,
        query: fio::NodeAttributesQuery,
    ) -> Result<fio::NodeAttributes2, Error> {
        let object_id = self.handle.object_id();
        let value = block_on(self.handle.store().tree().find_value(&ObjectKey::object(object_id)))?
            .ok_or(FxfsError::NotFound)?;
        let ObjectValue::Object {
            kind: ObjectKind::File { refs, .. },
            attributes:
                ObjectAttributes {
                    creation_time,
                    modification_time,
                    posix_attributes,
                    allocated_size,
                    access_time,
                    change_time,
                    ..
                },
        } = value
        else {
            return Err(FxfsError::Inconsistent.into());
        };
        let verity_descriptor_query =
            fio::NodeAttributesQuery::OPTIONS | fio::NodeAttributesQuery::ROOT_HASH;
        let verity_enabled = if query
            .intersects(verity_descriptor_query | fio::NodeAttributesQuery::VERITY_ENABLED)
        {
            Some(self.is_verity_enabled()?)
        } else {
            None
        };
        if verity_enabled == Some(true) && query.intersects(verity_descriptor_query) {
            // TODO(https://fxbug.dev/570053552): FIDL fallback. The options and root hash are in
            // the fsverity descriptor, which is stored out of line in the Merkle attribute.
            return Err(FxfsError::NotSupported.into());
        }
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
                // `encryption_policy` is always absent: lookup fails in fscrypt directories, so
                // fscrypt files can't be opened.
            },
            Immutable {
                protocols: fio::NodeProtocolKinds::FILE,
                abilities: FILE_ABILITIES,
                // TODO(https://fxbug.dev/570053552): Read content_size from the paged VMO once
                // files are backed by one.
                content_size: 0,
                storage_size: allocated_size,
                // TODO(https://fxbug.dev/570053552): Report 0 for open unlinked or unnamed
                // temporary files (like `FxFile`'s `to_be_purged` check) once write/unlink support
                // lands.
                link_count: refs,
                id: object_id,
                change_time: change_time.as_nanos(),
                verity_enabled: verity_enabled,
            }
        ))
    }

    /// Returns whether fsverity is enabled, which the kind of the data attribute record shows.
    fn is_verity_enabled(&self) -> Result<bool, Error> {
        let key = ObjectKey::attribute(
            self.handle.object_id(),
            AttributeId::DATA,
            AttributeKey::Attribute,
        );
        match block_on(self.handle.store().tree().find_value(&key))?.ok_or(FxfsError::NotFound)? {
            ObjectValue::Attribute { .. } => Ok(false),
            ObjectValue::VerifiedAttribute { .. } => Ok(true),
            _ => Err(FxfsError::Inconsistent.into()),
        }
    }

    pub fn get_extended_attribute(&self, name: Vec<u8>) -> Result<Vec<u8>, Error> {
        RemoteObjectStore::get_extended_attribute(&self.handle, name)
    }

    pub fn list_extended_attributes(&self) -> Result<Vec<Vec<u8>>, Error> {
        block_on(self.handle.list_extended_attributes())
    }
}
