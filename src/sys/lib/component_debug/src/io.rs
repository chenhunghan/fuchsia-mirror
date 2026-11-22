// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// TODO(https://fxbug.dev/42144933): Migrate to the new library that substitutes io_utils and fuchsia_fs::directory.
// Ask for host-side support on the new library (fxr/467217).

use anyhow::{Error, Result, anyhow, format_err};
use async_trait::async_trait;
use futures::lock::Mutex;
use std::path::{Path, PathBuf};
use std::{env, fs};
use zx_status::Status;

#[cfg(feature = "fdomain")]
use fuchsia_fs_fdomain as fuchsia_fs;

use flex_client::ProxyHasDomain;
use flex_fuchsia_io as fio;
use fuchsia_fs::directory::{DirEntry, open_directory_async, open_file_async, readdir};
use fuchsia_fs::file::{close, read, read_to_string, write};

/// Reads the whole file, pipelining requests where that is enabled.
async fn read_contents(file: &fio::FileProxy) -> Result<Vec<u8>> {
    #[cfg(any(feature = "fdomain", test))]
    {
        let depth = pipeline::depth();
        if depth > 1 {
            return pipeline::read(file, depth).await;
        }
    }
    Ok(read(file).await?)
}

/// Writes `data` from offset 0, pipelining requests where that is enabled.
async fn write_contents(file: &fio::FileProxy, data: &[u8]) -> Result<()> {
    #[cfg(any(feature = "fdomain", test))]
    {
        let depth = pipeline::depth();
        if depth > 1 {
            return pipeline::write(file, data, depth).await;
        }
    }
    Ok(write(file, data).await?)
}

/// Pipelined file reads and writes.
///
/// Each fuchsia.io read or write moves at most `fio::MAX_BUF` (8 KiB), so a
/// sequential copy costs one round trip per 8 KiB. Keeping several requests in
/// flight hides that latency on slow links, such as ffx over USB. It is built
/// only for the FDomain variant that ffx uses: on-device users such as the
/// `component` tool talk over local channels and gain nothing from it. Unit
/// tests, which build without FDomain, include it so that it is covered.
#[cfg(any(feature = "fdomain", test))]
mod pipeline {
    use super::*;
    use futures::TryStreamExt;
    use futures::stream::{FuturesOrdered, FuturesUnordered};
    use std::cmp::min;

    /// Environment variable that sets how many read or write requests are kept
    /// in flight per file. A value of 0 or 1 selects the sequential
    /// implementation.
    const DEPTH_ENV: &str = "FFX_COPY_PIPELINE_DEPTH";

    /// Default number of concurrent read or write requests per file. On a USB
    /// link with about 80 ms round trips, throughput stops improving past 64.
    const DEFAULT_DEPTH: usize = 64;

    /// Largest accepted depth. Every request in flight is a message queued on
    /// the file's channel, and Zircon kills a process that queues more than
    /// 3500 messages on one channel, so stay well below that.
    pub(super) const MAX_DEPTH: usize = 256;

    pub(super) fn depth() -> usize {
        parse_depth(env::var(DEPTH_ENV).ok().as_deref())
    }

