// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::block::boot_manager_open_any_partition;
use crate::boot_manager::{BootManager, parse_current_configuration};
use crate::partition::PartitionClient;
pub use abr::{ABR_DATA_SIZE, ABR_MAX_TRIES_REMAINING, ONE_SHOT_BOOTLOADER, ONE_SHOT_RECOVERY};
use abr::{
    Error as AbrError, Ops as AbrOps, Result as AbrResult, SlotIndex, SlotInfo, SlotState,
    UnbootableReason, get_and_clear_one_shot_flag, get_boot_slot, get_slot_info,
    get_slot_last_marked_active, mark_slot_active, mark_slot_successful, mark_slot_unbootable,
    set_one_shot_bootloader, set_one_shot_recovery,
};
use async_trait::async_trait;
use fidl_next_fuchsia_paver as fpaver;
use futures::lock::Mutex;
use std::fmt::Write;
use std::rc::Rc;

const _: () = assert!(ABR_MAX_TRIES_REMAINING == fpaver::MAX_PENDING_BOOT_ATTEMPTS);

/// Converts a [`fpaver::Configuration`] into the corresponding [`abr::SlotIndex`].
fn paver_to_libabr_slot(config: fpaver::Configuration) -> SlotIndex {
    match config {
        fpaver::Configuration::A => SlotIndex::A,
        fpaver::Configuration::B => SlotIndex::B,
        fpaver::Configuration::Recovery => SlotIndex::R,
    }
}

/// Converts an [`abr::SlotIndex`] into the corresponding [`fpaver::Configuration`].
fn libabr_to_paver_slot(slot: SlotIndex) -> fpaver::Configuration {
    match slot {
        SlotIndex::A => fpaver::Configuration::A,
        SlotIndex::B => fpaver::Configuration::B,
        SlotIndex::R => fpaver::Configuration::Recovery,
    }
}

/// Maps an [`abr::Error`] from `libabr` to a Fuchsia [`zx::Status`].
fn abr_error_to_status(err: AbrError) -> zx::Status {
    match err {
        AbrError::OpsError(_) => zx::Status::IO,
        AbrError::InvalidInput => zx::Status::INVALID_ARGS,
        AbrError::BadMagic | AbrError::BadChecksum => zx::Status::IO_DATA_INTEGRITY,
        AbrError::UnsupportedVersion => zx::Status::NOT_SUPPORTED,
    }
}

/// Maps a `libabr` [`UnbootableReason`] to the corresponding [`fpaver::UnbootableReason`].
fn unbootable_reason_to_fidl(reason: UnbootableReason) -> fpaver::UnbootableReason {
    match reason {
        UnbootableReason::None => fpaver::UnbootableReason::None,
        UnbootableReason::NoMoreTries => fpaver::UnbootableReason::NoMoreTries,
        UnbootableReason::OsRequested => fpaver::UnbootableReason::OsRequested,
        UnbootableReason::VerificationFailure => fpaver::UnbootableReason::VerificationFailure,
        UnbootableReason::Unknown(raw) => fpaver::UnbootableReason::from(u32::from(raw)),
    }
}

/// [`std::fmt::Write`] adapter forwarding each diagnostic message from `libabr` (`Ops::console`)
/// to `log::debug!`.
struct AbrConsoleLogger;

/// Logs a `libabr` diagnostic `message` at `debug` level, stripping the trailing newline that
/// `libabr` appends for raw serial consoles.
fn log_abr_message(message: &str) {
    let trimmed = message.trim_end_matches(['\r', '\n']);
    if !trimmed.is_empty() {
        log::debug!(target: "libabr", "{trimmed}");
    }
}

impl Write for AbrConsoleLogger {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        log_abr_message(s);
        Ok(())
    }

    // Each `log::debug!` call emits a separate structured Fuchsia log record. `libabr` writes each
    // diagnostic via a single `write!(f, ...)` (`write_fmt`) call, whose default trait
    // implementation splits `args` into multiple `write_str` calls; formatting `args` as a whole
    // here keeps each diagnostic on a single log record without buffering.
    fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> std::fmt::Result {
        log_abr_message(&args.to_string());
        Ok(())
    }
}

/// In-memory [`ABR_DATA_SIZE`]-byte A/B/R metadata buffer implementing [`abr::Ops`] with dirty
/// tracking so synchronous `libabr` state transitions can run between async [`PartitionClient`]
/// I/O calls.
struct AbrBufferOps {
    data: [u8; ABR_DATA_SIZE],
    dirty: bool,
    console: AbrConsoleLogger,
}

impl AbrBufferOps {
    /// Creates an [`AbrBufferOps`] wrapping `data` read from persistent storage.
    fn new(data: [u8; ABR_DATA_SIZE]) -> Self {
        Self { data, dirty: false, console: AbrConsoleLogger }
    }
}

impl AbrOps for AbrBufferOps {
    fn read_abr_metadata(&mut self, out: &mut [u8]) -> AbrResult<()> {
        let src = self.data.get(..out.len()).ok_or("ABR read out of range")?;
        out.copy_from_slice(src);
        Ok(())
    }

    fn write_abr_metadata(&mut self, data: &mut [u8]) -> AbrResult<()> {
        let dst = self.data.get_mut(..data.len()).ok_or("ABR write out of range")?;
        dst.copy_from_slice(data);
        self.dirty = true;
        Ok(())
    }

    fn console(&mut self) -> Option<&mut dyn Write> {
        Some(&mut self.console)
    }
}

struct ConfigurationState {
    status: fpaver::ConfigurationStatus,
    boot_attempts: Option<u8>,
    unbootable_reason: Option<fpaver::UnbootableReason>,
}

