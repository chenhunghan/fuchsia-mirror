// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::common::vars::{
    GBL_DISABLE_ASYNC_TASK_COMMAND, GBL_ENABLE_ASYNC_TASK_COMMAND, GBL_SYNC_TASKS_COMMAND,
    IS_USERSPACE_VAR, LOCKED_VAR, MAX_DOWNLOAD_SIZE_VAR, PARTITION_SIZE, PARTITION_START,
    PRODUCT_VAR, REVISION_VAR, STREAM_SEGMENT_SIZE, VERSION_GBL_VAR,
};
use crate::error::FfxFastbootError;
use crate::file_resolver::FileResolver;
use crate::manifest::{from_in_tree, from_local_product_bundle, from_path, from_sdk};
use crate::util::Event;
use base64::engine::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

type Result<T> = std::result::Result<T, FfxFastbootError>;
use assembly_partitions_config::UploadMethod;
use chrono::{Duration, Utc};
use ffx_config::EnvironmentContext;
use ffx_fastboot_interface::fastboot_interface::{
    FastbootError, FastbootInterface, FlashError, RebootEvent, StageError, UploadProgress,
};
use ffx_fastboot_interface::stream::{RawStreamIterator, SparseStreamIterator, StreamCommand};
use ffx_fastboot_interface::util::U32_SIZE;
use ffx_flash_manifest::{ManifestParams, OemFile, SSH_OEM_COMMAND};
use futures::prelude::*;
use futures::try_join;
use pbms::is_local_product_bundle;
use sdk::SdkVersion;
use sparse::reader::SparseReader;
use sparse::{build_sparse_writers_streaming, resparse_sparse_img_writers};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::num::{NonZeroU64, ParseIntError};
use std::path::PathBuf;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, oneshot};
use tokio::task::spawn_blocking;

pub const MISSING_CREDENTIALS: &str = "The flash manifest is missing the credential files to unlock this device.\n\
     Please unlock the target and try again.";

pub mod caching;
pub mod crypto;
pub mod vars;

pub use caching::CachingFastboot;

pub trait Partition {
    fn name(&self) -> &str;
    fn file(&self) -> &str;
    fn variable(&self) -> Option<&str>;
    fn variable_value(&self) -> Option<&str>;
}

pub trait Product<P> {
    fn name(&self) -> &String;
    fn bootloader_partitions(&self) -> &Vec<P>;
    fn partitions(&self) -> &Vec<P>;
    fn oem_files(&self) -> &Vec<OemFile>;
}

/// Slot suffixes that are ignored when matching partitions against `skip_partitions`.
const SLOT_SUFFIXES: [&str; 3] = ["_a", "_b", "_r"];

/// Returns `name` without its slot suffix, if it has one.
fn partition_base_name(name: &str) -> &str {
    SLOT_SUFFIXES.iter().find_map(|suffix| name.strip_suffix(suffix)).unwrap_or(name)
}

/// Returns the partitions in `partitions` whose base name isn't in `skip_partitions`.
fn unskipped_partitions<'a, P: Partition>(
    partitions: &'a [P],
    skip_partitions: &'a [String],
) -> impl Iterator<Item = &'a P> + Clone {
    partitions
        .iter()
        .filter(|p| !skip_partitions.iter().any(|s| s == partition_base_name(p.name())))
}

/// Checks that every entry in `skip_partitions` is a base name that matches at least one of
/// `product`'s partitions, so that typos or slotted names fail before anything is flashed.
pub fn check_skip_partitions<Part: Partition, P: Product<Part>>(
    product: &P,
    skip_partitions: &[String],
) -> Result<()> {
    let all_partitions = || product.bootloader_partitions().iter().chain(product.partitions());
    for name in skip_partitions {
        // Reject slotted names: skipping individual slots isn't supported, so the caller must
        // pass the base name, which skips every slot.
        let base_name = partition_base_name(name);
        if base_name != name {
            return Err(FfxFastbootError::SlottedSkipPartition {
                name: name.clone(),
                base_name: base_name.to_string(),
            });
        }
        if !all_partitions().any(|p| partition_base_name(p.name()) == name) {
            return Err(FfxFastbootError::UnknownSkipPartition {
                name: name.clone(),
                available: all_partitions().map(|p| p.name().to_string()).collect(),
            });
        }
    }
    Ok(())
}

pub trait Flash {
    fn flash<F, T>(
        &self,
        messenger: &Sender<Event>,
        file_resolver: &mut F,
        fastboot_interface: &mut T,
        cmd: ManifestParams,
        ssh_key_upload_method: Option<&UploadMethod>,
    ) -> impl Future<Output = Result<()>>
    where
        F: FileResolver + Sync + Send,
        T: FastbootInterface;
}

pub trait Unlock {
    fn unlock<F, T>(
        &self,
        _messenger: &Sender<Event>,
        _file_resolver: &mut F,
        _fastboot_interface: &mut T,
    ) -> impl Future<Output = Result<()>>
    where
        F: FileResolver + Sync + Send,
        T: FastbootInterface,
    {
        async { Err(FfxFastbootError::UnlockNotSupported) }
    }
}

pub trait Boot {
    fn boot<F, T>(
        &self,
        messenger: Sender<Event>,
        file_resolver: &mut F,
        slot: String,
        fastboot_interface: &mut T,
        cmd: ManifestParams,
    ) -> impl Future<Output = Result<()>>
    where
        F: FileResolver + Sync + Send,
        T: FastbootInterface;
}

pub const MISSING_PRODUCT: &str = "Manifest does not contain product";

const LOCK_COMMAND: &str = "vx-lock";

pub const UNLOCK_ERR: &str = "The product requires the target to be unlocked. \
                                     Please unlock target and try again.";

pub async fn stage_file<F: FileResolver + Sync, T: FastbootInterface>(
    prog_client: Sender<UploadProgress>,
    file_resolver: &mut F,
    resolve: bool,
    file: &str,
    fastboot_interface: &mut T,
) -> Result<()> {
    let file_to_upload =
        if resolve { file_resolver.get_file(file).await? } else { file.to_string() };
    log::debug!("Preparing to stage {}", file_to_upload);
    fastboot_interface.stage(&file_to_upload, prog_client).await?;
    Ok(())
}

async fn do_flash_dynamic_sparse<F: FastbootInterface>(
    name: &str,
    messenger: &Sender<Event>,
    fastboot_interface: &mut F,
    size: u32,
    reader: &mut (dyn Read + Send),
    timeout: Duration,
) -> Result<()> {
    let (prog_client, mut prog_server): (Sender<UploadProgress>, Receiver<UploadProgress>) =
        mpsc::channel(1);
    try_join!(
        fastboot_interface
            .flash_from_reader(name, size, reader, prog_client, timeout)
            .map_err(FfxFastbootError::Interface),
        async {
            loop {
                match prog_server.recv().await {
                    Some(upload) => {
                        messenger.send(Event::Upload(upload)).await?;
                    }
                    None => {
                        messenger
                            .send(Event::FlashPartition { partition_name: name.to_string() })
                            .await?;
                        return Ok(());
                    }
                }
            }
        }
    )?;
    Ok(())
}

async fn flash_partition_sparse<F: FastbootInterface>(
    name: &str,
    messenger: &Sender<Event>,
    file_to_upload: &str,
    fastboot_interface: &mut F,
    max_download_size: u64,
    timeout: Duration,
) -> Result<()> {
    log::debug!("Preparing to flash {} in sparse mode", file_to_upload);

    let mut in_file = File::open(file_to_upload).map_err(|e| FfxFastbootError::FileOpen {
        path: PathBuf::from(file_to_upload),
        source: e,
    })?;
    let file_size = in_file
        .metadata()
        .map_err(|e| FfxFastbootError::FileMetadata {
            path: PathBuf::from(file_to_upload),
            source: e,
        })?
        .len();

    let estimated_files = (file_size / max_download_size) + 1;
    messenger
        .send(Event::Upload(UploadProgress::OnReady {
            partition: name.to_owned(),
            files: estimated_files,
        }))
        .await?;

    let (sender, mut receiver) = mpsc::channel::<sparse::SparseFileWriter>(2);
    let name_clone = name.to_string();
    let file_to_upload_clone = file_to_upload.to_string();

    let producer = tokio::task::spawn_blocking(move || {
        build_sparse_writers_streaming(
            &name_clone,
            &file_to_upload_clone,
            max_download_size,
            |writer| {
                sender.blocking_send(writer).map_err(|_| sparse::SparseError::UnalignedChunk)?;
                Ok(())
            },
        )
    });

    while let Some(writer) = receiver.recv().await {
        let size = u32::try_from(writer.file_size())?;
        let mut slice_reader = writer.slice_reader(&mut in_file)?;
        do_flash_dynamic_sparse(
            name,
            messenger,
            fastboot_interface,
            size,
            &mut slice_reader,
            timeout,
        )
        .await?;
    }

    producer.await.map_err(FfxFastbootError::JoinError)??;

    Ok(())
}

async fn flash_basic_impl<T: FastbootInterface>(
    messenger: &Sender<Event>,
    name: &str,
    file_to_upload: &str,
    fastboot_interface: &mut T,
    min_timeout_secs: u64,
    flash_timeout_rate_mb_per_second: f64,
    override_max_download_size: Option<NonZeroU64>,
    manage_gbl_async: bool,
) -> Result<()> {
    // If the given file to flash is bigger than what the device can download
    // at once, we need to make a sparse image out of the given file
    let mut file_handle = File::open(&file_to_upload).map_err(|e| FfxFastbootError::FileOpen {
        path: PathBuf::from(&file_to_upload),
        source: e,
    })?;
    let file_size = file_handle
        .metadata()
        .map_err(|e| FfxFastbootError::FileMetadata {
            path: PathBuf::from(&file_to_upload),
            source: e,
        })?
        .len();

    // Calculate the flashing timeout
    let timeout =
        FlashTimeouts::new(min_timeout_secs, flash_timeout_rate_mb_per_second).for_bytes(file_size);
    let megabytes = (file_size as f64) / 1_000_000.0;
    log::debug!("Estimated timeout: {}s for {}MB", timeout, megabytes);

    // Get as a u32 because of fastboot protocol requirements.
    let max_download_size: u64 =
        get_hex_int::<u32>(MAX_DOWNLOAD_SIZE_VAR, fastboot_interface).await?.into();
    log::info!(
        "Fastboot device max-download-size: {} bytes ({:.2} MiB)",
        max_download_size,
        max_download_size as f64 / (1024.0 * 1024.0)
    );
    log::trace!("Override Max Download Size: {:?}", override_max_download_size);
    log::trace!("File size: {}", file_size);

    let max_download_size: u64 = match override_max_download_size {
        Some(omds) => omds.into(),
        None => max_download_size,
    };

    let gbl_async_enabled = if manage_gbl_async {
        try_enable_gbl_async_tasks(fastboot_interface).await?
    } else {
        false
    };
    let start_time = Utc::now();

    let flash_res: Result<()> = async {
        if max_download_size < file_size {
            // Next check if the file given is ALREADY in the sparse image format
            match SparseReader::is_sparse_file(&mut file_handle) {
                Ok(true) => {
                    log::debug!(
                        "Image is too big to fit into target RAM and is a sparse image. Re-sparsing"
                    );

                    log::debug!("Is already a sparse file. Building Reader");
                    let mut reader = SparseReader::new(file_handle)?;
                    log::debug!("Building sparse image");
                    let sparse_writers =
                        resparse_sparse_img_writers(&mut reader, max_download_size)?;

                    messenger
                        .send(Event::Upload(UploadProgress::OnReady {
                            partition: name.to_owned(),
                            files: sparse_writers.len().try_into()?,
                        }))
                        .await?;

                    for writer in sparse_writers {
                        let size = u32::try_from(writer.file_size())?;
                        let mut slice_reader = writer.slice_reader(&mut reader)?;
                        do_flash_dynamic_sparse(
                            name,
                            messenger,
                            fastboot_interface,
                            size,
                            &mut slice_reader,
                            timeout,
                        )
                        .await?;
                    }
                }
                Err(_) | Ok(false) => {
                    log::debug!("Image is too big to fit into target RAM; flashing in sparse mode");
                    flash_partition_sparse(
                        name,
                        &messenger,
                        &file_to_upload,
                        fastboot_interface,
                        max_download_size,
                        timeout,
                    )
                    .await?;
                }
            }
        } else {
            messenger
                .send(Event::Upload(UploadProgress::OnReady {
                    partition: name.to_owned(),
                    files: 1,
                }))
                .await?;
            let size = u32::try_from(file_size)?;
            do_flash_dynamic_sparse(
                name,
                messenger,
                fastboot_interface,
                size,
                &mut file_handle,
                timeout,
            )
            .await?;
        }
        Ok(())
    }
    .await;

    if gbl_async_enabled {
        finish_gbl_async_tasks(fastboot_interface, timeout, flash_res).await?;
    } else {
        flash_res?;
    }

    messenger
        .send(Event::FlashPartitionFinished {
            partition_name: name.to_string(),
            duration: Utc::now().signed_duration_since(start_time),
        })
        .await?;
    Ok(())
}

trait FromHexStr: Sized {
    fn from_hex_str(val: &str) -> std::result::Result<Self, ParseIntError>;
}

macro_rules! from_hex_str_impl {
    ($int_type:ty) => {
        impl FromHexStr for $int_type {
            fn from_hex_str(val: &str) -> std::result::Result<Self, ParseIntError> {
                let stripped =
                    val.strip_prefix("0x").or_else(|| val.strip_prefix("0X")).unwrap_or(val);
                Self::from_str_radix(stripped, 16)
            }
        }
    };
}