    pub(super) fn parse_depth(value: Option<&str>) -> usize {
        value.and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_DEPTH).min(MAX_DEPTH)
    }

    /// Returns true if `error` carries one of `statuses`.
    fn has_status(error: &Error, statuses: &[Status]) -> bool {
        error.downcast_ref::<Status>().is_some_and(|status| statuses.contains(status))
    }

    /// Reads `len` bytes at `offset`, retrying short reads. Returns fewer bytes
    /// only if the end of the file is reached.
    async fn read_exact_at(file: &fio::FileProxy, offset: u64, len: u64) -> Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(usize::try_from(len)?);
        while (buf.len() as u64) < len {
            let done = buf.len() as u64;
            let bytes =
                file.read_at(len - done, offset + done).await?.map_err(Status::err_from_raw)?;
            if bytes.is_empty() {
                break;
            }
            buf.extend_from_slice(&bytes);
        }
        Ok(buf)
    }

    /// Reads the whole file with up to `depth` `ReadAt` requests in flight.
    pub(super) async fn read(file: &fio::FileProxy, depth: usize) -> Result<Vec<u8>> {
        let size = match file.get_attributes(fio::NodeAttributesQuery::CONTENT_SIZE).await? {
            Ok((_, immutable_attributes)) => immutable_attributes.content_size.unwrap_or(0),
            Err(raw) if raw == Status::NOT_SUPPORTED.into_raw() => 0,
            Err(raw) => return Err(Status::err_from_raw(raw).into()),
        };

        // A node that reports no size, such as a Starnix pseudo-file, may not
        // be seekable and may reject `ReadAt` even at offset 0, so read it
        // sequentially.
        if size == 0 {
            return Ok(fuchsia_fs::file::read(file).await?);
        }

        // The size comes from the remote end, so fail cleanly instead of
        // aborting if it cannot be allocated.
        let capacity =
            usize::try_from(size).map_err(|_| format_err!("file size {} is too large", size))?;
        let mut out = Vec::new();
        out.try_reserve_exact(capacity)
            .map_err(|e| format_err!("cannot allocate {} bytes for the file: {}", size, e))?;

        // Keep at most `depth` requests in flight and append each chunk as it
        // arrives, so only those chunks are buffered beyond `out`.
        let chunk_size = fio::MAX_BUF;
        let mut in_flight = FuturesOrdered::new();
        let mut next_offset = 0;
        loop {
            while in_flight.len() < depth && next_offset < size {
                let len = min(chunk_size, size - next_offset);
                in_flight.push_back(read_exact_at(file, next_offset, len));
                next_offset += len;
            }
            let Some(chunk) = in_flight.try_next().await? else {
                break;
            };
            // Every chunk is requested at its full expected length, so a short
            // chunk, including the last, means the file shrank during the read.
            let expected = min(chunk_size, size - out.len() as u64);
            if chunk.len() as u64 != expected {
                return Err(format_err!("file shrank while it was being read"));
            }
            out.extend_from_slice(&chunk);
        }

        // Read anything past the size reported above, in case the file grew.
        loop {
            let bytes =
                file.read_at(chunk_size, out.len() as u64).await?.map_err(Status::err_from_raw)?;
            if bytes.is_empty() {
                break;
            }
            out.extend_from_slice(&bytes);
        }
        Ok(out)
    }

    /// Writes all of `data` at `offset`, retrying short writes.
    async fn write_all_at(file: &fio::FileProxy, offset: u64, data: &[u8]) -> Result<()> {
        let mut written = 0;
        while written < data.len() {
            let n = file
                .write_at(&data[written..], offset + written as u64)
                .await?
                .map_err(Status::err_from_raw)?;
            if n == 0 || n as usize > data.len() - written {
                return Err(format_err!("unexpected write size {} at offset {}", n, offset));
            }
            written += n as usize;
        }
        Ok(())
    }

    /// Writes `data` from offset 0 with up to `depth` `WriteAt` requests in
    /// flight.
    pub(super) async fn write(file: &fio::FileProxy, data: &[u8], depth: usize) -> Result<()> {
        let chunk_size = fio::MAX_BUF as usize;
        let mut chunks = data.chunks(chunk_size).enumerate();

        // Write the first chunk on its own. A file that is not seekable, such
        // as a Starnix pseudo-file, may reject `WriteAt` even at offset 0. Its
        // seek offset is still 0 then, so fall back to sequential writes.
        let Some((_, first)) = chunks.next() else {
            return Ok(());
        };
        if let Err(e) = write_all_at(file, 0, first).await {
            if has_status(&e, &[Status::NOT_SUPPORTED, Status::INVALID_ARGS]) {
                return Ok(fuchsia_fs::file::write(file, data).await?);
            }
            return Err(e);
        }

        let mut in_flight = FuturesUnordered::new();
        loop {
            while in_flight.len() < depth {
                let Some((i, chunk)) = chunks.next() else {
                    break;
                };
                in_flight.push(write_all_at(file, (i * chunk_size) as u64, chunk));
            }
            if in_flight.try_next().await?.is_none() {
                break;
            }
        }
        Ok(())
    }
}

pub enum DirentKind {
    File,
    Directory,
}

#[async_trait]
pub trait Directory: Sized {
    /// Attempts to open the directory at `relative_path` with read-only rights.
    fn open_dir_readonly<P: AsRef<Path> + Send>(&self, relative_path: P) -> Result<Self>;

    /// Attempts to open the directory at `relative_path` with read/write rights.
    fn open_dir_readwrite<P: AsRef<Path> + Send>(&self, relative_path: P) -> Result<Self>;

    /// Attempts to create a directory at `relative_path` with read right (if `readwrite` is false) or read/write rights (if `readwrite` is true).
    fn create_dir<P: AsRef<Path> + Send>(&self, relative_path: P, readwrite: bool) -> Result<Self>;

    /// Return a copy of self.
    fn clone(&self) -> Result<Self>;