/// [`BootManager`] implementation backed by `libabr` (`abr`) and a raw block partition (such as
/// `"durable_boot"` on Sherlock/VIM3 or `"misc"` on Nelson).
pub struct AbrBootManager {
    current_configuration: fpaver::Configuration,
    partition: Box<dyn PartitionClient>,
    // Unlike normal partition writes that replace a whole partition image, ABR updates are
    // read-modify-write operations over a single shared struct holding both slots' state,
    // `one_shot_flags`, and a CRC32 checksum.
    //
    // Paver serves each `BootManager` channel (and `Lifecycle.Stop`) in its own
    // `Scope::spawn_local` task on the local executor, and `PartitionClient` storage I/O yields
    // across `.await` points. Sharing a single `Rc<Mutex<()>>` owned by the board's `Paver` across
    // `AbrBootManager` instances serializes `with_ops` (read -> closure -> conditional write +
    // flush) and `flush` so tasks cannot observe un-flushed state or overwrite concurrent updates.
    //
    // Note: Raw `DataSink` / `PartitionIo` writes to the same backing partition (for example,
    // `WritePartition` on `"misc"` / `"durable_boot"`, or writes to bytes `[0..2048)` of `"misc"`
    // on Nelson) are not serialized by this lock unless the board backend also shares it with
    // those paths.
    lock: Rc<Mutex<()>>,
}

impl std::fmt::Debug for AbrBootManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AbrBootManager")
            .field("current_configuration", &self.current_configuration)
            .finish_non_exhaustive()
    }
}

impl AbrBootManager {
    /// Creates an [`AbrBootManager`] from structured configuration, a list of candidate ABR
    /// partition names (probing candidate names concurrently until one opens), and a shared `lock`
    /// serializing concurrent ABR operations across [`BootManager`] instances.
    ///
    /// # Errors
    ///
    /// - Returns `Err(zx::Status::NOT_SUPPORTED)` if `config` does not specify a supported current
    ///   boot slot or `partition_names` is empty.
    /// - Propagates any other error from [`boot_manager_open_any_partition`], such as
    ///   `Err(zx::Status::NOT_FOUND)` if no candidate is served under `/block/bootmanager`.
    pub async fn from_config(
        config: &crate::PaverConfig,
        partition_names: &[&str],
        lock: Rc<Mutex<()>>,
    ) -> Result<Self, zx::Status> {
        let current_configuration = parse_current_configuration(config)?;
        let partition = boot_manager_open_any_partition(partition_names).await?;
        Ok(Self::new(current_configuration, Box::new(partition), lock))
    }

    /// Creates an [`AbrBootManager`] with the given `current_configuration`, ABR `partition`, and
    /// shared `lock` serializing concurrent ABR operations.
    pub fn new(
        current_configuration: fpaver::Configuration,
        partition: Box<dyn PartitionClient>,
        lock: Rc<Mutex<()>>,
    ) -> Self {
        Self { current_configuration, partition, lock }
    }

    /// Returns a reference to the underlying ABR metadata [`PartitionClient`].
    #[cfg(test)]
    pub fn partition(&self) -> &dyn PartitionClient {
        &*self.partition
    }

    /// Acquires `self.lock`, reads the [`ABR_DATA_SIZE`]-byte ABR metadata region at offset 0 of
    /// `self.partition` into an [`AbrBufferOps`], runs `f`, and writes back and flushes the
    /// metadata only if `f` modified it.
    async fn with_ops<T>(
        &self,
        f: impl FnOnce(&mut AbrBufferOps) -> Result<T, zx::Status>,
    ) -> Result<T, zx::Status> {
        let _guard = self.lock.lock().await;
        let mut data = [0u8; ABR_DATA_SIZE];
        self.partition.read_at(&mut data, 0).await.map_err(|status| {
            log::error!(status:?; "Failed to read ABR metadata from storage");
            zx::Status::IO
        })?;
        let mut ops = AbrBufferOps::new(data);
        let out = f(&mut ops)?;
        if ops.dirty {
            self.partition.write_at(&ops.data, 0).await.map_err(|status| {
                log::error!(status:?; "Failed to write ABR metadata to storage");
                zx::Status::IO
            })?;
            self.partition.flush().await.map_err(|status| {
                log::error!(status:?; "Failed to flush ABR metadata to storage");
                zx::Status::IO
            })?;
        }
        Ok(out)
    }

    /// Detects whether `configuration` is currently running its final boot attempt.
    ///
    /// On the final boot attempt, `libabr` has already decremented `tries_remaining` to 0 in the
    /// bootloader, making the slot appear unbootable with reason `NoMoreTries` (or `None` on
    /// older bootloaders) even though it is currently booted and pending health verification.
    fn is_final_boot_attempt(
        &self,
        slot_info: &SlotInfo,
        configuration: fpaver::Configuration,
    ) -> bool {
        match slot_info.state {
            SlotState::Unbootable(UnbootableReason::NoMoreTries | UnbootableReason::None) => {
                self.current_configuration == configuration
            }
            SlotState::Successful | SlotState::Bootable(_) | SlotState::Unbootable(_) => false,
        }
    }

    /// Queries and translates the [`ConfigurationState`] of `configuration` from `libabr`.
    async fn configuration_state(
        &self,
        configuration: fpaver::Configuration,
    ) -> Result<ConfigurationState, zx::Status> {
        // `libabr::get_slot_info` reports `SlotIndex::R` as unconditionally `Successful`, whereas
        // `BootManager` requires rejecting `Recovery` with `INVALID_ARGS`.
        if configuration == fpaver::Configuration::Recovery {
            return Err(zx::Status::INVALID_ARGS);
        }
        let slot_index = paver_to_libabr_slot(configuration);
        let slot_info = self
            .with_ops(|ops| get_slot_info(ops, slot_index).map_err(abr_error_to_status))
            .await?;

        match slot_info.state {
            SlotState::Successful => Ok(ConfigurationState {
                status: fpaver::ConfigurationStatus::Healthy,
                boot_attempts: None,
                unbootable_reason: None,
            }),
            SlotState::Bootable(tries_remaining) => {
                if tries_remaining > fpaver::MAX_PENDING_BOOT_ATTEMPTS {
                    log::error!(
                        configuration:?,
                        tries_remaining;
                        "Invalid slot tries_remaining"
                    );
                    return Err(zx::Status::INTERNAL);
                }
                Ok(ConfigurationState {
                    status: fpaver::ConfigurationStatus::Pending,
                    boot_attempts: Some(fpaver::MAX_PENDING_BOOT_ATTEMPTS - tries_remaining),
                    unbootable_reason: None,
                })
            }
            SlotState::Unbootable(reason) => {
                if self.is_final_boot_attempt(&slot_info, configuration) {
                    return Ok(ConfigurationState {
                        status: fpaver::ConfigurationStatus::Pending,
                        boot_attempts: Some(fpaver::MAX_PENDING_BOOT_ATTEMPTS),
                        unbootable_reason: None,
                    });
                }
                Ok(ConfigurationState {
                    status: fpaver::ConfigurationStatus::Unbootable,
                    boot_attempts: None,
                    unbootable_reason: Some(unbootable_reason_to_fidl(reason)),
                })
            }
        }
    }