from_hex_str_impl!(u32);
from_hex_str_impl!(u64);

async fn get_hex_int<T: FromHexStr>(
    variable: &str,
    interface: &mut impl FastbootInterface,
) -> std::result::Result<T, FfxFastbootError> {
    let value = interface.get_var(variable).await?;
    T::from_hex_str(value.as_str())
        .inspect_err(|_| {
            log::error!(
                "Fastboot {variable} var was not a valid {}: {value}",
                std::any::type_name::<T>()
            )
        })
        .map_err(|e| e.into())
}

fn streaming_err_helper(message: String) -> FfxFastbootError {
    // TODO(b/563409803) Add more helpful fields or discriminants to
    // FfxFastbootError::StreamingFlash
    FfxFastbootError::StreamingFlash { message }
}

/// Pre-flight error while preparing a streaming flash iterator.
#[derive(Debug)]
enum StreamInitError {
    /// Target or image parameters cannot use the streaming protocol; single-partition flash may
    /// safely fall back to `flash_basic_impl` because nothing has been sent yet.
    Ineligible(String),
    /// Actionable failure (missing file, corrupt sparse image, image larger than partition) that
    /// must be propagated directly rather than retried via classical flash.
    Fatal(Box<FfxFastbootError>),
}

const _: () = assert!(std::mem::size_of::<StreamInitError>() <= 32);

impl StreamInitError {
    fn fatal(err: impl Into<FfxFastbootError>) -> Self {
        Self::Fatal(Box::new(err.into()))
    }
}

impl From<StreamInitError> for FfxFastbootError {
    fn from(err: StreamInitError) -> Self {
        match err {
            StreamInitError::Ineligible(msg) => streaming_err_helper(msg),
            StreamInitError::Fatal(e) => *e,
        }
    }
}

/// Commands that stream a partition image. Reading the image may fail.
type StreamCommands = Box<dyn Iterator<Item = std::io::Result<StreamCommand>> + Send>;

fn create_streaming_iter(
    filepath: &str,
    max_download_bytes: u64,
    segment_size_bytes: u64,
    partition_start_byte: u64,
    partition_size_bytes: u64,
) -> std::result::Result<(StreamCommands, u64), StreamInitError> {
    log::info!(
        "Fastboot streaming download segment limit: {} bytes ({:.2} MiB)",
        max_download_bytes,
        max_download_bytes as f64 / (1024.0 * 1024.0)
    );

    if max_download_bytes > u64::from(u32::MAX) || max_download_bytes % U32_SIZE != 0 {
        return Err(StreamInitError::Ineligible(format!(
            "Bad max download size: {max_download_bytes}"
        )));
    }

    if segment_size_bytes == 0 || segment_size_bytes % U32_SIZE != 0 {
        return Err(StreamInitError::Ineligible(format!(
            "Bad streaming segment size: {segment_size_bytes}"
        )));
    }

    if segment_size_bytes > max_download_bytes {
        return Err(StreamInitError::Ineligible(format!(
            "Max download < segment size: {max_download_bytes} < {segment_size_bytes}"
        )));
    }

    // Splits large data into whole segments, so that each command starts on a segment boundary,
    // e.g. a block of the device's storage, if the partition does.
    let max_download_bytes = max_download_bytes - max_download_bytes % segment_size_bytes;

    if partition_start_byte % U32_SIZE != 0 {
        return Err(StreamInitError::Ineligible(format!(
            "Bad partition start: {partition_start_byte}"
        )));
    }

    let file_handle = File::open(&filepath).map_err(|e| {
        StreamInitError::fatal(FfxFastbootError::FileOpen {
            path: PathBuf::from(&filepath),
            source: e,
        })
    })?;
    let expected = file_handle
        .metadata()
        .map_err(|e| {
            StreamInitError::fatal(FfxFastbootError::FileMetadata {
                path: PathBuf::from(&filepath),
                source: e,
            })
        })?
        .len();

    let mut reader = BufReader::new(file_handle);
    let is_sparse = match SparseReader::is_sparse_file(&mut reader) {
        Ok(is_sparse) => is_sparse,
        Err(_) => {
            reader.seek(SeekFrom::Start(0)).map_err(StreamInitError::fatal)?;
            false
        }
    };

    let (commands, expanded_file_size): (StreamCommands, u64) = if is_sparse {
        let sparse_reader = SparseReader::new(reader).map_err(StreamInitError::fatal)?;
        let iter = SparseStreamIterator::new(max_download_bytes, sparse_reader);
        let expanded_size = iter.get_expanded_size();

        (Box::new(iter), expanded_size)
    } else {
        if expected % U32_SIZE != 0 {
            log::debug!("Un-streamable file size: {expected}");
            return Err(StreamInitError::Ineligible(format!(
                "File size is not a multiple of {U32_SIZE}: {expected}"
            )));
        }
        let iter = RawStreamIterator::new(
            reader,
            expected,
            max_download_bytes,
            segment_size_bytes,
            partition_start_byte,
        )
        .map_err(StreamInitError::fatal)?;

        (Box::new(iter), expected)
    };

    // Must complete check host side because device never knows the full image size.
    if partition_size_bytes < expanded_file_size {
        return Err(StreamInitError::fatal(streaming_err_helper(format!(
            "Tried to stream a {expanded_file_size} byte image to a {partition_size_bytes} byte partition"
        ))));
    }

    Ok((commands, expanded_file_size))
}

struct StreamerTask<'a> {
    partition_name: &'a str,
    init_rx: Option<oneshot::Receiver<std::result::Result<u64, StreamInitError>>>,
    expanded_file_size: Option<u64>,
    cmd_rx: Receiver<std::io::Result<StreamCommand>>,
    handle: tokio::task::JoinHandle<()>,
}

impl<'a> StreamerTask<'a> {
    fn spawn(
        partition_name: &'a str,
        filepath: String,
        max_download_bytes: u64,
        segment_size_bytes: u64,
        partition_start_byte: u64,
        partition_size_bytes: u64,
    ) -> Self {
        let (init_tx, init_rx) = oneshot::channel();
        // Capacity 1 keeps one pre-hashed command ready while the active command uploads over the
        // wire; reserving the slot before advancing `commands` bounds host RAM to at most two
        // `max_download_bytes` buffers instead of five.
        let (cmd_tx, cmd_rx) = mpsc::channel(1);
        let handle = spawn_blocking(move || {
            let (mut commands, expanded_file_size) = match create_streaming_iter(
                &filepath,
                max_download_bytes,
                segment_size_bytes,
                partition_start_byte,
                partition_size_bytes,
            ) {
                Ok(streamer) => streamer,
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };

            if init_tx.send(Ok(expanded_file_size)).is_err() {
                return;
            }
            while let Ok(permit) = futures::executor::block_on(cmd_tx.reserve()) {
                let Some(cmd) = commands.next() else {
                    break;
                };
                let failed = cmd.is_err();
                permit.send(cmd);
                if failed {
                    break;
                }
            }
        });

        Self { partition_name, init_rx: Some(init_rx), expanded_file_size: None, cmd_rx, handle }
    }

    async fn wait_initialized(&mut self) -> std::result::Result<u64, StreamInitError> {
        if let Some(size) = self.expanded_file_size {
            return Ok(size);
        }
        let size = self
            .init_rx
            .take()
            .ok_or_else(|| {
                StreamInitError::fatal(streaming_err_helper(
                    "StreamerTask already initialized".to_string(),
                ))
            })?
            .await
            .map_err(|e| {
                StreamInitError::fatal(streaming_err_helper(format!(
                    "Streamer init task failed: {e}"
                )))
            })??;
        self.expanded_file_size = Some(size);
        Ok(size)
    }
}

async fn stream_partition_task<'a, T: FastbootInterface>(
    messenger: &Sender<Event>,
    mut streamer_task: StreamerTask<'a>,
    fastboot_interface: &mut T,
    timeouts: FlashTimeouts,
    manage_gbl_async: bool,
) -> Result<u64> {
    let expanded_file_size = streamer_task.wait_initialized().await?;
    let StreamerTask { partition_name, cmd_rx, handle, .. } = streamer_task;
    let gbl_async_enabled = if manage_gbl_async {
        try_enable_gbl_async_tasks(fastboot_interface).await?
    } else {
        false
    };

    let (prog_client, prog_server) = mpsc::channel(3);
    let server_task = async |mut prog_server: Receiver<UploadProgress>| -> Result<()> {
        while let Some(upload) = prog_server.recv().await {
            messenger.send(Event::Upload(upload)).await?;
        }
        Ok(())
    };

    let producer_task = async move {
        handle.await.map_err(|e| streaming_err_helper(format!("Producer task failed: {e}")))
    };

    let mut stream_task = async |prog_client: Sender<UploadProgress>,
                                 mut cmd_server: Receiver<std::io::Result<StreamCommand>>|
           -> Result<()> {
        prog_client
            .send(UploadProgress::OnStarted { size: expanded_file_size })
            .await
            .map_err(|e| FfxFastbootError::StreamingFlash { message: e.to_string() })?;
        while let Some(command) = cmd_server.recv().await {
            let command = command.map_err(|e| {
                std::io::Error::new(e.kind(), format!("reading the {partition_name} image: {e}"))
            })?;
            // Large commands, e.g. long fills, take a while to write.
            let timeout = timeouts.for_bytes(command.length_bytes());
            fastboot_interface.stream(partition_name, command, &prog_client, timeout).await?;
        }
        prog_client
            .send(UploadProgress::OnFinished)
            .await
            .map_err(|e| FfxFastbootError::StreamingFlash { message: e.to_string() })?;
        Ok(())
    };

    // We don't want to yoke the stream task and server task too tightly
    // since it's possible for the server task to be slow and to gate
    // progress for the stream task. Unlikely, but it's still better to bundle up all
    // the streaming commands into a single task and then log that progress on a best
    // effort basis.
    messenger
        .send(Event::Upload(UploadProgress::OnReady {
            partition: partition_name.to_owned(),
            files: 1,
        }))
        .await?;

    let start_time = Utc::now();
    let (stream_res, server_res) = futures::join!(
        async { try_join!(producer_task, stream_task(prog_client, cmd_rx)) },
        server_task(prog_server),
    );
    let flash_res = stream_res.map(|_| ()).and(server_res);
    if gbl_async_enabled {
        finish_gbl_async_tasks(
            fastboot_interface,
            timeouts.for_bytes(expanded_file_size),
            flash_res,
        )
        .await?;
    } else {
        flash_res?;
    }
    let duration = Utc::now().signed_duration_since(start_time);
    messenger
        .send(Event::FlashPartitionFinished { partition_name: partition_name.to_owned(), duration })
        .await?;

    Ok(expanded_file_size)
}

/// Timeouts of flash operations: at least `min_secs`, and longer for large writes, which are
/// expected to proceed at `rate_mb_per_second` or faster.
#[derive(Clone, Copy, Debug, PartialEq)]
struct FlashTimeouts {
    min_secs: u64,
    rate_mb_per_second: f64,
}

impl FlashTimeouts {
    /// Upper bound of the timeouts, which keeps them representable.
    const MAX_SECS: u64 = 24 * 60 * 60;

    fn new(min_secs: u64, rate_mb_per_second: f64) -> Self {
        Self { min_secs, rate_mb_per_second }
    }

    /// Returns the timeout of an operation that writes `bytes` bytes.
    fn for_bytes(&self, bytes: u64) -> Duration {
        let megabytes = bytes as f64 / 1_000_000.0;
        // Float to integer casts saturate, and map NaN to 0.
        let secs = match self.rate_mb_per_second > 0.0 {
            true => (megabytes / self.rate_mb_per_second) as u64,
            false => 0,
        };
        let secs = secs.max(self.min_secs).min(Self::MAX_SECS);
        // `secs` is at most `MAX_SECS`, so it fits in an i64.
        Duration::seconds(secs as i64)
    }
}

fn parameterized_var(base: &str, parameter: &str) -> String {
    format!("{}:{}", base, parameter)
}

async fn is_gbl_fastboot(fastboot_interface: &mut impl FastbootInterface) -> Result<bool> {
    match fastboot_interface.get_var(VERSION_GBL_VAR).await {
        Ok(ver) => Ok(!ver.trim().is_empty()),
        Err(FastbootError::GetVariableError { .. }) => Ok(false),
        Err(err) => Err(err.into()),
    }
}

/// Enables GBL background storage tasks when flashing a target over a supported transport (TCP)
/// that exposes `version-gbl`.
///
/// Returns `Ok(true)` if async tasks were enabled and must be synced on completion, or `Ok(false)`
/// when the transport does not use automatic GBL async tasks, the target is not GBL, or the target
/// rejects `oem gbl-enable-async-task`.
async fn try_enable_gbl_async_tasks<T: FastbootInterface>(
    fastboot_interface: &mut T,
) -> Result<bool> {
    if !fastboot_interface.supports_gbl_async_tasks() {
        return Ok(false);
    }
    if !is_gbl_fastboot(fastboot_interface).await? {
        return Ok(false);
    }
    match fastboot_interface.oem(GBL_ENABLE_ASYNC_TASK_COMMAND).await {
        Ok(()) => {
            log::debug!("Enabled GBL fastboot async tasks");
            Ok(true)
        }
        Err(FastbootError::OemCommandFailed { message, .. }) => {
            log::debug!(
                "GBL target rejected `oem {GBL_ENABLE_ASYNC_TASK_COMMAND}` ({message}); \
                 continuing synchronously"
            );
            Ok(false)
        }
        Err(err) => Err(err.into()),
    }
}