    /// Returns the contents of the file at `relative_path` as a string.
    async fn read_file<P: AsRef<Path> + Send>(&self, relative_path: P) -> Result<String>;

    /// Returns the contents of the file at `relative_path` as bytes.
    async fn read_file_bytes<P: AsRef<Path> + Send>(&self, relative_path: P) -> Result<Vec<u8>>;

    /// Returns true if an entry called `filename` exists in this directory. `filename` must be
    /// a plain file name, not a relative path.
    async fn exists(&self, filename: &str) -> Result<bool>;

    /// Returns the type of entry specified by `filename`, or None if no entry by that name
    /// is found. `filename` must be a plan file name, not a relative path.
    async fn entry_type(&self, filename: &str) -> Result<Option<DirentKind>>;

    /// Deletes the file at `relative_path`.
    async fn remove(&self, relative_path: &str) -> Result<()>;

    /// Writes `data` to a file at `relative_path`. Overwrites if the file already
    /// exists.
    async fn write_file<P: AsRef<Path> + Send>(&self, relative_path: P, data: &[u8]) -> Result<()>;

    /// Returns the size of the file at `relative_path` in bytes.
    async fn get_file_size<P: AsRef<Path> + Send>(&self, relative_path: P) -> Result<u64>;

    /// Returns a list of directory entry names as strings.
    async fn entry_names(&self) -> Result<Vec<String>>;
}

/// A convenience wrapper over a local directory.
#[derive(Debug)]
pub struct LocalDirectory {
    path: PathBuf,
}

impl LocalDirectory {
    /// Returns a new local directory wrapper with no base path component. All operations will
    /// be on paths relative to the environment used by std::fs.
    pub fn new() -> Self {
        LocalDirectory { path: PathBuf::new() }
    }

    /// Returns a new local directory such that the methods on `Self` will
    /// operate as expected on `path`:
    ///
    ///     - if `path` is absolute, returns a directory at "/"
    ///     - if `path` is relative, returns a directory at `cwd`
    pub fn for_path(path: &PathBuf) -> Self {
        if path.is_absolute() {
            LocalDirectory { path: "/".into() }
        } else {
            LocalDirectory { path: env::current_dir().unwrap() }
        }
    }
}

#[async_trait]
impl Directory for LocalDirectory {
    fn open_dir_readonly<P: AsRef<Path> + Send>(&self, relative_path: P) -> Result<Self> {
        let full_path = self.path.join(relative_path);
        Ok(LocalDirectory { path: full_path })
    }

    fn open_dir_readwrite<P: AsRef<Path> + Send>(&self, relative_path: P) -> Result<Self> {
        self.open_dir_readonly(relative_path)
    }

    fn create_dir<P: AsRef<Path> + Send>(
        &self,
        relative_path: P,
        _readwrite: bool,
    ) -> Result<Self> {
        let full_path = self.path.join(relative_path);
        fs::create_dir(full_path.clone())?;
        Ok(LocalDirectory { path: full_path })
    }

    fn clone(&self) -> Result<Self> {
        Ok(LocalDirectory { path: self.path.clone() })
    }

    async fn read_file<P: AsRef<Path> + Send>(&self, relative_path: P) -> Result<String> {
        let full_path = self.path.join(relative_path);
        fs::read_to_string(full_path).map_err(Into::into)
    }

    async fn read_file_bytes<P: AsRef<Path> + Send>(&self, relative_path: P) -> Result<Vec<u8>> {
        let full_path = self.path.join(relative_path);
        fs::read(full_path).map_err(Into::into)
    }

    async fn exists(&self, filename: &str) -> Result<bool> {
        Ok(self.path.join(filename).exists())
    }

    async fn entry_type(&self, filename: &str) -> Result<Option<DirentKind>> {
        let full_path = self.path.join(filename);
        if !full_path.exists() {
            return Ok(None);
        }
        let metadata = fs::metadata(full_path)?;
        if metadata.is_file() {
            Ok(Some(DirentKind::File))
        } else if metadata.is_dir() {
            Ok(Some(DirentKind::Directory))
        } else {
            Err(anyhow!("Unsupported entry type: {:?}", metadata.file_type()))
        }
    }

    async fn remove(&self, relative_path: &str) -> Result<()> {
        let full_path = self.path.join(relative_path);
        fs::remove_file(full_path).map_err(Into::into)
    }

    async fn write_file<P: AsRef<Path> + Send>(&self, relative_path: P, data: &[u8]) -> Result<()> {
        let full_path = self.path.join(relative_path);
        fs::write(full_path, data).map_err(Into::into)
    }