    /// Sets the one-shot bootloader flag in ABR metadata.
    ///
    /// # Errors
    ///
    /// - Returns `Err(zx::Status::IO)` if reading, writing, or flushing the ABR metadata partition
    ///   fails.
    /// - Returns `Err(zx::Status::NOT_SUPPORTED)` if the stored ABR metadata has an unsupported
    ///   major version.
    pub async fn set_one_shot_bootloader(&mut self) -> Result<(), zx::Status> {
        self.with_ops(|ops| set_one_shot_bootloader(ops, true).map_err(abr_error_to_status)).await
    }

    /// Reads and clears the one-shot flags from ABR metadata.
    ///
    /// # Errors
    ///
    /// - Returns `Err(zx::Status::IO)` if reading, writing, or flushing the ABR metadata partition
    ///   fails.
    /// - Returns `Err(zx::Status::NOT_SUPPORTED)` if the stored ABR metadata has an unsupported
    ///   major version.
    pub async fn get_and_clear_one_shot_flags(&mut self) -> Result<u8, zx::Status> {
        self.with_ops(|ops| get_and_clear_one_shot_flag(ops).map_err(abr_error_to_status)).await
    }
}

#[async_trait(?Send)]
impl BootManager for AbrBootManager {
    async fn query_current_configuration(&mut self) -> Result<fpaver::Configuration, zx::Status> {
        Ok(self.current_configuration)
    }

    async fn query_active_configuration(&mut self) -> Result<fpaver::Configuration, zx::Status> {
        let (slot_index, _) = self.with_ops(|ops| Ok(get_boot_slot(ops, false))).await?;
        if slot_index == SlotIndex::R {
            return Err(zx::Status::NOT_SUPPORTED);
        }
        Ok(libabr_to_paver_slot(slot_index))
    }

    async fn query_configuration_last_set_active(
        &mut self,
    ) -> Result<fpaver::Configuration, zx::Status> {
        let slot_index = self
            .with_ops(|ops| get_slot_last_marked_active(ops).map_err(abr_error_to_status))
            .await?;
        Ok(libabr_to_paver_slot(slot_index))
    }

    async fn query_configuration_status(
        &mut self,
        configuration: fpaver::Configuration,
    ) -> Result<fpaver::ConfigurationStatus, zx::Status> {
        Ok(self.configuration_state(configuration).await?.status)
    }

    async fn query_configuration_status_and_boot_attempts(
        &mut self,
        configuration: fpaver::Configuration,
    ) -> Result<fpaver::BootManagerQueryConfigurationStatusAndBootAttemptsResponse, zx::Status>
    {
        let state = self.configuration_state(configuration).await?;
        Ok(fpaver::BootManagerQueryConfigurationStatusAndBootAttemptsResponse {
            status: Some(state.status),
            boot_attempts: state.boot_attempts,
            unbootable_reason: state.unbootable_reason,
        })
    }

    async fn set_configuration_active(
        &mut self,
        configuration: fpaver::Configuration,
    ) -> Result<(), zx::Status> {
        let config_num = u32::from(configuration);
        log::info!("Setting configuration {config_num} as active");

        let slot_index = paver_to_libabr_slot(configuration);
        self.with_ops(|ops| mark_slot_active(ops, slot_index).map_err(abr_error_to_status)).await?;

        log::info!("Set active configuration to {config_num}");
        Ok(())
    }

    async fn set_configuration_unbootable(
        &mut self,
        configuration: fpaver::Configuration,
    ) -> Result<(), zx::Status> {
        let config_num = u32::from(configuration);
        log::info!("Setting configuration {config_num} as unbootable");

        let slot_index = paver_to_libabr_slot(configuration);
        self.with_ops(|ops| {
            mark_slot_unbootable(ops, slot_index, UnbootableReason::OsRequested)
                .map_err(abr_error_to_status)
        })
        .await?;

        // Load-bearing log message in the firmware test suite.
        log::info!("Set {config_num} configuration as unbootable");
        Ok(())
    }

    async fn set_configuration_healthy(
        &mut self,
        configuration: fpaver::Configuration,
    ) -> Result<(), zx::Status> {
        let config_num = u32::from(configuration);
        log::info!("Setting configuration {config_num} as healthy");

        let slot_index = paver_to_libabr_slot(configuration);
        self.with_ops(|ops| {
            let Err(err) =
                mark_slot_successful(ops, slot_index, false).map_err(abr_error_to_status)
            else {
                return Ok(());
            };
            match get_slot_info(ops, slot_index) {
                Ok(slot_info) if self.is_final_boot_attempt(&slot_info, configuration) => {
                    log::info!("Last boot attempt; allowing unbootable -> success");
                    mark_slot_successful(ops, slot_index, true).map_err(abr_error_to_status)
                }
                _ => {
                    log::error!(
                        err:?;
                        "Failed to set configuration {config_num} healthy"
                    );
                    Err(err)
                }
            }
        })
        .await?;

        // Load-bearing log message in the firmware test suite.
        log::info!("Set configuration {config_num} healthy");
        Ok(())
    }