/// Returns whether the fastboot transport remains connected and synchronized after `err`, making
/// it safe to drain any in-flight GBL background tasks via `oem gbl-sync-tasks`.
fn can_sync_gbl_tasks_after_error(err: &FfxFastbootError) -> bool {
    match err {
        FfxFastbootError::Interface(fb_err) => matches!(
            fb_err,
            FastbootError::GetVariableError { .. }
                | FastbootError::OemCommandFailed { .. }
                | FastbootError::SetActiveFailed { .. }
                | FastbootError::ErasePartitionFailed { .. }
                | FastbootError::DownloadFailed { .. }
                | FastbootError::BootFailed(_)
                | FastbootError::RebootFailed(_)
                | FastbootError::ContinueBootFailed(_)
                | FastbootError::RebootBootloaderFailed { .. }
                | FastbootError::GetAllVarsFailed(_)
                | FastbootError::StageError(
                    StageError::UploadFailed { .. } | StageError::InvalidFileSize(_)
                )
                | FastbootError::FlashError(
                    FlashError::FlashFailed { .. }
                        | FlashError::StreamFailed { .. }
                        | FlashError::InvalidFileSize(_)
                        | FlashError::UploadFailed(
                            StageError::UploadFailed { .. } | StageError::InvalidFileSize(_)
                        )
                )
        ),
        _ => true,
    }
}

async fn finish_gbl_async_tasks<T: FastbootInterface>(
    fastboot_interface: &mut T,
    sync_timeout: Duration,
    flash_res: Result<()>,
) -> Result<()> {
    if let Err(ref err) = flash_res
        && !can_sync_gbl_tasks_after_error(err)
    {
        return flash_res;
    }
    match fastboot_interface.oem_with_timeout(GBL_SYNC_TASKS_COMMAND, sync_timeout).await {
        Ok(()) => {
            if let Err(err) = fastboot_interface.oem(GBL_DISABLE_ASYNC_TASK_COMMAND).await {
                log::warn!(
                    "Failed to disable GBL async tasks after sync (`oem {GBL_DISABLE_ASYNC_TASK_COMMAND}`): {err}"
                );
            }
            flash_res
        }
        Err(sync_err) => {
            if flash_res.is_err() {
                log::warn!(
                    "Failed to sync GBL async tasks after flash error (`oem {GBL_SYNC_TASKS_COMMAND}`): {sync_err}"
                );
                flash_res
            } else {
                Err(sync_err.into())
            }
        }
    }
}

async fn probe_streaming_var<T: FromHexStr>(
    variable: &str,
    interface: &mut impl FastbootInterface,
) -> Result<Option<T>> {
    match get_hex_int::<T>(variable, interface).await {
        Ok(val) => Ok(Some(val)),
        Err(
            FfxFastbootError::Interface(FastbootError::GetVariableError { .. })
            | FfxFastbootError::IntegerParse(_),
        ) => Ok(None),
        Err(err) => Err(err),
    }
}

pub async fn flash_partition_impl<T: FastbootInterface>(
    messenger: Sender<Event>,
    name: &str,
    file_to_upload: &str,
    fb_intf: &mut T,
    min_timeout_secs: u64,
    flash_timeout_rate_mb_per_second: f64,
    override_max_download_size: Option<NonZeroU64>,
    streaming_flash: bool,
) -> Result<()> {
    if streaming_flash
        && let Some(segment_size) = probe_streaming_var::<u32>(STREAM_SEGMENT_SIZE, fb_intf).await?
        && let Some(partition_start) =
            probe_streaming_var::<u64>(parameterized_var(PARTITION_START, name).as_str(), fb_intf)
                .await?
        && let Some(partition_size) =
            probe_streaming_var::<u64>(parameterized_var(PARTITION_SIZE, name).as_str(), fb_intf)
                .await?
    {
        let max_download_bytes: u64 = match override_max_download_size {
            Some(omds) => omds.into(),
            None => get_hex_int::<u32>(MAX_DOWNLOAD_SIZE_VAR, fb_intf).await?.into(),
        };
        if let Some(max_download_bytes) = NonZeroU64::new(max_download_bytes) {
            let mut streamer_task = StreamerTask::spawn(
                name,
                file_to_upload.to_owned(),
                max_download_bytes.into(),
                segment_size.into(),
                partition_start,
                partition_size,
            );
            match streamer_task.wait_initialized().await {
                Ok(_) => {
                    let timeouts =
                        FlashTimeouts::new(min_timeout_secs, flash_timeout_rate_mb_per_second);
                    return stream_partition_task(
                        &messenger,
                        streamer_task,
                        fb_intf,
                        timeouts,
                        true,
                    )
                    .await
                    .map(|_| ());
                }
                Err(StreamInitError::Ineligible(reason)) => {
                    let _ = streamer_task.handle.await;
                    log::info!(
                        "Partition {name} ineligible for streaming flash ({reason}); falling back to standard flash"
                    );
                }
                Err(StreamInitError::Fatal(err)) => {
                    let _ = streamer_task.handle.await;
                    return Err(*err);
                }
            }
        }
    }

    flash_basic_impl(
        &messenger,
        name,
        file_to_upload,
        fb_intf,
        min_timeout_secs,
        flash_timeout_rate_mb_per_second,
        override_max_download_size,
        true,
    )
    .await
}

pub async fn verify_hardware(
    revision: &String,
    product_matches: &[String],
    fastboot_interface: &mut impl FastbootInterface,
) -> Result<()> {
    let rev = fastboot_interface.get_var(REVISION_VAR).await?;
    if let Some(r) = rev.split("-").next() {
        if r == *revision || rev == *revision {
            return Ok(());
        }
    }

    let mut found_product = None;
    if !product_matches.is_empty() {
        match fastboot_interface.get_var(PRODUCT_VAR).await {
            Ok(product) => {
                // Any match of the given set of products means success.
                if product_matches.contains(&product) {
                    return Ok(());
                }
                found_product = Some(product);
            }
            Err(e) => {
                log::warn!("Failed to get product variable from device: {e}");
            }
        }
    }

    return Err(FfxFastbootError::HardwareMismatch {
        expected: revision.clone(),
        found: rev,
        attempted_products: product_matches.to_vec(),
        found_product,
    });
}

pub async fn verify_variable_value(
    var: &str,
    value: &str,
    fastboot_interface: &mut impl FastbootInterface,
) -> Result<bool> {
    log::debug!("Verifying value for variable {} equals {}", var, value);
    Ok(fastboot_interface.get_var(var).await.map(|res| res == value)?)
}

pub async fn reboot_bootloader<F: FastbootInterface>(
    messenger: &Sender<Event>,
    fastboot_interface: &mut F,
) -> Result<()> {
    messenger.send(Event::RebootStarted).await?;
    let (reboot_client, mut reboot_server): (Sender<RebootEvent>, Receiver<RebootEvent>) =
        mpsc::channel(1);
    let start_time = Utc::now();
    try_join!(
        fastboot_interface.reboot_bootloader(reboot_client).map_err(FfxFastbootError::Interface),
        async move {
            match reboot_server.recv().await {
                Some(RebootEvent::OnReboot) => {
                    return Ok(());
                }
                None => {
                    return Err(FfxFastbootError::RebootSignalMissing);
                }
            };
        }
    )?;

    let d = Utc::now().signed_duration_since(start_time);
    log::debug!("Reboot duration: {:.2}s", (d.num_milliseconds() / 1000));
    messenger.send(Event::Rebooted(d)).await?;
    Ok(())
}

/// Uploads a file using the given [UploadMethod].
pub async fn upload_file<F: FileResolver + Sync, T: FastbootInterface>(
    messenger: &Sender<Event>,
    file_resolver: &mut F,
    resolve: bool,
    file: &str,
    method: &UploadMethod,
    fastboot_interface: &mut T,
) -> Result<()> {
    let file_path = match resolve {
        true => file_resolver.get_file(file).await?,
        false => file.to_string(),
    };

    match method {
        // Staged: stage the data then issue the command.
        UploadMethod::Staged { command } => {
            let (client, mut server) = mpsc::channel(1);
            try_join!(
                async {
                    fastboot_interface
                        .stage(&file_path, client)
                        .await
                        .map_err(FfxFastbootError::Interface)
                },
                async {
                    loop {
                        match server.recv().await {
                            Some(e) => {
                                messenger.send(Event::Upload(e)).await?;
                            }
                            None => {
                                return Ok(());
                            }
                        }
                    }
                }
            )?;

            messenger.send(Event::Oem { oem_command: command.clone() }).await?;
            fastboot_interface.oem(command).await?;
        }
        // Inline: chunk the data into consecutive OEM commands.
        UploadMethod::Inline {
            command_prefix,
            command_max_length,
            init_command,
            finalize_command,
        } => {
            let file_content = std::fs::read(&file_path).map_err(|e| {
                FfxFastbootError::FileOpen { path: PathBuf::from(&file_path), source: e }
            })?;
            let encoded = BASE64_STANDARD.encode(file_content);

            // Determine how much data we can fit in each command.
            let data_len =
                method.command_data_length(fastboot::MAX_COMMAND_LENGTH).map_err(|_| {
                    FfxFastbootError::InlineUploadOverflow {
                        prefix: command_prefix.clone(),
                        max_len: *command_max_length,
                    }
                })?;

            // If the device needs an initialization command, send it first.
            if let Some(init_cmd) = init_command {
                messenger.send(Event::Oem { oem_command: init_cmd.clone() }).await?;
                fastboot_interface.oem(init_cmd).await?;
            }

            for chunk in encoded.as_bytes().chunks(data_len) {
                // `unwrap()` will always succeed here because Base64 only uses single-byte data,
                // so no matter where the chunk splits the result is always valid UTF-8.
                let chunk_str = std::str::from_utf8(chunk).unwrap();
                let cmd = format!("{}{}", command_prefix, chunk_str);
                messenger.send(Event::Oem { oem_command: cmd.clone() }).await?;
                fastboot_interface.oem(&cmd).await?;
            }

            // If the device needs a finalization command, send it at the end.
            if let Some(finalize_command) = finalize_command {
                messenger.send(Event::Oem { oem_command: finalize_command.clone() }).await?;
                fastboot_interface.oem(finalize_command).await?;
            }
        }
    }
    Ok(())
}

pub async fn stage_oem_files<F: FileResolver + Sync, T: FastbootInterface>(
    messenger: &Sender<Event>,
    file_resolver: &mut F,
    resolve: bool,
    oem_files: &Vec<OemFile>,
    fastboot_interface: &mut T,
) -> Result<()> {
    for oem_file in oem_files {
        upload_file(
            messenger,
            file_resolver,
            resolve,
            oem_file.file(),
            &UploadMethod::Staged { command: oem_file.command().to_string() },
            fastboot_interface,
        )
        .await?;
    }
    Ok(())
}

pub async fn set_slot_a_active(fastboot_interface: &mut impl FastbootInterface) -> Result<()> {
    if fastboot_interface.erase("misc").await.is_err() {
        log::debug!("Could not erase misc partition");
    }
    fastboot_interface.set_active("a").await?;
    Ok(())
}

pub async fn flash_partitions<
    'a,
    F: FileResolver + Sync,
    P: Partition + 'a,
    T: FastbootInterface,
>(
    messenger: &Sender<Event>,
    file_resolver: &mut F,
    partitions: impl IntoIterator<Item = &'a P>,
    fastboot_interface: &mut T,
    min_timeout_secs: u64,
    flash_timeout_rate_mb_per_second: f64,
    streaming_flash: bool,
) -> Result<()> {
    if streaming_flash
        && let Ok(segment_size) =
            get_hex_int::<u32>(STREAM_SEGMENT_SIZE, fastboot_interface).await.map(u64::from)
    {
        flash_partitions_stream(
            messenger,
            file_resolver,
            partitions,
            fastboot_interface,
            segment_size,
            min_timeout_secs,
            flash_timeout_rate_mb_per_second,
        )
        .await
    } else {
        let resolved_partitions =
            resolve_partitions(partitions, file_resolver, fastboot_interface).await?;
        if resolved_partitions.is_empty() {
            return Ok(());
        }

        let timeouts = FlashTimeouts::new(min_timeout_secs, flash_timeout_rate_mb_per_second);
        let gbl_async_enabled = try_enable_gbl_async_tasks(fastboot_interface).await?;
        let mut attempted_bytes = 0u64;
        let mut flash_res = Ok(());

        // Flash all pre-resolved partitions
        for (name, file_to_upload) in resolved_partitions {
            let file_bytes = std::fs::metadata(&file_to_upload).map(|m| m.len()).unwrap_or(0);
            attempted_bytes = attempted_bytes.saturating_add(file_bytes);
            if let Err(err) = flash_basic_impl(
                messenger,
                name,
                &file_to_upload,
                fastboot_interface,
                min_timeout_secs,
                flash_timeout_rate_mb_per_second,
                None,
                false,
            )
            .await
            {
                flash_res = Err(err);
                break;
            }
        }

        if gbl_async_enabled {
            finish_gbl_async_tasks(
                fastboot_interface,
                timeouts.for_bytes(attempted_bytes),
                flash_res,
            )
            .await
        } else {
            flash_res
        }
    }
}

async fn resolve_partitions<'a, F: FileResolver, P: Partition + 'a, T: FastbootInterface>(
    partitions: impl IntoIterator<Item = &'a P>,
    file_resolver: &mut F,
    fastboot_interface: &mut T,
) -> Result<Vec<(&'a str, String)>> {
    // Pre-evaluate conditions and pre-resolve all partition files up-front
    let mut resolved_partitions = Vec::new();
    for partition in partitions {
        let should_flash = match (partition.variable(), partition.variable_value()) {
            (Some(var), Some(value)) => {
                verify_variable_value(var, value, fastboot_interface).await?
            }
            _ => true,
        };

        if should_flash {
            let file_to_upload = file_resolver.get_file(partition.file()).await?;
            resolved_partitions.push((partition.name(), file_to_upload));
        }
    }
    Ok(resolved_partitions)
}