    async fn get_file_size<P: AsRef<Path> + Send>(&self, relative_path: P) -> Result<u64> {
        let full_path = self.path.join(relative_path);
        let metadata = fs::metadata(full_path)?;
        if metadata.is_file() {
            Ok(metadata.len())
        } else {
            Err(anyhow!("Cannot get size of non-file"))
        }
    }

    async fn entry_names(&self) -> Result<Vec<String>> {
        let paths = fs::read_dir(self.path.clone())?;
        Ok(paths.into_iter().map(|p| p.unwrap().file_name().into_string().unwrap()).collect())
    }
}

/// A convenience wrapper over a FIDL DirectoryProxy.
#[derive(Debug)]
pub struct RemoteDirectory {
    path: PathBuf,
    proxy: fio::DirectoryProxy,
    // The `fuchsia.io.RemoteDirectory` protocol is stateful in readdir, and the associated `fuchsia_fs::directory`
    // library used for enumerating the directory has no mechanism for synchronization of readdir
    // operations, as such this mutex must be held throughout directory enumeration in order to
    // avoid race conditions from concurrent rewinds and reads.
    readdir_mutex: Mutex<()>,
}

impl RemoteDirectory {
    #[cfg(target_os = "fuchsia")]
    pub fn from_namespace<P: AsRef<Path> + Send>(path: P) -> Result<Self> {
        let path_str = path
            .as_ref()
            .as_os_str()
            .to_str()
            .ok_or_else(|| format_err!("could not convert path to string"))?;
        let proxy = fuchsia_fs::directory::open_in_namespace(path_str, fio::PERM_READABLE)?;
        let path = path.as_ref().to_path_buf();
        Ok(Self { path, proxy, readdir_mutex: Mutex::new(()) })
    }

    pub fn from_proxy(proxy: fio::DirectoryProxy) -> Self {
        let path = PathBuf::from(".");
        Self { path, proxy, readdir_mutex: Mutex::new(()) }
    }

    pub fn clone_proxy(&self) -> Result<fio::DirectoryProxy> {
        let (cloned_proxy, clone_server) =
            self.proxy.domain().create_proxy::<fio::DirectoryMarker>();
        self.proxy.clone(clone_server.into_channel().into())?;
        Ok(cloned_proxy)
    }

    async fn entries(&self) -> Result<Vec<DirEntry>, Error> {
        let _lock = self.readdir_mutex.lock().await;
        match readdir(&self.proxy).await {
            Ok(entries) => Ok(entries),
            Err(e) => Err(format_err!(
                "could not get entries of `{}`: {}",
                self.path.as_path().display(),
                e
            )),
        }
    }

    fn open_dir<P: AsRef<Path> + Send>(&self, relative_path: P, flags: fio::Flags) -> Result<Self> {
        let path = self.path.join(relative_path.as_ref());
        let relative_path = match relative_path.as_ref().to_str() {
            Some(relative_path) => relative_path,
            None => return Err(format_err!("could not convert relative path to &str")),
        };
        match open_directory_async(&self.proxy, relative_path, flags) {
            Ok(proxy) => Ok(Self { path, proxy, readdir_mutex: Mutex::new(()) }),
            Err(e) => Err(format_err!("could not open dir `{}`: {}", path.as_path().display(), e)),
        }
    }
}

#[async_trait]
impl Directory for RemoteDirectory {
    fn open_dir_readonly<P: AsRef<Path> + Send>(&self, relative_path: P) -> Result<Self> {
        self.open_dir(relative_path, fio::PERM_READABLE)
    }

    fn open_dir_readwrite<P: AsRef<Path> + Send>(&self, relative_path: P) -> Result<Self> {
        self.open_dir(relative_path, fio::PERM_READABLE | fio::PERM_WRITABLE)
    }

    fn create_dir<P: AsRef<Path> + Send>(&self, relative_path: P, readwrite: bool) -> Result<Self> {
        let mut flags = fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE;
        if readwrite {
            flags = flags | fio::PERM_WRITABLE;
        }
        self.open_dir(relative_path, flags)
    }