    async fn set_one_shot_recovery(&mut self) -> Result<(), zx::Status> {
        log::info!("Setting one shot recovery");
        self.with_ops(|ops| set_one_shot_recovery(ops, true).map_err(abr_error_to_status)).await?;
        log::info!("Set one shot recovery succeed");
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), zx::Status> {
        // `with_ops` already writes and flushes on every metadata change, so this only re-flushes
        // the underlying partition to uphold the `BootManager.Flush` contract.
        let _guard = self.lock.lock().await;
        self.partition.flush().await.map_err(|status| {
            log::error!(status:?; "Failed to flush ABR metadata to storage");
            zx::Status::IO
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fake_block_client::FakeBlockClient;

    const TEST_BLOCK_SIZE: u32 = 512;
    const TEST_BLOCK_COUNT: usize = 2;

    /// Constructs serialized ABR metadata where slot A is unbootable and slot B is active and
    /// successful using `libabr`'s public [`AbrOps`] implementation on `[u8; ABR_DATA_SIZE]`.
    fn abr_a_unbootable_b_successful() -> [u8; ABR_DATA_SIZE] {
        let mut buf = [0u8; ABR_DATA_SIZE];
        mark_slot_active(&mut buf, SlotIndex::B).expect("mark B active");
        mark_slot_successful(&mut buf, SlotIndex::B, false).expect("mark B successful");
        mark_slot_unbootable(&mut buf, SlotIndex::A, UnbootableReason::None)
            .expect("mark A unbootable");
        buf
    }

    /// Constructs serialized ABR metadata where both slots A and B are unbootable with `reason`.
    fn abr_both_unbootable(reason: UnbootableReason) -> [u8; ABR_DATA_SIZE] {
        let mut buf = [0u8; ABR_DATA_SIZE];
        mark_slot_unbootable(&mut buf, SlotIndex::A, reason).expect("mark A unbootable");
        mark_slot_unbootable(&mut buf, SlotIndex::B, reason).expect("mark B unbootable");
        buf
    }

    /// Computes the IEEE 802.3 CRC32 checksum used by the 32-byte ABR wire header.
    fn abr_crc32(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xffff_ffff;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                if (crc & 1) != 0 {
                    crc = (crc >> 1) ^ 0xedb8_8320;
                } else {
                    crc >>= 1;
                }
            }
        }
        !crc
    }

    /// Constructs serialized ABR metadata with an unsupported major version (`99`) and a valid
    /// header CRC32 so `libabr` rejects it with `Error::UnsupportedVersion`.
    fn abr_with_unsupported_major_version() -> [u8; ABR_DATA_SIZE] {
        // Offsets 4 (`version_major`) and 28..32 (`crc32`) follow the 32-byte ABR wire header.
        const _: () = assert!(ABR_DATA_SIZE == 32);
        let mut buf = abr_a_unbootable_b_successful();
        buf[4] = 99;
        let crc = abr_crc32(&buf[..28]);
        buf[28..ABR_DATA_SIZE].copy_from_slice(&crc.to_be_bytes());
        buf
    }

    /// Creates a single-instance [`AbrBootManager`] with its own fresh [`Mutex`] for unit tests.
    fn new_test_bm(
        current_configuration: fpaver::Configuration,
        partition: Box<dyn PartitionClient>,
    ) -> AbrBootManager {
        AbrBootManager::new(current_configuration, partition, Rc::default())
    }

    /// Initializes a fake block partition with optional serialized ABR bytes at offset 0 and
    /// `0x5a` sentinel bytes across the remainder of block 0.
    async fn init_abr_partition(
        abr_bytes: Option<[u8; ABR_DATA_SIZE]>,
    ) -> Box<dyn PartitionClient> {
        let bounce = std::num::NonZero::new(64 * 1024).unwrap();
        let client = crate::partition::BlockPartitionClient::new(
            FakeBlockClient::new(TEST_BLOCK_SIZE, TEST_BLOCK_COUNT),
            bounce,
            bounce,
        )
        .await
        .expect("new fake partition");
        let mut block = vec![0x5au8; TEST_BLOCK_SIZE as usize];
        if let Some(bytes) = abr_bytes {
            block[..ABR_DATA_SIZE].copy_from_slice(&bytes);
        } else {
            block[..ABR_DATA_SIZE].fill(0);
        }
        client.write_at(&block, 0).await.expect("write initial block");
        Box::new(client)
    }

    /// Reads the raw [`ABR_DATA_SIZE`]-byte ABR metadata from `client`.
    async fn read_disk_abr_bytes(client: &dyn PartitionClient) -> [u8; ABR_DATA_SIZE] {
        let mut buf = [0u8; ABR_DATA_SIZE];
        client.read_at(&mut buf, 0).await.expect("read abr");
        buf
    }

    /// Verifies that zeroed/uninitialized ABR metadata defaults to slot A pending with 0 boot
    /// attempts used, and checks slot/error helper mappings.
    #[fuchsia::test]
    async fn test_uninitialized_metadata_defaults_to_slot_a_pending() {
        assert_eq!(paver_to_libabr_slot(fpaver::Configuration::Recovery), SlotIndex::R);
        assert_eq!(libabr_to_paver_slot(SlotIndex::R), fpaver::Configuration::Recovery);
        assert_eq!(abr_error_to_status(AbrError::BadMagic), zx::Status::IO_DATA_INTEGRITY);
        assert_eq!(abr_error_to_status(AbrError::BadChecksum), zx::Status::IO_DATA_INTEGRITY);
        assert_eq!(abr_error_to_status(AbrError::InvalidInput), zx::Status::INVALID_ARGS);

        let client = init_abr_partition(None).await;
        let mut bm = new_test_bm(fpaver::Configuration::A, client);

        assert_eq!(bm.query_current_configuration().await, Ok(fpaver::Configuration::A));
        assert_eq!(bm.query_active_configuration().await, Ok(fpaver::Configuration::A));
        assert_eq!(bm.query_configuration_last_set_active().await, Ok(fpaver::Configuration::A));

        let status_a = bm
            .query_configuration_status_and_boot_attempts(fpaver::Configuration::A)
            .await
            .expect("query A");
        assert_eq!(status_a.status, Some(fpaver::ConfigurationStatus::Pending));
        assert_eq!(status_a.boot_attempts, Some(0));
        assert_eq!(status_a.unbootable_reason, None);
    }

    /// Verifies that an unsupported major ABR metadata version fails with `NOT_SUPPORTED` without
    /// clobbering on-disk metadata.
    #[fuchsia::test]
    async fn test_unsupported_version_returns_not_supported() {
        let expected = abr_with_unsupported_major_version();
        let client = init_abr_partition(Some(expected)).await;
        let mut bm = new_test_bm(fpaver::Configuration::A, client);

        assert_eq!(bm.query_configuration_last_set_active().await, Err(zx::Status::NOT_SUPPORTED));
        assert_eq!(
            bm.query_configuration_status(fpaver::Configuration::A).await,
            Err(zx::Status::NOT_SUPPORTED)
        );
        assert_eq!(
            bm.set_configuration_active(fpaver::Configuration::A).await,
            Err(zx::Status::NOT_SUPPORTED)
        );
        assert_eq!(read_disk_abr_bytes(bm.partition()).await, expected);
    }

    /// Verifies active slot vs last-set-active tracking across `set_configuration_active` and
    /// `set_configuration_unbootable`.
    #[fuchsia::test]
    async fn test_query_active_and_last_set_active() {
        let client = init_abr_partition(Some(abr_a_unbootable_b_successful())).await;
        let mut bm = new_test_bm(fpaver::Configuration::B, client);

        assert_eq!(bm.query_active_configuration().await, Ok(fpaver::Configuration::B));
        assert_eq!(bm.query_configuration_last_set_active().await, Ok(fpaver::Configuration::B));

        // Mark A active -> A is now active and last set active.
        bm.set_configuration_active(fpaver::Configuration::A).await.expect("set A active");
        assert_eq!(bm.query_active_configuration().await, Ok(fpaver::Configuration::A));
        assert_eq!(bm.query_configuration_last_set_active().await, Ok(fpaver::Configuration::A));

        // Mark A unbootable -> active reverts to B (since B is still successful), but
        // last_set_active remains A (since A still has higher raw priority).
        bm.set_configuration_unbootable(fpaver::Configuration::A).await.expect("set A unbootable");
        assert_eq!(bm.query_active_configuration().await, Ok(fpaver::Configuration::B));
        assert_eq!(bm.query_configuration_last_set_active().await, Ok(fpaver::Configuration::A));

        // Mark B unbootable -> both slots are unbootable, so query_active_configuration returns
        // NOT_SUPPORTED.
        bm.set_configuration_unbootable(fpaver::Configuration::B).await.expect("set B unbootable");
        assert_eq!(bm.query_active_configuration().await, Err(zx::Status::NOT_SUPPORTED));
    }

    /// Verifies Healthy, Pending (boot_attempts calculation), Recovery rejection, and all
    /// `UnbootableReason` mappings (including forward-compatible unknown reason codes).
    #[fuchsia::test]
    async fn test_query_configuration_status_and_boot_attempts_variants() {
        // 1. Pending slot A (with 1 try remaining after 6 simulated boot attempts -> 6 attempts
        // used), and Healthy slot B.
        let mut data = [0u8; ABR_DATA_SIZE];
        mark_slot_active(&mut data, SlotIndex::A).expect("mark A active");
        for _ in 0..(ABR_MAX_TRIES_REMAINING - 1) {
            assert_eq!(get_boot_slot(&mut data, true), (SlotIndex::A, false));
        }
        let client = init_abr_partition(Some(data)).await;
        let mut bm = new_test_bm(fpaver::Configuration::A, client);

        let status_a = bm
            .query_configuration_status_and_boot_attempts(fpaver::Configuration::A)
            .await
            .expect("query A");
        assert_eq!(status_a.status, Some(fpaver::ConfigurationStatus::Pending));
        assert_eq!(status_a.boot_attempts, Some(ABR_MAX_TRIES_REMAINING - 1));
        assert_eq!(status_a.unbootable_reason, None);

        bm.set_configuration_healthy(fpaver::Configuration::B).await.expect("mark B healthy");
        let status_b = bm
            .query_configuration_status_and_boot_attempts(fpaver::Configuration::B)
            .await
            .expect("query B");
        assert_eq!(status_b.status, Some(fpaver::ConfigurationStatus::Healthy));
        assert_eq!(status_b.boot_attempts, None);
        assert_eq!(status_b.unbootable_reason, None);

        // Recovery is rejected with INVALID_ARGS.
        assert_eq!(
            bm.query_configuration_status(fpaver::Configuration::Recovery).await,
            Err(zx::Status::INVALID_ARGS)
        );

        // 2. Unbootable reasons on non-current slot B when current slot is A.
        for (abr_reason, expected_fidl_reason) in [
            (UnbootableReason::None, fpaver::UnbootableReason::None),
            (UnbootableReason::NoMoreTries, fpaver::UnbootableReason::NoMoreTries),
            (UnbootableReason::OsRequested, fpaver::UnbootableReason::OsRequested),
            (UnbootableReason::VerificationFailure, fpaver::UnbootableReason::VerificationFailure),
            (UnbootableReason::Unknown(0x42), fpaver::UnbootableReason::from(0x42)),
        ] {
            let client = init_abr_partition(Some(abr_both_unbootable(abr_reason))).await;
            let mut bm = new_test_bm(fpaver::Configuration::A, client);
            let res_b = bm
                .query_configuration_status_and_boot_attempts(fpaver::Configuration::B)
                .await
                .expect("query B unbootable");
            assert_eq!(res_b.status, Some(fpaver::ConfigurationStatus::Unbootable));
            assert_eq!(res_b.boot_attempts, None);
            assert_eq!(res_b.unbootable_reason, Some(expected_fidl_reason));
        }
    }

    /// Verifies final-boot-attempt detection (`tries_remaining == 0` on current slot) and allowing
    /// `set_configuration_healthy` on the final boot attempt.
    #[fuchsia::test]
    async fn test_final_boot_attempt_detection_and_marking_healthy() {
        // Both slots unbootable with NoMoreTries while currently booted into Slot B.
        let client =
            init_abr_partition(Some(abr_both_unbootable(UnbootableReason::NoMoreTries))).await;
        let mut bm = new_test_bm(fpaver::Configuration::B, client);

        // Slot B (current slot) reports Pending with MAX_PENDING_BOOT_ATTEMPTS.
        let res_b = bm
            .query_configuration_status_and_boot_attempts(fpaver::Configuration::B)
            .await
            .expect("query B on final boot attempt");
        assert_eq!(res_b.status, Some(fpaver::ConfigurationStatus::Pending));
        assert_eq!(res_b.boot_attempts, Some(fpaver::MAX_PENDING_BOOT_ATTEMPTS));
        assert_eq!(res_b.unbootable_reason, None);

        // Slot A (non-current slot) reports Unbootable with NoMoreTries.
        let res_a = bm
            .query_configuration_status_and_boot_attempts(fpaver::Configuration::A)
            .await
            .expect("query A");
        assert_eq!(res_a.status, Some(fpaver::ConfigurationStatus::Unbootable));
        assert_eq!(res_a.unbootable_reason, Some(fpaver::UnbootableReason::NoMoreTries));

        // Marking non-current unbootable Slot A healthy fails with INVALID_ARGS.
        assert_eq!(
            bm.set_configuration_healthy(fpaver::Configuration::A).await,
            Err(zx::Status::INVALID_ARGS)
        );

        // Marking current Slot B healthy on its final boot attempt succeeds!
        bm.set_configuration_healthy(fpaver::Configuration::B)
            .await
            .expect("mark B healthy on final attempt");
        assert_eq!(
            bm.query_configuration_status(fpaver::Configuration::B).await,
            Ok(fpaver::ConfigurationStatus::Healthy)
        );
    }

    /// Verifies priority rollover when marking active, clearing the inactive slot's `successful`
    /// flag when marking healthy, and preserving bytes beyond `ABR_DATA_SIZE` in block 0.
    #[fuchsia::test]
    async fn test_set_configuration_active_rollover_and_healthy_clears_other_slot() {
        let client = init_abr_partition(Some(abr_a_unbootable_b_successful())).await;
        let mut bm = new_test_bm(fpaver::Configuration::A, client);

        // Marking A active when B was previously active lowers B's priority below A while keeping
        // B's state Successful until A is marked healthy.
        bm.set_configuration_active(fpaver::Configuration::A).await.expect("set A active");
        let mut disk_abr = read_disk_abr_bytes(bm.partition()).await;
        assert_eq!(get_slot_last_marked_active(&mut disk_abr), Ok(SlotIndex::A));
        assert_eq!(
            get_slot_info(&mut disk_abr, SlotIndex::A),
            Ok(SlotInfo { state: SlotState::Bootable(ABR_MAX_TRIES_REMAINING), is_active: true })
        );
        assert_eq!(
            get_slot_info(&mut disk_abr, SlotIndex::B),
            Ok(SlotInfo { state: SlotState::Successful, is_active: false })
        );

        // Marking A healthy clears B's successful_boot and resets B's tries_remaining to max.
        bm.set_configuration_healthy(fpaver::Configuration::A).await.expect("set A healthy");
        let mut disk_abr = read_disk_abr_bytes(bm.partition()).await;
        assert_eq!(
            get_slot_info(&mut disk_abr, SlotIndex::A),
            Ok(SlotInfo { state: SlotState::Successful, is_active: true })
        );
        assert_eq!(
            get_slot_info(&mut disk_abr, SlotIndex::B),
            Ok(SlotInfo { state: SlotState::Bootable(ABR_MAX_TRIES_REMAINING), is_active: false })
        );

        // Verify surrounding bytes in block 0 outside ABR_DATA_SIZE were preserved.
        let mut full_block = vec![0u8; TEST_BLOCK_SIZE as usize];
        PartitionClient::read_at(bm.partition(), &mut full_block, 0)
            .await
            .expect("read full block");
        assert_eq!(
            &full_block[ABR_DATA_SIZE..],
            &[0x5au8; TEST_BLOCK_SIZE as usize - ABR_DATA_SIZE]
        );
    }

    /// Verifies setting and clearing one-shot recovery and one-shot bootloader flags.
    #[fuchsia::test]
    async fn test_one_shot_flags() {
        let client = init_abr_partition(Some(abr_a_unbootable_b_successful())).await;
        let mut bm = new_test_bm(fpaver::Configuration::B, client);

        assert_eq!(bm.get_and_clear_one_shot_flags().await, Ok(0));

        bm.set_one_shot_recovery().await.expect("set one shot recovery");
        assert_eq!(bm.get_and_clear_one_shot_flags().await, Ok(ONE_SHOT_RECOVERY));
        assert_eq!(bm.get_and_clear_one_shot_flags().await, Ok(0));

        bm.set_one_shot_bootloader().await.expect("set one shot bootloader");
        bm.set_one_shot_recovery().await.expect("set one shot recovery");
        assert_eq!(
            bm.get_and_clear_one_shot_flags().await,
            Ok(ONE_SHOT_RECOVERY | ONE_SHOT_BOOTLOADER)
        );
        assert_eq!(bm.get_and_clear_one_shot_flags().await, Ok(0));
    }

    /// Verifies negative final-boot-attempt cases (`VerificationFailure`, `OsRequested`),
    /// `Recovery` rejection on mutations, unsupported-version preservation, corrupt CRC32
    /// self-healing, storage I/O error normalization, [`FixedOffsetPartitionClient`] composition,
    /// and concurrent RMW/read/flush serialization.
    #[fuchsia::test]
    async fn test_abr_negative_cases_io_errors_fixed_offset_and_concurrent_rmw() {
        use crate::partition::tests::FakePartitionClient;
        use std::cell::{Cell, RefCell};

        // 1. Current slot with UnbootableReason::VerificationFailure or OsRequested is NOT treated
        // as a final boot attempt: reports Unbootable and rejects set_configuration_healthy with
        // INVALID_ARGS while leaving the slot unbootable on disk.
        for reason in [UnbootableReason::VerificationFailure, UnbootableReason::OsRequested] {
            let client = init_abr_partition(Some(abr_both_unbootable(reason))).await;
            let mut bm = new_test_bm(fpaver::Configuration::A, client);
            assert_eq!(
                bm.query_configuration_status(fpaver::Configuration::A).await,
                Ok(fpaver::ConfigurationStatus::Unbootable)
            );
            assert_eq!(
                bm.set_configuration_healthy(fpaver::Configuration::A).await,
                Err(zx::Status::INVALID_ARGS)
            );
            let mut disk_abr = read_disk_abr_bytes(bm.partition()).await;
            assert_eq!(
                get_slot_info(&mut disk_abr, SlotIndex::A),
                Ok(SlotInfo { state: SlotState::Unbootable(reason), is_active: false })
            );
        }

        // 2. Recovery is rejected with INVALID_ARGS on all slot-mutating methods.
        let client = init_abr_partition(Some(abr_a_unbootable_b_successful())).await;
        let mut bm = new_test_bm(fpaver::Configuration::B, client);
        assert_eq!(
            bm.set_configuration_active(fpaver::Configuration::Recovery).await,
            Err(zx::Status::INVALID_ARGS)
        );
        assert_eq!(
            bm.set_configuration_unbootable(fpaver::Configuration::Recovery).await,
            Err(zx::Status::INVALID_ARGS)
        );
        assert_eq!(
            bm.set_configuration_healthy(fpaver::Configuration::Recovery).await,
            Err(zx::Status::INVALID_ARGS)
        );

        // 3. Unsupported version rejects all remaining mutating methods with NOT_SUPPORTED and
        // leaves on-disk bytes untouched.
        let expected_raw = abr_with_unsupported_major_version();
        let client = init_abr_partition(Some(expected_raw)).await;
        let mut bm = new_test_bm(fpaver::Configuration::A, client);
        assert_eq!(
            bm.set_configuration_unbootable(fpaver::Configuration::A).await,
            Err(zx::Status::NOT_SUPPORTED)
        );
        assert_eq!(
            bm.set_configuration_healthy(fpaver::Configuration::B).await,
            Err(zx::Status::NOT_SUPPORTED)
        );
        assert_eq!(bm.set_one_shot_recovery().await, Err(zx::Status::NOT_SUPPORTED));
        assert_eq!(bm.set_one_shot_bootloader().await, Err(zx::Status::NOT_SUPPORTED));
        assert_eq!(bm.get_and_clear_one_shot_flags().await, Err(zx::Status::NOT_SUPPORTED));
        let disk_raw = read_disk_abr_bytes(bm.partition()).await;
        assert_eq!(disk_raw, expected_raw);

        // 4. Corrupt CRC32 self-heals on read and persists valid metadata on mutation.
        let mut corrupt = abr_a_unbootable_b_successful();
        corrupt[ABR_DATA_SIZE - 1] ^= 0xff;
        let corrupt_client = FakePartitionClient::new(512, 512, 0x00);
        corrupt_client.write_at(&corrupt, 0).await.expect("write corrupt");
        let mut corrupt_bm =
            new_test_bm(fpaver::Configuration::A, Box::new(corrupt_client.clone()));
        assert_eq!(corrupt_bm.query_active_configuration().await, Ok(fpaver::Configuration::A));
        corrupt_bm
            .set_configuration_active(fpaver::Configuration::B)
            .await
            .expect("heal and set B");
        let mut healed = read_disk_abr_bytes(&corrupt_client).await;
        assert_eq!(get_slot_last_marked_active(&mut healed), Ok(SlotIndex::B));
        assert_eq!(
            get_slot_info(&mut healed, SlotIndex::B),
            Ok(SlotInfo { state: SlotState::Bootable(ABR_MAX_TRIES_REMAINING), is_active: true })
        );

        // 5. FixedOffsetPartitionClient (e.g. Nelson 2048-byte offset in `misc`) preserves prefix
        // bytes `[0..2048)` while reading/writing ABR metadata at `[2048..2080)`.
        let misc = FakePartitionClient::new(4096, 512, 0xcd);
        let shifted = misc.clone().with_offset(2048);
        let mut nelson_bm = new_test_bm(fpaver::Configuration::A, Box::new(shifted));
        nelson_bm.set_configuration_active(fpaver::Configuration::B).await.expect("set B active");
        assert_eq!(misc.flushes(), 1);
        nelson_bm.flush().await.expect("flush");
        assert_eq!(misc.flushes(), 2);
        let misc_bytes = misc.bytes();
        assert_eq!(&misc_bytes[..2048], &[0xcdu8; 2048]);
        let mut nelson_abr_raw: [u8; ABR_DATA_SIZE] =
            misc_bytes[2048..2048 + ABR_DATA_SIZE].try_into().unwrap();
        assert_eq!(get_slot_last_marked_active(&mut nelson_abr_raw), Ok(SlotIndex::B));
        assert_eq!(
            get_slot_info(&mut nelson_abr_raw, SlotIndex::B),
            Ok(SlotInfo { state: SlotState::Bootable(ABR_MAX_TRIES_REMAINING), is_active: true })
        );

        // 6. Storage I/O errors normalize to `zx::Status::IO`.
        struct FailingAbrClient {
            fail_read: bool,
        }
        #[async_trait(?Send)]
        impl PartitionClient for FailingAbrClient {
            fn partition_size(&self) -> Result<u64, zx::Status> {
                Ok(512)
            }
            async fn read_vmo(
                &self,
                vmo: &zx::Vmo,
                vmo_offset: u64,
                _dev_offset: u64,
                size: u64,
            ) -> Result<(), zx::Status> {
                if self.fail_read {
                    Err(zx::Status::PEER_CLOSED)
                } else {
                    vmo.write(&vec![0u8; size as usize], vmo_offset)
                }
            }
            async fn write_vmo(
                &self,
                _vmo: &zx::Vmo,
                _vmo_offset: u64,
                _dev_offset: u64,
                _size: u64,
            ) -> Result<(), zx::Status> {
                Err(zx::Status::NO_RESOURCES)
            }
        }
        let mut fail_read_bm =
            new_test_bm(fpaver::Configuration::A, Box::new(FailingAbrClient { fail_read: true }));
        assert_eq!(fail_read_bm.query_active_configuration().await, Err(zx::Status::IO));
        let mut fail_write_bm =
            new_test_bm(fpaver::Configuration::A, Box::new(FailingAbrClient { fail_read: false }));
        assert_eq!(
            fail_write_bm.set_configuration_active(fpaver::Configuration::B).await,
            Err(zx::Status::IO)
        );

        // 7. Concurrent serialization across `BootManager` instances sharing an `Rc<Mutex<()>>`:
        // deterministic sync points pause `bm1` mid-`write_vmo` (before `flush`) and verify `bm3`
        // blocks on `shared_lock` rather than observing un-flushed state, while `bm2` and `bm4`
        // serialize cleanly without clobbering updates.
        async fn yield_once() {
            let mut yielded = false;
            futures::future::poll_fn(|cx| {
                if std::mem::replace(&mut yielded, true) {
                    std::task::Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;
        }

        #[derive(Clone)]
        struct YieldingSharedPartition {
            inner: FakePartitionClient,
            reads: Rc<Cell<usize>>,
            first_write_started_tx: Rc<RefCell<Option<futures::channel::oneshot::Sender<()>>>>,
            allow_first_write_finish_rx:
                Rc<RefCell<Option<futures::channel::oneshot::Receiver<()>>>>,
        }
        #[async_trait(?Send)]
        impl PartitionClient for YieldingSharedPartition {
            fn partition_size(&self) -> Result<u64, zx::Status> {
                self.inner.partition_size()
            }
            async fn read_vmo(
                &self,
                vmo: &zx::Vmo,
                vmo_offset: u64,
                dev_offset: u64,
                size: u64,
            ) -> Result<(), zx::Status> {
                self.reads.set(self.reads.get() + 1);
                self.inner.read_vmo(vmo, vmo_offset, dev_offset, size).await?;
                yield_once().await;
                Ok(())
            }
            async fn write_vmo(
                &self,
                vmo: &zx::Vmo,
                vmo_offset: u64,
                dev_offset: u64,
                size: u64,
            ) -> Result<(), zx::Status> {
                self.inner.write_vmo(vmo, vmo_offset, dev_offset, size).await?;
                if let Some(tx) = self.first_write_started_tx.borrow_mut().take() {
                    let _ = tx.send(());
                }
                let rx = self.allow_first_write_finish_rx.borrow_mut().take();
                if let Some(rx) = rx {
                    let _ = rx.await;
                } else {
                    yield_once().await;
                }
                Ok(())
            }
            async fn flush(&self) -> Result<(), zx::Status> {
                self.inner.flush().await
            }
        }

        let (first_write_started_tx, first_write_started_rx) = futures::channel::oneshot::channel();
        let (allow_first_write_finish_tx, allow_first_write_finish_rx) =
            futures::channel::oneshot::channel();
        let shared = YieldingSharedPartition {
            inner: FakePartitionClient::new(512, 512, 0x00),
            reads: Rc::new(Cell::new(0)),
            first_write_started_tx: Rc::new(RefCell::new(Some(first_write_started_tx))),
            allow_first_write_finish_rx: Rc::new(RefCell::new(Some(allow_first_write_finish_rx))),
        };
        let shared_lock = Rc::new(Mutex::new(()));
        let mut bm1 = AbrBootManager::new(
            fpaver::Configuration::A,
            Box::new(shared.clone()),
            Rc::clone(&shared_lock),
        );
        let mut bm2 = AbrBootManager::new(
            fpaver::Configuration::A,
            Box::new(shared.clone()),
            Rc::clone(&shared_lock),
        );
        let mut bm3 = AbrBootManager::new(
            fpaver::Configuration::A,
            Box::new(shared.clone()),
            Rc::clone(&shared_lock),
        );
        let mut bm4 = AbrBootManager::new(
            fpaver::Configuration::A,
            Box::new(shared.clone()),
            Rc::clone(&shared_lock),
        );
        let (r1, r2, r3, r4) = futures::join!(
            bm1.set_configuration_active(fpaver::Configuration::B),
            bm2.set_one_shot_recovery(),
            async {
                // Wait until `bm1` has written Slot B active to `inner` (before `flush`).
                first_write_started_rx.await.expect("bm1 entered write_vmo");
                assert_eq!(shared.inner.flushes(), 0);
                let mut query_fut = std::pin::pin!(bm3.query_active_configuration());
                assert!(futures::poll!(&mut query_fut).is_pending());
                allow_first_write_finish_tx.send(()).expect("release bm1 write_vmo");
                assert_eq!(query_fut.await, Ok(fpaver::Configuration::B));
                // `bm1`'s `with_ops` must have completed its flush before `bm3`'s read ran.
                assert!(shared.inner.flushes() >= 1);
            },
            bm4.flush(),
        );
        r1.expect("bm1 set B active");
        r2.expect("bm2 set one shot recovery");
        let () = r3;
        r4.expect("bm4 flush");
        let mut final_abr = read_disk_abr_bytes(&shared.inner).await;
        assert_eq!(get_slot_last_marked_active(&mut final_abr), Ok(SlotIndex::B));
        assert_eq!(get_and_clear_one_shot_flag(&mut final_abr), Ok(ONE_SHOT_RECOVERY));
    }
}