pub async fn flash_partitions_stream<
    'a,
    F: FileResolver + Sync,
    P: Partition + 'a,
    T: FastbootInterface,
>(
    messenger: &Sender<Event>,
    file_resolver: &mut F,
    partitions: impl IntoIterator<Item = &'a P>,
    fastboot_interface: &mut T,
    stream_segment_size: u64,
    min_timeout_secs: u64,
    flash_timeout_rate_mb_per_second: f64,
) -> Result<()> {
    let resolved_partitions =
        resolve_partitions(partitions, file_resolver, fastboot_interface).await?;
    if resolved_partitions.is_empty() {
        return Ok(());
    }

    let max_download_bytes: u64 = NonZeroU64::new(
        get_hex_int::<u32>(MAX_DOWNLOAD_SIZE_VAR, fastboot_interface).await?.into(),
    )
    .ok_or_else(|| streaming_err_helper(format!("Max download size of 0")))?
    .into();
    let timeouts = FlashTimeouts::new(min_timeout_secs, flash_timeout_rate_mb_per_second);

    let mut partition_ranges = Vec::new();
    for (name, file_to_upload) in resolved_partitions {
        let partition_size = get_hex_int::<u64>(
            parameterized_var(PARTITION_SIZE, name).as_str(),
            fastboot_interface,
        )
        .await?;
        let partition_start = get_hex_int::<u64>(
            parameterized_var(PARTITION_START, name).as_str(),
            fastboot_interface,
        )
        .await?;
        partition_ranges.push((name, file_to_upload, partition_start, partition_size));
    }

    let gbl_async_enabled = try_enable_gbl_async_tasks(fastboot_interface).await?;
    let mut attempted_bytes = 0u64;
    let mut flash_res = Ok(());

    let mut streamers = partition_ranges.into_iter().map(
        |(name, file_to_upload, partition_start, partition_size)| {
            StreamerTask::spawn(
                name,
                file_to_upload,
                max_download_bytes,
                stream_segment_size,
                partition_start,
                partition_size,
            )
        },
    );

    // Prepares the commands of the next partition while the current one streams, but no further
    // ahead, which bounds memory use.
    let mut next_streamer = streamers.next();
    while let Some(mut streamer_task) = next_streamer {
        next_streamer = streamers.next();
        match streamer_task.wait_initialized().await {
            Ok(expanded_size) => {
                attempted_bytes = attempted_bytes.saturating_add(expanded_size);
                if let Err(err) = stream_partition_task(
                    messenger,
                    streamer_task,
                    fastboot_interface,
                    timeouts,
                    false,
                )
                .await
                {
                    flash_res = Err(err);
                    break;
                }
            }
            Err(err) => {
                let _ = streamer_task.handle.await;
                flash_res = Err(err.into());
                break;
            }
        }
    }

    if let Some(unused_streamer) = next_streamer {
        let StreamerTask { init_rx, cmd_rx, handle, .. } = unused_streamer;
        drop((init_rx, cmd_rx));
        let _ = handle.await;
    }

    if gbl_async_enabled {
        finish_gbl_async_tasks(fastboot_interface, timeouts.for_bytes(attempted_bytes), flash_res)
            .await
    } else {
        flash_res
    }
}

/// Flashes both bootloader and product partitions.
///
/// # Returns
///
/// `Ok(true)` if slot A was marked active during bootloader flashing, or `Ok(false)` otherwise.
pub async fn flash<F, Part, P, T>(
    messenger: &Sender<Event>,
    file_resolver: &mut F,
    product: &P,
    fastboot_interface: &mut T,
    cmd: ManifestParams,
    ssh_key_upload_method: Option<&UploadMethod>,
) -> Result<bool>
where
    F: FileResolver + Sync,
    Part: Partition,
    P: Product<Part>,
    T: FastbootInterface,
{
    check_skip_partitions(product, &cmd.skip_partitions)?;
    if !cmd.skip_partitions.is_empty() {
        log::info!("Skipping partitions: {:?}", cmd.skip_partitions);
    }
    messenger
        .send(Event::FlashProduct {
            product_name: product.name().clone(),
            partition_count: flashed_partition_count(product, &cmd),
        })
        .await?;
    let mut cached_fastboot = CachingFastboot::new(fastboot_interface);
    let set_active_performed =
        flash_bootloader(messenger, file_resolver, product, &mut cached_fastboot, &cmd).await?;
    flash_product(
        messenger,
        file_resolver,
        product,
        &mut cached_fastboot,
        &cmd,
        ssh_key_upload_method,
    )
    .await?;
    Ok(set_active_performed)
}

pub async fn is_userspace_fastboot(
    fastboot_interface: &mut impl FastbootInterface,
) -> Result<bool> {
    match fastboot_interface.get_var(IS_USERSPACE_VAR).await {
        Ok(rev) => Ok(rev == "yes"),
        _ => Ok(false),
    }
}

/// Returns the number of `product`'s bootloader and product partitions that `cmd` will flash.
pub fn flashed_partition_count<Part: Partition, P: Product<Part>>(
    product: &P,
    cmd: &ManifestParams,
) -> usize {
    unskipped_partitions(product.bootloader_partitions(), &cmd.skip_partitions).count()
        + unskipped_partitions(product.partitions(), &cmd.skip_partitions).count()
}