    async fn read_file<P: AsRef<Path> + Send>(&self, relative_path: P) -> Result<String> {
        let path = self.path.join(relative_path.as_ref());
        let relative_path = match relative_path.as_ref().to_str() {
            Some(relative_path) => relative_path,
            None => return Err(format_err!("relative path is not valid unicode")),
        };

        let proxy = match open_file_async(&self.proxy, relative_path, fio::PERM_READABLE) {
            Ok(proxy) => proxy,
            Err(e) => {
                return Err(format_err!(
                    "could not open file `{}`: {}",
                    path.as_path().display(),
                    e
                ));
            }
        };

        match read_to_string(&proxy).await {
            Ok(data) => Ok(data),
            Err(e) => Err(format_err!(
                "could not read file `{}` as string: {}",
                path.as_path().display(),
                e
            )),
        }
    }

    async fn read_file_bytes<P: AsRef<Path> + Send>(&self, relative_path: P) -> Result<Vec<u8>> {
        let path = self.path.join(relative_path.as_ref());
        let relative_path = match relative_path.as_ref().to_str() {
            Some(relative_path) => relative_path,
            None => return Err(format_err!("relative path is not valid unicode")),
        };

        let proxy = match open_file_async(&self.proxy, relative_path, fio::PERM_READABLE) {
            Ok(proxy) => proxy,
            Err(e) => {
                return Err(format_err!(
                    "could not open file `{}`: {}",
                    path.as_path().display(),
                    e
                ));
            }
        };

        match read_contents(&proxy).await {
            Ok(data) => Ok(data),
            Err(e) => Err(format_err!("could not read file `{}`: {}", path.as_path().display(), e)),
        }
    }

    async fn exists(&self, filename: &str) -> Result<bool> {
        let filename = PathBuf::from(filename);
        let mut path_iter = filename.iter().peekable();
        let path_element = path_iter.next().ok_or_else(|| format_err!("path is empty"))?;
        match path_iter.peek() {
            Some(_) => {
                let sub_directory = self.open_dir_readonly(path_element)?;
                let sub_path = path_iter.collect::<PathBuf>();
                let sub_path_str =
                    sub_path.to_str().ok_or_else(|| format_err!("path is not valid utf8"))?;
                sub_directory.exists(sub_path_str).await
            }
            None => match self.entry_names().await {
                Ok(entries) => Ok(entries.iter().any(|s| s.as_str() == path_element)),
                Err(e) => Err(format_err!(
                    "could not check if `{}` exists in `{}`: {}",
                    filename.display(),
                    self.path.as_path().display(),
                    e
                )),
            },
        }
    }

    async fn entry_type(&self, filename: &str) -> Result<Option<DirentKind>> {
        let entries = self.entries().await?;

        entries
            .into_iter()
            .find(|e| e.name == filename)
            .map(|e| {
                match e.kind {
                    // TODO(https://fxbug.dev/42077929): Update component_manager vfs to assign proper DirentType when installing the directory tree.
                    fio::DirentType::Directory | fio::DirentType::Unknown => {
                        Ok(Some(DirentKind::Directory))
                    }
                    fio::DirentType::File => Ok(Some(DirentKind::File)),
                    _ => {
                        return Err(anyhow!(
                            "Unsupported entry type for file {}: {:?}",
                            filename,
                            e.kind,
                        ));
                    }
                }
            })
            .unwrap_or(Ok(None))
    }

    async fn remove(&self, filename: &str) -> Result<()> {
        let filename = PathBuf::from(filename);
        let mut path_iter = filename.iter().peekable();
        let path_element = path_iter.next().ok_or_else(|| {
            format_err!(
                "could not delete `{}` from `{}: path is empty",
                filename.display(),
                self.path.as_path().display(),
            )
        })?;
        match path_iter.peek() {
            Some(_) => {
                let sub_directory = self.open_dir_readwrite(path_element)?;
                let sub_path = path_iter.collect::<PathBuf>();
                let sub_path_str = sub_path.to_str().ok_or_else(|| {
                    format_err!(
                        "could not delete `{}` from `{}: path is not valid utf8",
                        filename.display(),
                        self.path.as_path().display(),
                    )
                })?;
                sub_directory.remove(sub_path_str).await
            }
            None => {
                let options = fio::UnlinkOptions::default();
                let path_str =
                    path_element.to_str().ok_or_else(|| format_err!("path is not valid utf8"))?;
                match self.proxy.unlink(path_str, &options).await {
                    Ok(r) => match r {
                        Ok(()) => Ok(()),
                        Err(e) => Err(format_err!(
                            "could not delete `{}` from `{}`: {}",
                            path_element.display(),
                            self.path.as_path().display(),
                            e
                        )),
                    },
                    Err(e) => Err(format_err!(
                        "proxy error while deleting `{}` from `{}`: {}",
                        path_element.display(),
                        self.path.as_path().display(),
                        e
                    )),
                }
            }
        }
    }

