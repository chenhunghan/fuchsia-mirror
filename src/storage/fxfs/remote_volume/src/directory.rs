// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::file::RemoteFile;
use crate::symlink::RemoteSymlink;
use crate::{NodeKind, RemoteObjectStore};
use anyhow::{Error, ensure};
use fidl_fuchsia_io as fio;
use futures::executor::block_on;
use fxfs::errors::FxfsError;
use fxfs::object_store::{Directory, ObjectDescriptor};
use std::sync::Arc;
use vfs::attributes;
use vfs::path::Path;

/// The operations a directory supports. The volume is read-only, so there are no write abilities.
const DIRECTORY_ABILITIES: fio::Operations = fio::Operations::GET_ATTRIBUTES
    .union(fio::Operations::ENUMERATE)
    .union(fio::Operations::TRAVERSE);

#[derive(Debug)]
pub struct RemoteDirectory {
    directory: Directory<RemoteObjectStore>,
}

impl RemoteDirectory {
    pub fn open(store: &Arc<RemoteObjectStore>, object_id: u64) -> Result<Arc<Self>, Error> {
        Ok(Arc::new(Self { directory: block_on(Directory::open(store, object_id))? }))
    }

    fn lookup(&self, name: &str) -> Result<NodeKind, Error> {
        // TODO(https://fxbug.dev/570053552): Take a read lock on the directory once remote
        // volumes support writes.
        let (object_id, descriptor, _locked) =
            block_on(self.directory.lookup(name))?.ok_or(FxfsError::NotFound)?;
        let store = self.directory.owner();
        match descriptor {
            ObjectDescriptor::Directory => Ok(NodeKind::Directory(Self::open(store, object_id)?)),
            ObjectDescriptor::File => Ok(NodeKind::File(RemoteFile::new(store.clone(), object_id))),
            ObjectDescriptor::Symlink => {
                Ok(NodeKind::Symlink(RemoteSymlink::new(store.clone(), object_id)))
            }
            ObjectDescriptor::Volume => Err(FxfsError::Inconsistent.into()),
        }
    }

    pub fn walk(self: &Arc<Self>, mut path: Path) -> Result<NodeKind, Error> {
        let mut node = NodeKind::Directory(self.clone());
        while let Some(name) = path.next() {
            let NodeKind::Directory(directory) = &node else {
                return Err(FxfsError::NotDir.into());
            };
            node = directory.lookup(name)?;
        }
        Ok(node)
    }

    pub fn get_attributes(
        &self,
        query: fio::NodeAttributesQuery,
    ) -> Result<fio::NodeAttributes2, Error> {
        let props = block_on(self.directory.get_properties())?;
        Ok(attributes!(
            query,
            Mutable {
                creation_time: props.creation_time.as_nanos(),
                modification_time: props.modification_time.as_nanos(),
                access_time: props.access_time.as_nanos(),
                mode: props.posix_attributes.map(|a| a.mode),
                uid: props.posix_attributes.map(|a| a.uid),
                gid: props.posix_attributes.map(|a| a.gid),
                rdev: props.posix_attributes.map(|a| a.rdev),
                casefold: self.directory.dir_type().is_casefold(),
                selinux_context: crate::get_inline_selinux_context(self.directory.handle())?,
                encryption_policy: props.dir_type.fscrypt_policy(),
            },
            Immutable {
                protocols: fio::NodeProtocolKinds::DIRECTORY,
                abilities: DIRECTORY_ABILITIES,
                content_size: props.data_attribute_size,
                storage_size: props.allocated_size,
                link_count: props.refs + 1 + props.sub_dirs,
                id: self.directory.object_id(),
                change_time: props.change_time.as_nanos(),
                verity_enabled: false,
            }
        ))
    }

    pub fn get_extended_attribute(&self, name: Vec<u8>) -> Result<Vec<u8>, Error> {
        ensure!(!self.directory.is_deleted(), FxfsError::Deleted);
        RemoteObjectStore::get_extended_attribute(self.directory.handle(), name)
    }

    pub fn list_extended_attributes(&self) -> Result<Vec<Vec<u8>>, Error> {
        block_on(self.directory.list_extended_attributes())
    }
}