/// Flashes bootloader partitions from `product`, except those in `cmd.skip_partitions`.
///
/// # Returns
///
/// `Ok(true)` if slot A was marked active as part of bootloader flashing (which occurs when
/// bootloader partitions were flashed and the bootloader was rebooted), or `Ok(false)` otherwise.
pub async fn flash_bootloader<F, Part, P, T>(
    messenger: &Sender<Event>,
    file_resolver: &mut F,
    product: &P,
    fastboot_interface: &mut T,
    cmd: &ManifestParams,
) -> Result<bool>
where
    F: FileResolver + Sync,
    Part: Partition,
    P: Product<Part>,
    T: FastbootInterface,
{
    let partitions = unskipped_partitions(product.bootloader_partitions(), &cmd.skip_partitions);
    let flashing_any = partitions.clone().next().is_some();
    flash_partitions(
        messenger,
        file_resolver,
        partitions,
        fastboot_interface,
        cmd.flash_min_timeout_seconds,
        cmd.flash_timeout_rate_mb_per_second,
        cmd.streaming_flash,
    )
    .await?;
    if flashing_any
        && !cmd.no_bootloader_reboot
        && !is_userspace_fastboot(fastboot_interface).await?
    {
        set_slot_a_active(fastboot_interface).await?;
        reboot_bootloader(messenger, fastboot_interface).await?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub async fn flash_product<F, Part, P, T>(
    messenger: &Sender<Event>,
    file_resolver: &mut F,
    product: &P,
    fastboot_interface: &mut T,
    cmd: &ManifestParams,
    ssh_key_upload_method: Option<&UploadMethod>,
) -> Result<()>
where
    F: FileResolver + Sync,
    Part: Partition,
    P: Product<Part>,
    T: FastbootInterface,
{
    flash_partitions(
        &messenger,
        file_resolver,
        unskipped_partitions(product.partitions(), &cmd.skip_partitions),
        fastboot_interface,
        cmd.flash_min_timeout_seconds,
        cmd.flash_timeout_rate_mb_per_second,
        cmd.streaming_flash,
    )
    .await?;
    if !cmd.no_bootloader_reboot && is_userspace_fastboot(fastboot_interface).await? {
        reboot_bootloader(messenger, fastboot_interface).await?;
    }

    // Upload OEM files from the commandline.
    stage_oem_files(messenger, file_resolver, false, &cmd.oem_stage, fastboot_interface).await?;

    // Upload the SSH key OEM file.
    if let Some(ssh_key) = &cmd.ssh_key {
        // Upload the SSH keys using the mechanism indicated by the product being flashed.
        let default_method = UploadMethod::Staged { command: SSH_OEM_COMMAND.to_string() };
        let method = ssh_key_upload_method.unwrap_or(&default_method);
        upload_file(messenger, file_resolver, false, ssh_key, method, fastboot_interface).await?;
    }

    // Upload the OEM files from the product.
    stage_oem_files(messenger, file_resolver, true, product.oem_files(), fastboot_interface).await
}

pub async fn flash_and_reboot<F, Part, P, T>(
    messenger: &Sender<Event>,
    file_resolver: &mut F,
    product: &P,
    fastboot_interface: &mut T,
    cmd: ManifestParams,
    ssh_key_upload_method: Option<&UploadMethod>,
) -> Result<()>
where
    F: FileResolver + Sync,
    Part: Partition,
    P: Product<Part>,
    T: FastbootInterface,
{
    let set_active_performed =
        flash(messenger, file_resolver, product, fastboot_interface, cmd, ssh_key_upload_method)
            .await?;
    finish(fastboot_interface, set_active_performed).await
}

/// Finalizes flashing by ensuring slot A is active (if not already set during bootloader
/// flashing) and continuing boot.
pub async fn finish<F: FastbootInterface>(
    fastboot_interface: &mut F,
    set_active_performed: bool,
) -> Result<()> {
    if !set_active_performed {
        set_slot_a_active(fastboot_interface).await?;
    }
    // LINT.IfChange
    fastboot_interface.continue_boot().await.map_err(FfxFastbootError::ContinueBootFailed)?;
    // LINT.ThenChange(//tools/lib/ffxutil/flash.go)
    Ok(())
}

pub async fn is_locked(fastboot_interface: &mut impl FastbootInterface) -> Result<bool> {
    verify_variable_value(LOCKED_VAR, "no", fastboot_interface).await.map(|l| !l)
}

pub async fn lock_device(fastboot_interface: &mut impl FastbootInterface) -> Result<()> {
    fastboot_interface.oem(LOCK_COMMAND).await?;
    Ok(())
}

pub async fn from_manifest<C, F>(
    context: &EnvironmentContext,
    messenger: Sender<Event>,
    input: C,
    fastboot_interface: &mut F,
) -> Result<()>
where
    C: Into<ManifestParams>,
    F: FastbootInterface,
{
    let cmd: ManifestParams = input.into();
    match &cmd.manifest {
        Some(manifest) => {
            if !manifest.is_file() {
                return Err(FfxFastbootError::ManifestNotAFile { path: manifest.to_path_buf() });
            }
            from_path(&messenger, manifest.to_path_buf(), fastboot_interface, cmd).await
        }
        None => {
            if let Some(path) = cmd.product_bundle.as_ref().filter(|s| is_local_product_bundle(s)) {
                from_local_product_bundle(
                    &messenger,
                    PathBuf::from(&*path),
                    fastboot_interface,
                    cmd,
                )
                .await
            } else {
                let sdk = context.get_sdk()?;
                let mut path = sdk.get_path_prefix().to_path_buf();
                path.push("flash.json"); // Not actually used, placeholder value needed.
                match sdk.get_version() {
                    SdkVersion::InTree => {
                        from_in_tree(context, &messenger, fastboot_interface, cmd).await
                    }
                    SdkVersion::Version(_) => {
                        from_sdk(context, &messenger, fastboot_interface, cmd).await
                    }
                    _ => Err(FfxFastbootError::UnknownSdkType),
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::file_resolver::test::TestResolver;
    use fastboot::reply::Reply;
    use fastboot::test_transport::TestTransport;
    use ffx_fastboot_interface::fastboot_proxy::FastbootProxy;
    use ffx_fastboot_interface::interface_factory::{
        InterfaceFactory, InterfaceFactoryBase, InterfaceFactoryError,
    };
    use ffx_fastboot_interface::test::setup;
    use ffx_fastboot_interface::util::multi_chain;
    use ffx_flash_manifest::v2::FlashManifest;
    use ffx_flash_manifest::{BootParams, Command};
    use serde_json::{from_str, json};
    use sparse::builder::{DataSource, SparseImageBuilder};
    use std::io::{Seek, SeekFrom, Write};
    use tempfile::NamedTempFile;
    use tokio::sync::mpsc;

    #[derive(Default, Debug, Clone)]
    struct TestTransportFactory {}

    impl InterfaceFactoryBase<TestTransport> for TestTransportFactory {
        async fn open(&mut self) -> std::result::Result<TestTransport, InterfaceFactoryError> {
            Ok(TestTransport::new())
        }

        async fn close(&self) {}

        async fn rediscover(&mut self) -> std::result::Result<(), InterfaceFactoryError> {
            Ok(())
        }

        fn supports_gbl_async_tasks(&self) -> bool {
            true
        }
    }

    impl InterfaceFactory<TestTransport> for TestTransportFactory {}

    async fn streaming_flash_impl<T: FastbootInterface>(
        messenger: &Sender<Event>,
        name: &str,
        filepath: &str,
        fastboot_interface: &mut T,
        segment_size_bytes: u64,
        partition_start_byte: u64,
        partition_size_bytes: u64,
        timeouts: FlashTimeouts,
        override_max_download_size: Option<NonZeroU64>,
    ) -> Result<()> {
        let max_download_bytes: u64 = match override_max_download_size {
            Some(omds) => omds.into(),
            None => NonZeroU64::new(
                get_hex_int::<u32>(MAX_DOWNLOAD_SIZE_VAR, fastboot_interface).await?.into(),
            )
            .ok_or_else(|| streaming_err_helper(format!("Max download size of 0")))?
            .into(),
        };

        let streamer_task = StreamerTask::spawn(
            name,
            filepath.to_owned(),
            max_download_bytes,
            segment_size_bytes,
            partition_start_byte,
            partition_size_bytes,
        );
        stream_partition_task(messenger, streamer_task, fastboot_interface, timeouts, false)
            .await
            .map(|_| ())
    }

    async fn flash_partition<F: FileResolver + Sync, T: FastbootInterface>(
        messenger: Sender<Event>,
        file_resolver: &mut F,
        name: &str,
        file: &str,
        fastboot_interface: &mut T,
        min_timeout_secs: u64,
        flash_timeout_rate_mb_per_second: f64,
    ) -> Result<()> {
        let file_to_upload = file_resolver.get_file(file).await?;
        log::debug!("Preparing to upload {}", file_to_upload);
        flash_partition_impl(
            messenger,
            name,
            &file_to_upload,
            fastboot_interface,
            min_timeout_secs,
            flash_timeout_rate_mb_per_second,
            None,
            true,
        )
        .await
    }

    /// Runs a fastboot sequence of uploading a file via inline upload.
    ///
    /// # Arguments
    ///
    /// * `data`: the data to upload
    /// * `method`: [UploadMethod] parameters
    ///
    /// # Returns
    ///
    /// The resulting list of commands issued to the device, or `Err` if [upload_file] failed.
    async fn run_upload_file(data: &[u8], method: UploadMethod) -> Result<Vec<String>> {
        let (state, mut mock) = setup();

        let data_file = NamedTempFile::new().unwrap();
        std::fs::write(data_file.path(), data).unwrap();

        let (messenger, _receiver) = mpsc::channel(100);

        upload_file(
            &messenger,
            &mut TestResolver::new(),
            false,
            &data_file.path().to_string_lossy(),
            &method,
            &mut mock,
        )
        .await?;

        let state = state.lock().unwrap();
        Ok(state.oem_commands.clone())
    }

    #[fuchsia::test]
    async fn upload_inline() {
        let cmds = run_upload_file(
            b"foo bar baz",
            UploadMethod::Inline {
                command_prefix: "write-chunk ".to_string(),
                command_max_length: 24,
                init_command: Some("init-write".to_string()),
                finalize_command: Some("done-write".to_string()),
            },
        )
        .await
        .unwrap();

        // Base64("foo bar baz") == "Zm9vIGJhciBiYXo=" (16 bytes)
        // "oem write-chunk " command prefix is 16 bytes.
        // 24 max length - 16 prefix = 8 data per command, so we fill exactly 2 commands.
        assert_eq!(
            cmds,
            [
                "oem init-write",
                "oem write-chunk Zm9vIGJh",
                "oem write-chunk ciBiYXo=",
                "oem done-write"
            ]
        );
    }

    #[fuchsia::test]
    async fn upload_inline_partial_command() {
        let cmds = run_upload_file(
            b"foo bar",
            UploadMethod::Inline {
                command_prefix: "write-chunk ".to_string(),
                command_max_length: 24,
                init_command: Some("init-write".to_string()),
                finalize_command: Some("done-write".to_string()),
            },
        )
        .await
        .unwrap();

        // Base64("foo bar") == "Zm9vIGJhcg==" (12 bytes)
        // "oem write-chunk " command prefix is 16 bytes.
        // 24 max length - 16 prefix = 8 data per command, so we send 1 full and 1 partial command.
        assert_eq!(
            cmds,
            [
                "oem init-write",
                "oem write-chunk Zm9vIGJh",
                "oem write-chunk cg==",
                "oem done-write"
            ]
        );
    }

    #[fuchsia::test]
    async fn upload_inline_no_init_or_finalize() {
        let cmds = run_upload_file(
            b"foo bar baz",
            UploadMethod::Inline {
                command_prefix: "write-chunk ".to_string(),
                command_max_length: 24,
                init_command: None,
                finalize_command: None,
            },
        )
        .await
        .unwrap();

        // Base64("foo bar baz") == "Zm9vIGJhciBiYXo=" (16 bytes)
        // "oem write-chunk " command prefix is 16 bytes.
        // 24 max length - 16 prefix = 8 data per command, so we fill exactly 2 commands.
        assert_eq!(cmds, ["oem write-chunk Zm9vIGJh", "oem write-chunk ciBiYXo=",]);
    }

    #[fuchsia::test]
    async fn upload_inline_single_chunk() {
        let cmds = run_upload_file(
            b"1234\nABCD",
            UploadMethod::Inline {
                command_prefix: "write-chunk ".to_string(),
                command_max_length: 64,
                init_command: Some("init-write".to_string()),
                finalize_command: None,
            },
        )
        .await
        .unwrap();

        // Base64("1234\nABCD") == "MTIzNApBQkNE"
        assert_eq!(cmds, ["oem init-write", "oem write-chunk MTIzNApBQkNE"]);
    }

    #[fuchsia::test]
    async fn upload_inline_large_command() {
        // 120 bytes of data -> 160 bytes of Base64, exceeding the legacy 64-byte packet limit.
        let data = &[b'a'; 120];
        let cmds = run_upload_file(
            data,
            UploadMethod::Inline {
                command_prefix: "cmdline add iris.ssh_creds=".to_string(),
                command_max_length: 256,
                init_command: Some("cmdline set".to_string()),
                finalize_command: None,
            },
        )
        .await
        .unwrap();

        let expected_data = BASE64_STANDARD.encode(data);
        assert_eq!(
            cmds,
            ["oem cmdline set", &format!("oem cmdline add iris.ssh_creds={expected_data}")]
        );
    }

    #[fuchsia::test]
    async fn upload_inline_overflow() {
        let err = run_upload_file(
            b"foo",
            UploadMethod::Inline {
                command_prefix: "write-chunk ".to_string(),
                // Command length is too short to fit the prefix with any data.
                command_max_length: 10,
                init_command: None,
                finalize_command: None,
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(err, FfxFastbootError::InlineUploadOverflow { .. }));
    }

    #[fuchsia::test()]
    async fn test_streaming_flash_fails_cleanly() -> std::result::Result<(), anyhow::Error> {
        let tmp_file = NamedTempFile::new().expect("tmp access failed");
        let tmp_file_name = tmp_file.path().to_string_lossy().to_string();

        let tmp_img_files = [(); 4].map(|_| NamedTempFile::new().expect("tmp access failed"));
        let partition_to_path = tmp_img_files
            .iter()
            .zip(["zircon_a", "zircon_b", "vbmeta_a", "vbmeta_b"].iter())
            .map(|(tmpfile, partition_name)| {
                [partition_name, tmpfile.path().to_str().expect("non-unicode tmp path")]
            })
            .collect::<Vec<[&str; 2]>>();

        let manifest = json!({
            "hw_revision": "fastboot",
            "products": [
                {
                    "name": "nostream",
                    "requires_unlock": false,
                    "bootloader_partitions": [],
                    "partitions": partition_to_path.as_slice(),
                    "oem_files": []
                }
            ],
        });

        let v: FlashManifest = from_str(&manifest.to_string())?;
        let (state, mut proxy) = setup();
        {
            let mut state = state.lock().unwrap();
            state.set_multiple_vars([
                (IS_USERSPACE_VAR, "no"),
                (REVISION_VAR, "fastboot"),
                (MAX_DOWNLOAD_SIZE_VAR, "8192"),
            ]);
        }
        let (client, mut server) = mpsc::channel(100);
        v.flash(
            &client,
            &mut TestResolver::new(),
            &mut proxy,
            ManifestParams {
                manifest: Some(PathBuf::from(tmp_file_name)),
                product: "nostream".to_string(),
                op: Command::Boot(BootParams { zbi: None, vbmeta: None, slot: "a".to_string() }),
                ..Default::default()
            },
            None,
        )
        .await?;

        // Checked once for bootloader_partitions and once for product partitions.
        assert_eq!(state.lock().unwrap().get_var_call_count(STREAM_SEGMENT_SIZE), (false, 2));
        server.close();
        let mut messages = vec![];
        while let Some(m) = server.recv().await {
            // The duration uses a real clock, and refactoring to inject a
            // test clock would be a large change.
            if !matches!(&m, Event::FlashPartitionFinished { partition_name: _, duration: _ }) {
                messages.push(m);
            }
        }

        let expected = vec![
            Event::FlashProduct { product_name: "nostream".to_string(), partition_count: 4 },
            Event::Upload(UploadProgress::OnReady { partition: "zircon_a".to_string(), files: 1 }),
            Event::Upload(UploadProgress::OnStarted { size: 0 }),
            Event::Upload(UploadProgress::OnProgress { bytes_written: 0 }),
            Event::Upload(UploadProgress::OnFinished),
            Event::FlashPartition { partition_name: "zircon_a".to_string() },
            Event::Upload(UploadProgress::OnReady { partition: "zircon_b".to_string(), files: 1 }),
            Event::Upload(UploadProgress::OnStarted { size: 0 }),
            Event::Upload(UploadProgress::OnProgress { bytes_written: 0 }),
            Event::Upload(UploadProgress::OnFinished),
            Event::FlashPartition { partition_name: "zircon_b".to_string() },
            Event::Upload(UploadProgress::OnReady { partition: "vbmeta_a".to_string(), files: 1 }),
            Event::Upload(UploadProgress::OnStarted { size: 0 }),
            Event::Upload(UploadProgress::OnProgress { bytes_written: 0 }),
            Event::Upload(UploadProgress::OnFinished),
            Event::FlashPartition { partition_name: "vbmeta_a".to_string() },
            Event::Upload(UploadProgress::OnReady { partition: "vbmeta_b".to_string(), files: 1 }),
            Event::Upload(UploadProgress::OnStarted { size: 0 }),
            Event::Upload(UploadProgress::OnProgress { bytes_written: 0 }),
            Event::Upload(UploadProgress::OnFinished),
            Event::FlashPartition { partition_name: "vbmeta_b".to_string() },
        ];
        assert_eq!(expected, messages);

        Ok(())
    }

    #[fuchsia::test]
    async fn test_stream_flash() -> Result<()> {
        const SEGMENT_SIZE_BYTES: usize = 0x4000;

        let fill_zero = std::iter::repeat(0u8).take(SEGMENT_SIZE_BYTES);
        let fill_data = (0u8..=255).cycle().take(SEGMENT_SIZE_BYTES);
        let buf = multi_chain!(fill_data.clone(), fill_zero.clone(), fill_data.clone())
            .collect::<Vec<_>>();

        let (mut file, tmp_path) = NamedTempFile::new().unwrap().into_parts();
        file.write_all(buf.as_slice()).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.flush().unwrap();

        let mut test_transport = TestTransport::new();
        test_transport.extend([
            Reply::Okay("0x1000".to_owned()),             // Stream segment size
            Reply::Okay("0x2000".to_owned()),             // Partition zircon_a start
            Reply::Okay("0x1000000".to_owned()),          // Partition zircon_a size
            Reply::Okay("0x2000".to_owned()),             // Max download size
            Reply::Fail("Variable not found".to_owned()), // version-gbl (non-GBL target)
            Reply::Data(0x2000),                          // Download request
            Reply::Okay("".to_owned()),                   // Download
            Reply::Okay("".to_owned()),                   // Stream flash
            Reply::Data(0x2000),                          // Download request
            Reply::Okay("".to_owned()),                   // Download
            Reply::Okay("".to_owned()),                   // Stream flash
            Reply::Okay("".to_owned()),                   // Stream fill
            Reply::Data(0x2000),                          // Download request
            Reply::Okay("".to_owned()),                   // Download
            Reply::Okay("".to_owned()),                   // Stream flash
            Reply::Data(0x2000),                          // Download request
            Reply::Okay("".to_owned()),                   // Download
            Reply::Okay("".to_owned()),                   // Stream flash
        ]);

        let mut fastboot_client =
            FastbootProxy::new("stream".to_string(), test_transport, TestTransportFactory {});

        let (var_client, mut var_server): (Sender<Event>, Receiver<Event>) = mpsc::channel(3);
        use Event::*;
        use UploadProgress::*;

        let mut server_actual = vec![];
        let mut server_task = async || {
            while let Some(m) = var_server.recv().await {
                let m = if let FlashPartitionFinished { partition_name, duration: _ } = m {
                    // Cheat to avoid massive refactor around passing a test clock
                    FlashPartitionFinished { partition_name, duration: Duration::seconds(0) }
                } else {
                    m
                };

                server_actual.push(m);
            }
            Ok(())
        };

        let mut resolver = TestResolver::new();
        let flash_partition_task = flash_partition(
            var_client,
            &mut resolver,
            "zircon_a",
            tmp_path.to_str().unwrap(),
            &mut fastboot_client,
            360,
            1000.0,
        );

        try_join!(flash_partition_task, server_task())?;

        let server_expected = &[
            Upload(OnReady { partition: "zircon_a".to_owned(), files: 1 }),
            Upload(OnStarted { size: 0xc000 }),
            Upload(OnProgress { bytes_written: 0x2000 }),
            Upload(OnProgress { bytes_written: 0x2000 }),
            Upload(OnProgress { bytes_written: 0x4000 }),
            Upload(OnProgress { bytes_written: 0x2000 }),
            Upload(OnProgress { bytes_written: 0x2000 }),
            Upload(OnFinished),
            FlashPartitionFinished {
                partition_name: "zircon_a".to_owned(),
                duration: Duration::seconds(0),
            },
        ];

        assert_eq!(&server_actual, server_expected);
        Ok(())
    }

    #[fuchsia::test]
    async fn test_stream_flash_sparse_image() -> Result<()> {
        let raw = (0u8..=255).cycle().take(4096);
        let data = multi_chain!(raw.clone(), raw.clone(), raw.clone(), raw.clone())
            .collect::<Vec<_>>()
            .into_boxed_slice();

        let (mut sparse_file, tmp_path) = NamedTempFile::new().unwrap().into_parts();
        SparseImageBuilder::new()
            .add_source(DataSource::Buffer(data.clone()))
            .add_source(DataSource::Skip(4096))
            .add_source(DataSource::Fill(0, 4096))
            .add_source(DataSource::Buffer(data.clone()))
            .build(&mut sparse_file)
            .expect("Sparse image build failed");
        sparse_file.seek(SeekFrom::Start(0)).unwrap();

        let mut test_transport = TestTransport::new();
        test_transport.extend([
            Reply::Okay("0x1000".to_owned()),             // Stream segment size
            Reply::Okay("0x2000".to_owned()),             // Partition zircon_a start
            Reply::Okay("0x1000000".to_owned()),          // Partition zircon_a size
            Reply::Okay("0x2000".to_owned()),             // Max download size
            Reply::Fail("Variable not found".to_owned()), // version-gbl (non-GBL target)
            Reply::Data(0x2000),                          // Download request
            Reply::Okay("".to_owned()),                   // Download
            Reply::Okay("".to_owned()),                   // Stream flash
            Reply::Data(0x2000),                          // Download request
            Reply::Okay("".to_owned()),                   // Download
            Reply::Okay("".to_owned()),                   // Stream flash
            Reply::Okay("".to_owned()),                   // Stream fill
            Reply::Data(0x2000),                          // Download request
            Reply::Okay("".to_owned()),                   // Download
            Reply::Okay("".to_owned()),                   // Stream flash
            Reply::Data(0x2000),                          // Download request
            Reply::Okay("".to_owned()),                   // Download
            Reply::Okay("".to_owned()),                   // Stream flash
        ]);

        let mut fastboot_client =
            FastbootProxy::new("stream".to_string(), test_transport, TestTransportFactory {});

        let (var_client, mut var_server): (Sender<Event>, Receiver<Event>) = mpsc::channel(3);
        use Event::*;
        use UploadProgress::*;

        let mut server_actual = vec![];
        let mut server_task = async || {
            while let Some(m) = var_server.recv().await {
                let m = if let FlashPartitionFinished { partition_name, duration: _ } = m {
                    // Cheat to avoid massive refactor around passing a test clock
                    FlashPartitionFinished { partition_name, duration: Duration::seconds(0) }
                } else {
                    m
                };

                server_actual.push(m);
            }
            Ok(())
        };

        let mut resolver = TestResolver::new();
        let flash_partition_task = flash_partition(
            var_client,
            &mut resolver,
            "zircon_a",
            tmp_path.to_str().unwrap(),
            &mut fastboot_client,
            360,
            1000.0,
        );

        try_join!(flash_partition_task, server_task())?;

        let server_expected = &[
            Upload(OnReady { partition: "zircon_a".to_owned(), files: 1 }),
            Upload(OnStarted { size: 0xD000 }),
            Upload(OnProgress { bytes_written: 0x2000 }),
            Upload(OnProgress { bytes_written: 0x2000 }),
            Upload(OnProgress { bytes_written: 0x4000 }),
            Upload(OnProgress { bytes_written: 0x2000 }),
            Upload(OnProgress { bytes_written: 0x2000 }),
            Upload(OnFinished),
            FlashPartitionFinished {
                partition_name: "zircon_a".to_owned(),
                duration: Duration::seconds(0),
            },
        ];

        assert_eq!(&server_actual, server_expected);
        Ok(())
    }

    #[fuchsia::test]
    async fn test_stream_flash_error_propagation_stops_producer() -> Result<()> {
        const SEGMENT_SIZE_BYTES: usize = 0x4000;

        let fill_zero = std::iter::repeat(0u8).take(SEGMENT_SIZE_BYTES);
        let fill_data = (0u8..=255).cycle().take(SEGMENT_SIZE_BYTES);
        let buf = multi_chain!(fill_data.clone(), fill_zero.clone(), fill_data.clone())
            .collect::<Vec<_>>();

        let (mut file, tmp_path) = NamedTempFile::new().unwrap().into_parts();
        file.write_all(buf.as_slice()).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.flush().unwrap();

        let mut test_transport = TestTransport::new();
        test_transport.extend([
            Reply::Okay("0x2000".to_owned()),            // Max download size
            Reply::Data(0x2000),                         // Download request
            Reply::Fail("Download ack fail".to_owned()), // Download ack fails -> sends OnError
        ]);

        let mut fastboot_client =
            FastbootProxy::new("stream".to_string(), test_transport, TestTransportFactory {});

        let (var_client, mut var_server): (Sender<Event>, Receiver<Event>) = mpsc::channel(10);
        let result = streaming_flash_impl(
            &var_client,
            "zircon_a",
            tmp_path.to_str().unwrap(),
            &mut fastboot_client,
            0x1000,
            0x2000,
            0x1000000,
            FlashTimeouts::new(360, 1000.0),
            None,
        )
        .await;
        drop(var_client);

        assert!(result.is_err());

        let mut events = vec![];
        while let Some(event) = var_server.recv().await {
            events.push(event);
        }
        assert!(
            events.iter().any(|e| matches!(e, Event::Upload(UploadProgress::OnError { .. }))),
            "Expected UploadProgress::OnError to be forwarded to messenger, got: {events:?}"
        );
        Ok(())
    }

    #[fuchsia::test(logging = true)]
    async fn test_flash_partitions_stream() -> Result<()> {
        struct TestPartition {
            name: String,
            file: String,
        }
        impl Partition for TestPartition {
            fn name(&self) -> &str {
                &self.name
            }
            fn file(&self) -> &str {
                &self.file
            }
            fn variable(&self) -> Option<&str> {
                None
            }
            fn variable_value(&self) -> Option<&str> {
                None
            }
        }

        let buf = (0u8..=255).cycle().take(0x2000).collect::<Vec<_>>();
        let (mut file_a, path_a) = NamedTempFile::new().unwrap().into_parts();
        file_a.write_all(&buf).unwrap();
        file_a.flush().unwrap();

        let (mut file_b, path_b) = NamedTempFile::new().unwrap().into_parts();
        file_b.write_all(&buf).unwrap();
        file_b.flush().unwrap();

        let partitions = vec![
            TestPartition {
                name: "zircon_a".to_owned(),
                file: path_a.to_str().unwrap().to_owned(),
            },
            TestPartition {
                name: "zircon_b".to_owned(),
                file: path_b.to_str().unwrap().to_owned(),
            },
        ];

        let mut test_transport = TestTransport::new();
        test_transport.extend([
            Reply::Okay("0x2000".to_owned()),             // Max download size
            Reply::Okay("0x1000000".to_owned()),          // Partition zircon_a size
            Reply::Okay("0x2000".to_owned()),             // Partition zircon_a start
            Reply::Okay("0x1000000".to_owned()),          // Partition zircon_b size
            Reply::Okay("0x4000".to_owned()),             // Partition zircon_b start
            Reply::Fail("Variable not found".to_owned()), // version-gbl (non-GBL target)
            // Stream commands for zircon_a
            Reply::Data(0x2000),
            Reply::Okay("".to_owned()),
            Reply::Okay("".to_owned()),
            // Stream commands for zircon_b
            Reply::Data(0x2000),
            Reply::Okay("".to_owned()),
            Reply::Okay("".to_owned()),
        ]);

        let mut fastboot_client =
            FastbootProxy::new("stream".to_string(), test_transport, TestTransportFactory {});

        let (var_client, mut var_server): (Sender<Event>, Receiver<Event>) = mpsc::channel(20);
        let mut resolver = TestResolver::new();

        flash_partitions_stream(
            &var_client,
            &mut resolver,
            &partitions,
            &mut fastboot_client,
            4096,
            360,
            1000.0,
        )
        .await?;
        drop(var_client);

        let mut events = vec![];
        while let Some(m) = var_server.recv().await {
            let m = if let Event::FlashPartitionFinished { partition_name, duration: _ } = m {
                Event::FlashPartitionFinished { partition_name, duration: Duration::seconds(0) }
            } else {
                m
            };
            events.push(m);
        }

        use Event::*;
        use UploadProgress::*;
        let expected = vec![
            Upload(OnReady { partition: "zircon_a".to_owned(), files: 1 }),
            Upload(OnStarted { size: 0x2000 }),
            Upload(OnProgress { bytes_written: 0x2000 }),
            Upload(OnFinished),
            FlashPartitionFinished {
                partition_name: "zircon_a".to_owned(),
                duration: Duration::seconds(0),
            },
            Upload(OnReady { partition: "zircon_b".to_owned(), files: 1 }),
            Upload(OnStarted { size: 0x2000 }),
            Upload(OnProgress { bytes_written: 0x2000 }),
            Upload(OnFinished),
            FlashPartitionFinished {
                partition_name: "zircon_b".to_owned(),
                duration: Duration::seconds(0),
            },
        ];
        assert_eq!(events, expected);
        Ok(())
    }

    #[fuchsia::test]
    fn test_from_hex_str() {
        assert_eq!(u32::from_hex_str("0x1000"), Ok(0x1000));
        assert_eq!(u32::from_hex_str("0X1000"), Ok(0x1000));
        assert_eq!(u64::from_hex_str("1000"), Ok(0x1000));
        assert!(u32::from_hex_str("0x0x1000").is_err());
        assert!(u64::from_hex_str("0X0x1000").is_err());
    }

    #[fuchsia::test]
    fn test_flash_timeouts() {
        let timeouts = FlashTimeouts::new(60, 2.0);
        assert_eq!(timeouts.for_bytes(0), Duration::seconds(60));
        assert_eq!(timeouts.for_bytes(64 << 20), Duration::seconds(60));
        // 1 GB at 2 MB/s.
        assert_eq!(timeouts.for_bytes(1_000_000_000), Duration::seconds(500));
        // Without a valid rate, the minimum applies.
        assert_eq!(FlashTimeouts::new(60, 0.0).for_bytes(1 << 40), Duration::seconds(60));
        assert_eq!(FlashTimeouts::new(60, f64::NAN).for_bytes(1 << 40), Duration::seconds(60));
        // Timeouts are capped.
        assert_eq!(
            FlashTimeouts::new(60, 1e-9).for_bytes(u64::MAX),
            Duration::seconds(24 * 60 * 60)
        );
    }

    #[fuchsia::test]
    async fn test_stream_flash_respects_override_max_download_size() -> Result<()> {
        let buf = (0u8..=255).cycle().take(0x4000).collect::<Vec<_>>();
        let (mut file, tmp_path) = NamedTempFile::new().unwrap().into_parts();
        file.write_all(&buf).unwrap();
        file.flush().unwrap();

        assert!(
            create_streaming_iter(
                tmp_path.to_str().unwrap(),
                (u64::from(u32::MAX) + 1).next_multiple_of(U32_SIZE),
                0x1000,
                0,
                0x1000000,
            )
            .is_err()
        );

        let mut test_transport = TestTransport::new();
        test_transport.extend([
            Reply::Data(0x2000),        // Download request (chunk 1)
            Reply::Okay("".to_owned()), // Download
            Reply::Okay("".to_owned()), // Stream flash
            Reply::Data(0x2000),        // Download request (chunk 2)
            Reply::Okay("".to_owned()), // Download
            Reply::Okay("".to_owned()), // Stream flash
        ]);

        let mut fastboot_client =
            FastbootProxy::new("stream".to_string(), test_transport, TestTransportFactory {});

        let (var_client, mut var_server): (Sender<Event>, Receiver<Event>) = mpsc::channel(10);
        streaming_flash_impl(
            &var_client,
            "zircon_a",
            tmp_path.to_str().unwrap(),
            &mut fastboot_client,
            0x1000,
            0x2000,
            0x1000000,
            FlashTimeouts::new(360, 1000.0),
            NonZeroU64::new(0x2000),
        )
        .await?;
        drop(var_client);

        let mut progress_chunks = vec![];
        while let Some(m) = var_server.recv().await {
            if let Event::Upload(UploadProgress::OnProgress { bytes_written }) = m {
                progress_chunks.push(bytes_written);
            }
        }
        assert_eq!(progress_chunks, vec![0x2000, 0x2000]);
        Ok(())
    }

    #[fuchsia::test]
    async fn test_flash_partitions_stream_empty_partitions() -> Result<()> {
        let (state, mut mock) = setup();
        let (var_client, _var_server): (Sender<Event>, Receiver<Event>) = mpsc::channel(10);
        let mut resolver = TestResolver::new();
        let empty_partitions: Vec<ffx_flash_manifest::v1::Partition> = vec![];

        flash_partitions_stream(
            &var_client,
            &mut resolver,
            &empty_partitions,
            &mut mock,
            4096,
            360,
            1000.0,
        )
        .await?;

        assert_eq!(state.lock().unwrap().get_var_call_count(MAX_DOWNLOAD_SIZE_VAR), (false, 0));
        Ok(())
    }

    #[fuchsia::test]
    async fn test_flash_partition_impl_streaming_disabled() -> Result<()> {
        let (mut file, tmp_path) = NamedTempFile::new().unwrap().into_parts();
        file.write_all(&[0u8; 0x1000]).unwrap();
        file.flush().unwrap();
        let path_str = tmp_path.to_str().unwrap();

        let (state, mut mock) = setup();
        {
            let mut state = state.lock().unwrap();
            state.set_multiple_vars([
                (STREAM_SEGMENT_SIZE, "0x1000"),
                (MAX_DOWNLOAD_SIZE_VAR, "0x2000"),
                ("partition-size:zircon_a", "0x10000"),
                ("partition-start:zircon_a", "0x2000"),
            ]);
        }

        let (messenger, mut rx) = mpsc::channel(20);
        flash_partition_impl(messenger, "zircon_a", path_str, &mut mock, 360, 1000.0, None, false)
            .await?;

        assert_eq!(state.lock().unwrap().get_var_call_count(STREAM_SEGMENT_SIZE), (true, 0));

        let mut basic_flashed = vec![];
        while let Some(ev) = rx.recv().await {
            if let Event::FlashPartition { partition_name } = ev {
                basic_flashed.push(partition_name);
            }
        }
        assert_eq!(basic_flashed, vec!["zircon_a".to_string()]);
        Ok(())
    }

    #[fuchsia::test]
    async fn test_flash_partitions_streaming_disabled() -> Result<()> {
        struct TestPartition {
            name: String,
            file: String,
        }
        impl Partition for TestPartition {
            fn name(&self) -> &str {
                &self.name
            }
            fn file(&self) -> &str {
                &self.file
            }
            fn variable(&self) -> Option<&str> {
                None
            }
            fn variable_value(&self) -> Option<&str> {
                None
            }
        }

        let buf = vec![0u8; 0x1000];
        let (mut file_a, path_a) = NamedTempFile::new().unwrap().into_parts();
        file_a.write_all(&buf).unwrap();
        file_a.flush().unwrap();

        let (mut file_b, path_b) = NamedTempFile::new().unwrap().into_parts();
        file_b.write_all(&buf).unwrap();
        file_b.flush().unwrap();

        let partitions = vec![
            TestPartition {
                name: "zircon_a".to_owned(),
                file: path_a.to_str().unwrap().to_owned(),
            },
            TestPartition {
                name: "zircon_b".to_owned(),
                file: path_b.to_str().unwrap().to_owned(),
            },
        ];

        let (state, mut mock) = setup();
        {
            let mut state = state.lock().unwrap();
            state.set_multiple_vars([
                (STREAM_SEGMENT_SIZE, "0x1000"),
                (MAX_DOWNLOAD_SIZE_VAR, "0x2000"),
                ("partition-size:zircon_a", "0x1000000"),
                ("partition-start:zircon_a", "0x2000"),
                ("partition-size:zircon_b", "0x1000000"),
                ("partition-start:zircon_b", "0x4000"),
            ]);
        }

        let (messenger, mut rx) = mpsc::channel(50);
        let mut resolver = TestResolver::new();
        flash_partitions(&messenger, &mut resolver, &partitions, &mut mock, 360, 1000.0, false)
            .await?;
        drop(messenger);

        assert_eq!(state.lock().unwrap().get_var_call_count(STREAM_SEGMENT_SIZE), (true, 0));

        let mut basic_flashed = vec![];
        while let Some(ev) = rx.recv().await {
            if let Event::FlashPartition { partition_name } = ev {
                basic_flashed.push(partition_name);
            }
        }
        assert_eq!(basic_flashed, vec!["zircon_a".to_string(), "zircon_b".to_string()]);
        Ok(())
    }

    #[fuchsia::test]
    async fn test_flash_partitions_streaming_missing_partition_vars_fails() -> Result<()> {
        struct TestPartition {
            name: String,
            file: String,
        }
        impl Partition for TestPartition {
            fn name(&self) -> &str {
                &self.name
            }
            fn file(&self) -> &str {
                &self.file
            }
            fn variable(&self) -> Option<&str> {
                None
            }
            fn variable_value(&self) -> Option<&str> {
                None
            }
        }

        let buf = vec![0u8; 0x1000];
        let (mut file_a, path_a) = NamedTempFile::new().unwrap().into_parts();
        file_a.write_all(&buf).unwrap();
        file_a.flush().unwrap();

        let partitions = vec![TestPartition {
            name: "zircon_a".to_owned(),
            file: path_a.to_str().unwrap().to_owned(),
        }];

        let (state, mut mock) = setup();
        {
            let mut state = state.lock().unwrap();
            state.set_multiple_vars([
                (STREAM_SEGMENT_SIZE, "0x1000"),
                (MAX_DOWNLOAD_SIZE_VAR, "0x2000"),
                // Missing partition-size:zircon_a and partition-start:zircon_a
            ]);
        }

        let (messenger, _rx) = mpsc::channel(20);
        let mut resolver = TestResolver::new();
        let res =
            flash_partitions(&messenger, &mut resolver, &partitions, &mut mock, 360, 1000.0, true)
                .await;
        assert!(res.is_err());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_flash_single_partition_mid_stream_failure_does_not_fallback() -> Result<()> {
        let buf = (0u8..=255).cycle().take(0x2000).collect::<Vec<_>>();
        let (mut file, tmp_path) = NamedTempFile::new().unwrap().into_parts();
        file.write_all(&buf).unwrap();
        file.flush().unwrap();

        let mut test_transport = TestTransport::new();
        test_transport.extend([
            Reply::Okay("0x1000".to_owned()),             // Stream segment size
            Reply::Okay("0x2000".to_owned()),             // Partition zircon_a start
            Reply::Okay("0x1000000".to_owned()),          // Partition zircon_a size
            Reply::Okay("0x2000".to_owned()),             // Max download size
            Reply::Fail("Variable not found".to_owned()), // version-gbl (non-GBL target)
            Reply::Data(0x2000),                          // Download request
            Reply::Okay("".to_owned()),                   // Download ack
            Reply::Fail("disk write error".to_owned()),   // StreamFlash fails mid-stream
        ]);
        let mut fastboot_client =
            FastbootProxy::new("stream".to_string(), test_transport, TestTransportFactory {});

        let (messenger, mut rx) = mpsc::channel(20);
        let res = flash_partition_impl(
            messenger,
            "zircon_a",
            tmp_path.to_str().unwrap(),
            &mut fastboot_client,
            360,
            1000.0,
            None,
            true,
        )
        .await;

        assert!(res.is_err(), "mid-stream failure must propagate instead of falling back");
        while let Some(ev) = rx.recv().await {
            assert!(
                !matches!(ev, Event::FlashPartition { .. }),
                "unexpected fallback to flash_basic_impl: {ev:?}"
            );
        }
        Ok(())
    }

    #[fuchsia::test]
    async fn test_flash_single_partition_oversized_image_does_not_fallback() -> Result<()> {
        let buf = vec![0xabu8; 0x4000];
        let (mut file, tmp_path) = NamedTempFile::new().unwrap().into_parts();
        file.write_all(&buf).unwrap();
        file.flush().unwrap();

        let (state, mut mock) = setup();
        state.lock().unwrap().set_multiple_vars([
            (STREAM_SEGMENT_SIZE, "0x1000"),
            (MAX_DOWNLOAD_SIZE_VAR, "0x4000"),
            ("partition-start:zircon_a", "0x2000"),
            ("partition-size:zircon_a", "0x2000"), // Smaller than 0x4000 image
        ]);

        let (messenger, _rx) = mpsc::channel(20);
        let res = flash_partition_impl(
            messenger,
            "zircon_a",
            tmp_path.to_str().unwrap(),
            &mut mock,
            360,
            1000.0,
            None,
            true,
        )
        .await;

        assert!(matches!(res, Err(FfxFastbootError::StreamingFlash { .. })));
        // MAX_DOWNLOAD_SIZE_VAR was queried once for streaming, NOT a second time for basic flash.
        assert_eq!(state.lock().unwrap().get_var_call_count(MAX_DOWNLOAD_SIZE_VAR), (true, 1));
        Ok(())
    }

    #[fuchsia::test]
    async fn test_flash_single_partition_unaligned_raw_file_falls_back_to_basic() -> Result<()> {
        // 10 bytes is not a multiple of U32_SIZE (4), so streaming is ineligible pre-flight.
        let (mut file, tmp_path) = NamedTempFile::new().unwrap().into_parts();
        file.write_all(&[1u8; 10]).unwrap();
        file.flush().unwrap();

        let (state, mut mock) = setup();
        state.lock().unwrap().set_multiple_vars([
            (STREAM_SEGMENT_SIZE, "0x1000"),
            (MAX_DOWNLOAD_SIZE_VAR, "0x4000"),
            ("partition-start:zircon_a", "0x2000"),
            ("partition-size:zircon_a", "0x10000"),
        ]);

        let (messenger, mut rx) = mpsc::channel(20);
        flash_partition_impl(
            messenger,
            "zircon_a",
            tmp_path.to_str().unwrap(),
            &mut mock,
            360,
            1000.0,
            None,
            true,
        )
        .await?;

        let mut ready_count = 0;
        let mut basic_flashed = false;
        while let Some(ev) = rx.recv().await {
            match ev {
                Event::Upload(UploadProgress::OnReady { .. }) => ready_count += 1,
                Event::FlashPartition { partition_name } if partition_name == "zircon_a" => {
                    basic_flashed = true;
                }
                _ => {}
            }
        }
        assert!(basic_flashed, "ineligible unaligned raw file should fall back to basic flash");
        assert_eq!(ready_count, 1, "OnReady should only be emitted once by basic flash");
        Ok(())
    }

    #[fuchsia::test]
    async fn test_create_streaming_iter_rejects_corrupt_sparse_image() -> Result<()> {
        let raw = vec![0x55u8; 4096].into_boxed_slice();
        let builder = SparseImageBuilder::new().add_source(DataSource::Buffer(raw));
        let mut sparse_bytes = vec![0u8; builder.built_size().try_into().unwrap()];
        builder.build(&mut std::io::Cursor::new(&mut sparse_bytes[..])).unwrap();
        // Keep the valid 28-byte sparse header, but truncate inside the 12-byte chunk header so
        // `SparseReader::new` fails while total file length (32 bytes) is still a multiple of 4.
        sparse_bytes.truncate(32);

        let (mut file, tmp_path) = NamedTempFile::new().unwrap().into_parts();
        file.write_all(&sparse_bytes).unwrap();
        file.flush().unwrap();

        let res = create_streaming_iter(tmp_path.to_str().unwrap(), 0x4000, 0x1000, 0, 0x10000);
        assert!(
            matches!(res, Err(StreamInitError::Fatal(err)) if matches!(*err, FfxFastbootError::Sparse(_))),
            "corrupt sparse image must fail with SparseError rather than raw fallback"
        );
        Ok(())
    }

    #[fuchsia::test]
    async fn test_flash_gbl_async_auto_enabled_cached_and_synced() -> Result<()> {
        let tmp_manifest = NamedTempFile::new().unwrap();
        let (mut bl_file, bl_path) = NamedTempFile::new().unwrap().into_parts();
        bl_file.write_all(&vec![0x11u8; 0x1000]).unwrap();
        bl_file.flush().unwrap();
        let (mut part_file, part_path) = NamedTempFile::new().unwrap().into_parts();
        part_file.write_all(&vec![0x22u8; 0x2000]).unwrap();
        part_file.flush().unwrap();

        let manifest = json!({
            "hw_revision": "fastboot",
            "products": [{
                "name": "gbl_product",
                "requires_unlock": false,
                "bootloader_partitions": [["bootloader", bl_path.to_str().unwrap()]],
                "partitions": [["zircon_a", part_path.to_str().unwrap()]],
                "oem_files": []
            }],
        });
        let v: FlashManifest = from_str(&manifest.to_string()).unwrap();

        let (state, mock) = setup();
        state.lock().unwrap().set_multiple_vars([
            (VERSION_GBL_VAR, "gbl.2026"),
            (IS_USERSPACE_VAR, "no"),
            (REVISION_VAR, "fastboot"),
            (MAX_DOWNLOAD_SIZE_VAR, "0x4000"),
            (STREAM_SEGMENT_SIZE, "0x1000"),
            ("partition-size:bootloader", "0x10000"),
            ("partition-start:bootloader", "0x1000"),
            ("partition-size:zircon_a", "0x10000"),
            ("partition-start:zircon_a", "0x20000"),
        ]);
        let mut cached = CachingFastboot::new(mock);
        let (messenger, _rx) = mpsc::channel(100);

        v.flash(
            &messenger,
            &mut TestResolver::new(),
            &mut cached,
            ManifestParams {
                manifest: Some(tmp_manifest.path().to_path_buf()),
                product: "gbl_product".to_string(),
                no_bootloader_reboot: true,
                flash_min_timeout_seconds: 180,
                flash_timeout_rate_mb_per_second: 2.0,
                streaming_flash: true,
                ..Default::default()
            },
            None,
        )
        .await?;

        let state = state.lock().unwrap();
        // `CachingFastboot` queries `version-gbl` once across bootloader + product batches.
        assert_eq!(state.get_var_call_count(VERSION_GBL_VAR), (true, 1));
        assert_eq!(
            state.oem_commands,
            vec![
                "oem gbl-enable-async-task",
                "oem gbl-sync-tasks",
                "oem gbl-disable-async-task",
                "oem gbl-enable-async-task",
                "oem gbl-sync-tasks",
                "oem gbl-disable-async-task",
            ]
        );
        assert_eq!(
            state.oem_timeouts,
            vec![
                ("oem gbl-sync-tasks".to_string(), Duration::seconds(180)),
                ("oem gbl-sync-tasks".to_string(), Duration::seconds(180)),
            ]
        );
        assert_eq!(state.set_actives, vec!["a".to_string()]);
        assert_eq!(state.continue_boots, 1);
        Ok(())
    }

    #[fuchsia::test]
    async fn test_flash_partition_impl_gbl_async_streaming_wire_sequence() -> Result<()> {
        let buf = (0u8..=255).cycle().take(0x2000).collect::<Vec<_>>();
        let (mut file, tmp_path) = NamedTempFile::new().unwrap().into_parts();
        file.write_all(&buf).unwrap();
        file.flush().unwrap();

        let mut test_transport = TestTransport::new();
        test_transport.extend([
            Reply::Okay("0x1000".to_owned()),    // getvar:stream-segment-size
            Reply::Okay("0x2000".to_owned()),    // getvar:partition-start:zircon_a
            Reply::Okay("0x1000000".to_owned()), // getvar:partition-size:zircon_a
            Reply::Okay("0x2000".to_owned()),    // getvar:max-download-size
            Reply::Okay("gbl.2026".to_owned()),  // getvar:version-gbl
            Reply::Okay("".to_owned()),          // oem gbl-enable-async-task
            Reply::Data(0x2000),                 // download:00002000
            Reply::Okay("".to_owned()),          // download ack
            Reply::Okay("".to_owned()),          // stream-flash
            Reply::Okay("".to_owned()),          // oem gbl-sync-tasks
            Reply::Okay("".to_owned()),          // oem gbl-disable-async-task
        ]);
        let mut fastboot_client =
            FastbootProxy::new("stream".to_string(), test_transport, TestTransportFactory {});
        let (messenger, _rx) = mpsc::channel(20);

        flash_partition_impl(
            messenger,
            "zircon_a",
            tmp_path.to_str().unwrap(),
            &mut fastboot_client,
            360,
            1000.0,
            None,
            true,
        )
        .await
    }

    #[fuchsia::test]
    async fn test_flash_non_gbl_target_sends_no_gbl_oem_commands() -> Result<()> {
        let (mut file, tmp_path) = NamedTempFile::new().unwrap().into_parts();
        file.write_all(&[0u8; 0x1000]).unwrap();
        file.flush().unwrap();

        let (state, mock) = setup();
        state.lock().unwrap().set_multiple_vars([(MAX_DOWNLOAD_SIZE_VAR, "0x2000")]);
        let mut cached = CachingFastboot::new(mock);
        let (messenger, _rx) = mpsc::channel(20);

        flash_partition_impl(
            messenger.clone(),
            "zircon_a",
            tmp_path.to_str().unwrap(),
            &mut cached,
            120,
            10.0,
            None,
            false,
        )
        .await?;
        flash_partition_impl(
            messenger,
            "zircon_b",
            tmp_path.to_str().unwrap(),
            &mut cached,
            120,
            10.0,
            None,
            false,
        )
        .await?;

        let state = state.lock().unwrap();
        assert_eq!(state.get_var_call_count(VERSION_GBL_VAR), (false, 1));
        assert!(state.oem_commands.is_empty());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_flash_gbl_unsupported_async_oem_falls_back_cleanly() -> Result<()> {
        let (mut file, tmp_path) = NamedTempFile::new().unwrap().into_parts();
        file.write_all(&[0xabu8; 0x1000]).unwrap();
        file.flush().unwrap();

        let (state, mut mock) = setup();
        {
            let mut state = state.lock().unwrap();
            state.set_multiple_vars([
                (VERSION_GBL_VAR, "gbl.legacy"),
                (MAX_DOWNLOAD_SIZE_VAR, "0x2000"),
            ]);
            state.fail_oem(GBL_ENABLE_ASYNC_TASK_COMMAND, "Unknown OEM command");
        }
        let (messenger, _rx) = mpsc::channel(20);

        flash_partition_impl(
            messenger,
            "zircon_a",
            tmp_path.to_str().unwrap(),
            &mut mock,
            120,
            10.0,
            None,
            false,
        )
        .await?;

        assert_eq!(state.lock().unwrap().oem_commands, vec!["oem gbl-enable-async-task"]);
        Ok(())
    }

    #[fuchsia::test]
    async fn test_flash_gbl_async_syncs_after_protocol_fail_but_not_transport_disconnect()
    -> Result<()> {
        let buf = (0u8..=255).cycle().take(0x2000).collect::<Vec<_>>();
        let (mut file, tmp_path) = NamedTempFile::new().unwrap().into_parts();
        file.write_all(&buf).unwrap();
        file.flush().unwrap();

        // Case 1: Device replies FAIL to stream-flash while session remains connected -> syncs and
        // disables async tasks before returning the stream error.
        let mut live_transport = TestTransport::new();
        live_transport.extend([
            Reply::Okay("0x1000".to_owned()),         // stream-segment-size
            Reply::Okay("0x2000".to_owned()),         // partition-start
            Reply::Okay("0x1000000".to_owned()),      // partition-size
            Reply::Okay("0x2000".to_owned()),         // max-download-size
            Reply::Okay("gbl.2026".to_owned()),       // version-gbl
            Reply::Okay("".to_owned()),               // oem gbl-enable-async-task
            Reply::Data(0x2000),                      // download
            Reply::Okay("".to_owned()),               // download ack
            Reply::Fail("write rejected".to_owned()), // stream-flash FAIL
            Reply::Okay("".to_owned()),               // oem gbl-sync-tasks
            Reply::Okay("".to_owned()),               // oem gbl-disable-async-task
            Reply::Okay("after-sync".to_owned()),     // proves sync + disable were both consumed
        ]);
        let mut live_client =
            FastbootProxy::new("stream".to_string(), live_transport, TestTransportFactory {});
        let (messenger, _rx) = mpsc::channel(20);
        let err = flash_partition_impl(
            messenger,
            "zircon_a",
            tmp_path.to_str().unwrap(),
            &mut live_client,
            120,
            10.0,
            None,
            true,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            FfxFastbootError::Interface(FastbootError::FlashError(FlashError::StreamFailed { .. }))
        ));
        assert!(verify_variable_value("product", "after-sync", &mut live_client).await.unwrap());

        // Case 2: Both stream-flash and oem gbl-sync-tasks reply FAIL -> skips disable, logs sync
        // warning, and preserves the primary StreamFailed error.
        let mut sync_fail_transport = TestTransport::new();
        sync_fail_transport.extend([
            Reply::Okay("0x1000".to_owned()),          // stream-segment-size
            Reply::Okay("0x2000".to_owned()),          // partition-start
            Reply::Okay("0x1000000".to_owned()),       // partition-size
            Reply::Okay("0x2000".to_owned()),          // max-download-size
            Reply::Okay("gbl.2026".to_owned()),        // version-gbl
            Reply::Okay("".to_owned()),                // oem gbl-enable-async-task
            Reply::Data(0x2000),                       // download
            Reply::Okay("".to_owned()),                // download ack
            Reply::Fail("write rejected".to_owned()),  // stream-flash FAIL
            Reply::Fail("sync failed".to_owned()),     // oem gbl-sync-tasks FAIL
            Reply::Okay("after-sync-fail".to_owned()), // proves disable was NOT sent after failed sync
        ]);
        let mut sync_fail_client =
            FastbootProxy::new("stream".to_string(), sync_fail_transport, TestTransportFactory {});
        let (messenger, _rx) = mpsc::channel(20);
        let err = flash_partition_impl(
            messenger,
            "zircon_a",
            tmp_path.to_str().unwrap(),
            &mut sync_fail_client,
            120,
            10.0,
            None,
            true,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            FfxFastbootError::Interface(FastbootError::FlashError(FlashError::StreamFailed { .. }))
        ));
        assert!(
            verify_variable_value("product", "after-sync-fail", &mut sync_fail_client)
                .await
                .unwrap()
        );

        // Case 3: Transport disconnects during download and reconnect also hits EOF -> must NOT
        // attempt a third `open()` just to send `oem gbl-sync-tasks`.
        #[derive(Default, Debug)]
        struct CountingFactory {
            opens: usize,
        }
        impl InterfaceFactoryBase<TestTransport> for CountingFactory {
            async fn open(&mut self) -> std::result::Result<TestTransport, InterfaceFactoryError> {
                self.opens += 1;
                assert!(
                    self.opens <= 1,
                    "finish_gbl_async_tasks must not reopen a disconnected transport"
                );
                Ok(TestTransport::new()) // Empty -> EOF on retry too
            }
            async fn close(&self) {}
            async fn rediscover(&mut self) -> std::result::Result<(), InterfaceFactoryError> {
                Ok(())
            }
            fn supports_gbl_async_tasks(&self) -> bool {
                true
            }
        }
        impl InterfaceFactory<TestTransport> for CountingFactory {}

        let mut broken_transport = TestTransport::new();
        broken_transport.extend([
            Reply::Okay("0x1000".to_owned()),    // stream-segment-size
            Reply::Okay("0x2000".to_owned()),    // partition-start
            Reply::Okay("0x1000000".to_owned()), // partition-size
            Reply::Okay("0x2000".to_owned()),    // max-download-size
            Reply::Okay("gbl.2026".to_owned()),  // version-gbl
            Reply::Okay("".to_owned()),          // oem gbl-enable-async-task
                                                 // EOF during download -> triggers 1 reconnect retry, then fails with transport error.
        ]);
        let mut broken_client =
            FastbootProxy::new("stream".to_string(), broken_transport, CountingFactory::default());
        let (messenger, _rx) = mpsc::channel(20);
        assert!(
            flash_partition_impl(
                messenger,
                "zircon_a",
                tmp_path.to_str().unwrap(),
                &mut broken_client,
                120,
                10.0,
                None,
                true,
            )
            .await
            .is_err()
        );
        Ok(())
    }

    #[fuchsia::test]
    async fn test_flash_partitions_empty_skips_gbl_probe() -> Result<()> {
        let (state, mut mock) = setup();
        state
            .lock()
            .unwrap()
            .set_multiple_vars([(VERSION_GBL_VAR, "gbl.2026"), (STREAM_SEGMENT_SIZE, "0x1000")]);
        let (messenger, _rx) = mpsc::channel(10);
        let mut resolver = TestResolver::new();
        let empty: Vec<ffx_flash_manifest::v1::Partition> = vec![];

        flash_partitions(&messenger, &mut resolver, &empty, &mut mock, 120, 10.0, false).await?;
        flash_partitions(&messenger, &mut resolver, &empty, &mut mock, 120, 10.0, true).await?;

        let state = state.lock().unwrap();
        assert_eq!(state.get_var_call_count(VERSION_GBL_VAR), (true, 0));
        assert!(state.oem_commands.is_empty());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_flash_non_tcp_transport_skips_gbl_async_tasks() -> Result<()> {
        // Factory with default `supports_gbl_async_tasks() == false` (matching `UsbFactory` /
        // `UdpFactory`) must not query `version-gbl` or send `oem gbl-enable-async-task`.
        #[derive(Default, Debug, Clone)]
        struct NonTcpTransportFactory;
        impl InterfaceFactoryBase<TestTransport> for NonTcpTransportFactory {
            async fn open(&mut self) -> std::result::Result<TestTransport, InterfaceFactoryError> {
                Ok(TestTransport::new())
            }
            async fn close(&self) {}
            async fn rediscover(&mut self) -> std::result::Result<(), InterfaceFactoryError> {
                Ok(())
            }
        }
        impl InterfaceFactory<TestTransport> for NonTcpTransportFactory {}

        let buf = (0u8..=255).cycle().take(0x1000).collect::<Vec<_>>();
        let (mut file, tmp_path) = NamedTempFile::new().unwrap().into_parts();
        file.write_all(&buf).unwrap();
        file.flush().unwrap();

        let mut usb_transport = TestTransport::new();
        usb_transport.extend([
            Reply::Okay("0x1000".to_owned()),    // getvar:stream-segment-size
            Reply::Okay("0x2000".to_owned()),    // getvar:partition-start:zircon_a
            Reply::Okay("0x1000000".to_owned()), // getvar:partition-size:zircon_a
            Reply::Okay("0x2000".to_owned()),    // getvar:max-download-size
            Reply::Data(0x1000),                 // download:00001000 (no version-gbl / oem!)
            Reply::Okay("".to_owned()),          // download ack
            Reply::Okay("".to_owned()),          // stream-flash
        ]);
        let mut usb_client = CachingFastboot::new(FastbootProxy::new(
            "usb-serial".to_string(),
            usb_transport,
            NonTcpTransportFactory,
        ));
        let (messenger, _rx) = mpsc::channel(20);
        flash_partition_impl(
            messenger.clone(),
            "zircon_a",
            tmp_path.to_str().unwrap(),
            &mut usb_client,
            120,
            10.0,
            None,
            true,
        )
        .await?;

        // Also verify multi-partition `flash_partitions` (both streaming and classical) via
        // `TestFastbootInterface` with `set_supports_gbl_async_tasks(false)`.
        let (state, mut mock) = setup();
        mock.set_supports_gbl_async_tasks(false);
        state.lock().unwrap().set_multiple_vars([
            (VERSION_GBL_VAR, "gbl.2026"),
            (MAX_DOWNLOAD_SIZE_VAR, "0x4000"),
            (STREAM_SEGMENT_SIZE, "0x1000"),
            ("partition-size:zircon_a", "0x100000"),
            ("partition-start:zircon_a", "0x2000"),
        ]);
        let mut cached_mock = CachingFastboot::new(mock);
        let mut resolver = TestResolver::new();
        let parts = vec![ffx_flash_manifest::v1::Partition::new(
            "zircon_a".to_string(),
            tmp_path.to_str().unwrap().to_string(),
            None,
            None,
        )];
        flash_partitions(&messenger, &mut resolver, &parts, &mut cached_mock, 120, 10.0, true)
            .await?;
        flash_partitions(&messenger, &mut resolver, &parts, &mut cached_mock, 120, 10.0, false)
            .await?;

        let state = state.lock().unwrap();
        assert_eq!(state.get_var_call_count(VERSION_GBL_VAR), (true, 0));
        assert!(state.oem_commands.is_empty());
        Ok(())
    }
}