    async fn write_file<P: AsRef<Path> + Send>(&self, relative_path: P, data: &[u8]) -> Result<()> {
        let path = self.path.join(relative_path.as_ref());
        let relative_path = match relative_path.as_ref().to_str() {
            Some(relative_path) => relative_path,
            None => return Err(format_err!("relative path is not valid unicode")),
        };

        let file = match open_file_async(
            &self.proxy,
            relative_path,
            fio::PERM_WRITABLE | fio::Flags::FLAG_MAYBE_CREATE,
        ) {
            Ok(proxy) => proxy,
            Err(e) => {
                return Err(format_err!(
                    "could not open file `{}`: {}",
                    path.as_path().display(),
                    e
                ));
            }
        };

        let () = file
            .resize(0)
            .await
            .map_err(|e| {
                format_err!("could not truncate file `{}`: {}", path.as_path().display(), e)
            })?
            .map_err(Status::err_from_raw)
            .map_err(|status| {
                format_err!("could not truncate file `{}`: {}", path.as_path().display(), status)
            })?;

        match write_contents(&file, data).await {
            Ok(()) => {}
            Err(e) => {
                return Err(format_err!(
                    "could not write to file `{}`: {}",
                    path.as_path().display(),
                    e
                ));
            }
        }

        match close(file).await {
            Ok(()) => Ok(()),
            Err(e) => {
                Err(format_err!("could not close file `{}`: {}", path.as_path().display(), e))
            }
        }
    }

    async fn get_file_size<P: AsRef<Path> + Send>(&self, relative_path: P) -> Result<u64> {
        let path = self.path.join(relative_path.as_ref());
        let relative_path = match relative_path.as_ref().to_str() {
            Some(relative_path) => relative_path,
            None => return Err(format_err!("relative path is not valid unicode")),
        };

        let file = match open_file_async(&self.proxy, relative_path, fio::PERM_READABLE) {
            Ok(proxy) => proxy,
            Err(e) => {
                return Err(format_err!(
                    "could not open file `{}`: {}",
                    path.as_path().display(),
                    e
                ));
            }
        };
        let (_, immutable_attributes) = file
            .get_attributes(fio::NodeAttributesQuery::STORAGE_SIZE)
            .await
            .map_err(|e| {
                format_err!("Unexpected FIDL error during file attribute retrieval: {}", e)
            })?
            .unwrap();
        Ok(immutable_attributes.storage_size.unwrap_or_default())
    }

    async fn entry_names(&self) -> Result<Vec<String>> {
        match self.entries().await {
            Ok(entries) => Ok(entries.into_iter().map(|e| e.name).collect()),
            Err(e) => Err(format_err!(
                "could not get entry names of `{}`: {}",
                self.path.as_path().display(),
                e
            )),
        }
    }

    fn clone(&self) -> Result<Self> {
        let proxy = open_directory_async(&self.proxy, ".", fio::PERM_READABLE)?;
        Ok(Self { path: self.path.clone(), proxy, readdir_mutex: Mutex::new(()) })
    }
}

#[cfg(all(test, target_os = "fuchsia"))]
mod tests {
    use super::*;
    use futures::TryStreamExt;
    use std::cell::RefCell;
    use std::rc::Rc;
    use tempfile::TempDir;
    use test_case::test_case;

    const CHUNK: usize = fio::MAX_BUF as usize;

    fn open_tmp_dir() -> (TempDir, RemoteDirectory) {
        let tmp_dir = TempDir::new_in("/tmp").unwrap();
        let proxy = fuchsia_fs::directory::open_in_namespace(
            tmp_dir.path().to_str().unwrap(),
            fio::PERM_READABLE | fio::PERM_WRITABLE,
        )
        .unwrap();
        (tmp_dir, RemoteDirectory::from_proxy(proxy))
    }

    /// Returns `len` bytes where each 8 KiB chunk differs, so that a chunk
    /// written or read at the wrong offset is detected.
    fn test_data(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i / CHUNK) as u8).wrapping_mul(31).wrapping_add(i as u8)).collect()
    }

    fn open_file(dir: &RemoteDirectory, name: &str, flags: fio::Flags) -> fio::FileProxy {
        open_file_async(&dir.proxy, name, flags).unwrap()
    }

    #[test_case(None, 64; "unset")]
    #[test_case(Some("0"), 0; "zero")]
    #[test_case(Some("1"), 1; "one")]
    #[test_case(Some("128"), 128; "in range")]
    #[test_case(Some("256"), 256; "maximum")]
    #[test_case(Some("100000"), pipeline::MAX_DEPTH; "clamped")]
    #[test_case(Some("deep"), 64; "not a number")]
    fn parse_depth(value: Option<&str>, expected: usize) {
        assert_eq!(pipeline::parse_depth(value), expected);
    }

    #[test_case(0, 4; "empty")]
    #[test_case(1, 4; "one byte")]
    #[test_case(CHUNK - 1, 4; "under one chunk")]
    #[test_case(CHUNK, 4; "one chunk")]
    #[test_case(CHUNK + 1, 4; "over one chunk")]
    #[test_case(10 * CHUNK + 123, 4; "depth below chunk count")]
    #[test_case(10 * CHUNK + 123, 64; "depth above chunk count")]
    #[fuchsia::test]
    async fn read_pipelined_returns_whole_file(len: usize, depth: usize) {
        let (tmp_dir, dir) = open_tmp_dir();
        let data = test_data(len);
        std::fs::write(tmp_dir.path().join("f"), &data).unwrap();

        let file = open_file(&dir, "f", fio::PERM_READABLE);
        assert_eq!(pipeline::read(&file, depth).await.unwrap(), data);
    }

    /// Returns the bytes of `data` in `[offset, offset + count)` that exist.
    fn slice_at(data: &[u8], offset: u64, count: u64) -> &[u8] {
        let start = std::cmp::min(offset as usize, data.len());
        let end = std::cmp::min(start + count as usize, data.len());
        &data[start..end]
    }

    /// Writes `data` into `contents` at `offset`, extending it as needed.
    fn write_at(contents: &mut Vec<u8>, offset: u64, data: &[u8]) {
        let start = offset as usize;
        let end = start + data.len();
        if contents.len() < end {
            contents.resize(end, 0);
        }
        contents[start..end].copy_from_slice(data);
    }

    /// Serves a file whose contents live in `contents`.
    ///
    /// `GetAttributes` reports `reported_size`, or fails with `NOT_SUPPORTED`
    /// when it is `None`. A file that is not `seekable` rejects `ReadAt` and
    /// `WriteAt` with `NOT_SUPPORTED`, like a non-seekable Starnix file, but
    /// serves `Read` and `Write` at its seek offset.
    fn serve_fake_file(
        contents: Rc<RefCell<Vec<u8>>>,
        reported_size: Option<u64>,
        seekable: bool,
    ) -> fio::FileProxy {
        let (proxy, mut stream) = fidl::endpoints::create_proxy_and_stream::<fio::FileMarker>();
        fuchsia_async::Task::local(async move {
            let mut seek_offset = 0;
            while let Some(request) = stream.try_next().await.unwrap() {
                match request {
                    fio::FileRequest::GetAttributes { responder, .. } => match reported_size {
                        Some(size) => {
                            let immutable_attributes = fio::ImmutableNodeAttributes {
                                content_size: Some(size),
                                ..Default::default()
                            };
                            responder
                                .send(Ok((
                                    &fio::MutableNodeAttributes::default(),
                                    &immutable_attributes,
                                )))
                                .unwrap();
                        }
                        None => responder.send(Err(Status::NOT_SUPPORTED.into_raw())).unwrap(),
                    },
                    fio::FileRequest::ReadAt { responder, .. } if !seekable => {
                        responder.send(Err(Status::NOT_SUPPORTED.into_raw())).unwrap();
                    }
                    fio::FileRequest::ReadAt { count, offset, responder } => {
                        responder.send(Ok(slice_at(&contents.borrow(), offset, count))).unwrap();
                    }
                    fio::FileRequest::Read { count, responder } => {
                        let contents = contents.borrow();
                        let bytes = slice_at(&contents, seek_offset, count);
                        seek_offset += bytes.len() as u64;
                        responder.send(Ok(bytes)).unwrap();
                    }
                    fio::FileRequest::WriteAt { responder, .. } if !seekable => {
                        responder.send(Err(Status::NOT_SUPPORTED.into_raw())).unwrap();
                    }
                    fio::FileRequest::WriteAt { data, offset, responder } => {
                        write_at(&mut contents.borrow_mut(), offset, &data);
                        responder.send(Ok(data.len() as u64)).unwrap();
                    }
                    fio::FileRequest::Write { data, responder } => {
                        write_at(&mut contents.borrow_mut(), seek_offset, &data);
                        seek_offset += data.len() as u64;
                        responder.send(Ok(data.len() as u64)).unwrap();
                    }
                    request => panic!("unexpected request: {:?}", request),
                }
            }
        })
        .detach();
        proxy
    }

    fn serve_fake_file_with_data(
        data: Vec<u8>,
        reported_size: Option<u64>,
        seekable: bool,
    ) -> fio::FileProxy {
        serve_fake_file(Rc::new(RefCell::new(data)), reported_size, seekable)
    }

    #[test_case(50, 100; "small file")]
    #[test_case(2 * CHUNK + 10, 3 * CHUNK; "short last chunk")]
    #[test_case(CHUNK, 3 * CHUNK; "missing last chunks")]
    #[fuchsia::test]
    async fn read_pipelined_fails_if_file_shrinks(actual_len: usize, reported_len: usize) {
        let file =
            serve_fake_file_with_data(test_data(actual_len), Some(reported_len as u64), true);
        let err = pipeline::read(&file, 4).await.unwrap_err();
        assert!(err.to_string().contains("shrank"), "unexpected error: {err:#}");
    }

    #[fuchsia::test]
    async fn read_pipelined_fails_if_size_cannot_be_allocated() {
        let file = serve_fake_file_with_data(test_data(10), Some(u64::MAX), true);
        let err = pipeline::read(&file, 4).await.unwrap_err();
        assert!(err.to_string().contains("allocate"), "unexpected error: {err:#}");
    }

    // A file that reports no size is read with `Read`, which works even if the
    // file is not seekable.
    #[test_case(0, None; "empty without content size")]
    #[test_case(3 * CHUNK + 5, None; "several chunks without content size")]
    #[test_case(3 * CHUNK + 5, Some(0); "several chunks with zero content size")]
    #[fuchsia::test]
    async fn read_pipelined_reads_unsized_file_sequentially(
        len: usize,
        reported_size: Option<u64>,
    ) {
        let data = test_data(len);
        let file = serve_fake_file_with_data(data.clone(), reported_size, false);
        assert_eq!(pipeline::read(&file, 4).await.unwrap(), data);
    }

    #[fuchsia::test]
    async fn read_pipelined_reads_past_reported_size() {
        let data = test_data(2 * CHUNK + 7);
        let file = serve_fake_file_with_data(data.clone(), Some(CHUNK as u64), true);
        assert_eq!(pipeline::read(&file, 4).await.unwrap(), data);
    }

    #[test_case(0, 4; "empty")]
    #[test_case(1, 4; "one byte")]
    #[test_case(CHUNK, 4; "one chunk")]
    #[test_case(CHUNK + 1, 4; "over one chunk")]
    #[test_case(10 * CHUNK + 123, 4; "depth below chunk count")]
    #[test_case(10 * CHUNK + 123, 64; "depth above chunk count")]
    #[fuchsia::test]
    async fn write_pipelined_writes_whole_file(len: usize, depth: usize) {
        let (tmp_dir, dir) = open_tmp_dir();
        let data = test_data(len);

        let file = open_file(&dir, "f", fio::PERM_WRITABLE | fio::Flags::FLAG_MAYBE_CREATE);
        pipeline::write(&file, &data, depth).await.unwrap();
        close(file).await.unwrap();

        assert_eq!(std::fs::read(tmp_dir.path().join("f")).unwrap(), data);
    }

    #[test_case(1; "one byte")]
    #[test_case(3 * CHUNK + 5; "several chunks")]
    #[fuchsia::test]
    async fn write_pipelined_falls_back_for_non_seekable_file(len: usize) {
        let data = test_data(len);
        let contents = Rc::new(RefCell::new(Vec::new()));
        let file = serve_fake_file(contents.clone(), None, false);

        pipeline::write(&file, &data, 4).await.unwrap();

        assert_eq!(*contents.borrow(), data);
    }

    #[fuchsia::test]
    async fn write_file_replaces_longer_file() {
        let (tmp_dir, dir) = open_tmp_dir();
        std::fs::write(tmp_dir.path().join("f"), test_data(5 * CHUNK)).unwrap();
        let data = test_data(2 * CHUNK + 7);

        dir.write_file("f", &data).await.unwrap();

        assert_eq!(std::fs::read(tmp_dir.path().join("f")).unwrap(), data);
    }

    #[fuchsia::test]
    async fn read_file_bytes_round_trips_write_file() {
        let (_tmp_dir, dir) = open_tmp_dir();
        let data = test_data(3 * CHUNK + 5);

        dir.write_file("f", &data).await.unwrap();

        assert_eq!(dir.read_file_bytes("f").await.unwrap(), data);
    }
}
