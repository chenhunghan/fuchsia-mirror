// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Shared test fixtures: the fake UFS host controller and helpers for driving a
//! [`TransferRequestProcessor`] against it. Tests live alongside the code they exercise.

pub mod fake_ufs;

use crate::DriverMmio;
/// The admin command slot.
pub use crate::constants::ADMIN_SLOT;
use crate::constants::{
    PA_AVAILRXDATALANES, PA_AVAILTXDATALANES, PA_MAXRXHSGEAR, PA_PWRMODE,
    START_STOP_POWER_CONDITION_SHIFT, UFS_DEVICE_WLUN,
};
use crate::controller::UfsController;
use crate::device_manager::DeviceManager;
use crate::power::PowerControl;
use crate::query::{Attribute, Flag, QueryOpcode, QueryUpiu, RefClkFreq};
use crate::quirks::HostQuirks;
use crate::registers::{
    HostControllerStatusReg, InterruptStatusReg, UicCommandArgument1Reg, UicCommandArgument3Reg,
};
use crate::request_list::{OverallCommandStatus, SlotState};
use crate::scsi::{ScsiOpcode, ScsiStatus, StartStopPowerCondition};
use crate::transfer_request_processor::{IoCompletion, ScsiOutcome, TransferRequestProcessor};
use crate::uic::UicCommandOpcode;
use crate::upiu::{
    CommandUpiu, NopInUpiu, NopOutUpiu, ResponseUpiu, UpiuHeader, UpiuTransactionCode,
};
use fake_bti::FakeBti;
use fake_ufs::{
    FakeMmio, FakeUfs, FakeUtp, HCS_ACTIVE_LINK, complete_uic_command, uic_hardware, uic_opcode,
    wait_until,
};
use fidl::endpoints::create_proxy_and_stream;
use fidl_fuchsia_hardware_ufs_phy as fufs_phy;
use fuchsia_async as fasync;
use fuchsia_inspect::reader::{ArrayContent, DiagnosticsHierarchy, Property};
use fuchsia_sync::Mutex;
use futures::TryStreamExt;
use futures::channel::oneshot;
use mmio::{ReadableRegister, Register};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;
use zerocopy::IntoBytes;
use zerocopy::byteorder::big_endian::{U16, U32};

/// A processor over the fake register block.
pub type TestProcessor = TransferRequestProcessor<FakeMmio>;

/// Register block size shared by the single-doorbell unit tests.
pub const MMIO_SIZE: usize = 4096;
/// Page size assumed for test data VMOs.
pub const PAGE_SIZE: u64 = 4096;
/// Overall Command Status value reported by the fake for a successful request.
pub const OCS_SUCCESS: u8 = OverallCommandStatus::Success as u8;

/// Creates an initialized 32-slot processor and the VMO backing its registers.
pub fn create_test_processor() -> (zx::Vmo, Arc<TestProcessor>) {
    create_test_processor_inner(None)
}

/// Creates a [`PowerControl`] backed by `processor` with default test quirks and no PHY/hooks, so
/// unit tests can acquire real [`crate::power::PowerGuard`] handles via [`PowerControl::lock`].
pub fn test_power(processor: &Arc<TestProcessor>) -> PowerControl<FakeMmio, ()> {
    PowerControl::new(DeviceManager::new(HostQuirks::default()), Arc::clone(processor), None, ())
}

/// Creates an initialized 32-slot processor backed by a power-domain-gated [`FakeMmio`].
pub fn create_test_processor_with_power_domain(
    power_domain: Arc<AtomicBool>,
) -> (zx::Vmo, Arc<TestProcessor>) {
    create_test_processor_inner(Some(power_domain))
}

fn create_test_processor_inner(
    power_domain: Option<Arc<AtomicBool>>,
) -> (zx::Vmo, Arc<TestProcessor>) {
    let bti = FakeBti::create().expect("Failed to create fake bti");
    let vmo = zx::Vmo::create(MMIO_SIZE as u64).unwrap();
    vmo.write(&HCS_ACTIVE_LINK.to_le_bytes(), HostControllerStatusReg::OFFSET as u64).unwrap();
    let mmio = match power_domain {
        Some(power_domain) => FakeMmio::new(&vmo, MMIO_SIZE).with_power_domain(power_domain),
        None => FakeMmio::new(&vmo, MMIO_SIZE),
    };
    let controller = Arc::new(UfsController::new(mmio));
    let processor = Arc::new(TransferRequestProcessor::new(&bti, controller).unwrap());
    processor.init().unwrap();
    (vmo, processor)
}

/// Maps the fake hardware view of `processor`'s registers and descriptors. Completions raised
/// through [`FakeUtp::complete`] are delivered to `processor` by interrupt.
pub fn fake_utp(vmo: &zx::Vmo, processor: &Arc<TestProcessor>) -> FakeUtp {
    FakeUtp::new(vmo, MMIO_SIZE, processor)
}

/// Returns the state of `slot`.
pub fn slot_state(processor: &TestProcessor, slot: u8) -> SlotState {
    processor.request_list().lock().slot(slot).unwrap().state
}

/// Returns whether `slot` currently holds a pinned client buffer.
pub fn slot_pinned(processor: &TestProcessor, slot: u8) -> bool {
    processor.request_list().lock().slot(slot).unwrap().pmt.is_some()
}

/// Moves `slot`'s deadline into the past so the timeout watchdog inspects it on its next pass.
pub fn expire_deadline(processor: &TestProcessor, slot: u8) {
    let now = std::panic::catch_unwind(|| fasync::MonotonicInstant::now().into_zx())
        .unwrap_or_else(|_| zx::MonotonicInstant::get());
    processor.request_list().lock().slot_mut(slot).unwrap().deadline =
        Some(now - zx::MonotonicDuration::from_seconds(1));
}

/// Receives the outcome delivered to an [`io_callback`].
pub type IoOutcomeReceiver = oneshot::Receiver<ScsiOutcome>;

/// A oneshot [`IoCompletion`] and the receiver that collects its delivered outcome.
pub fn io_callback() -> (IoCompletion, IoOutcomeReceiver) {
    let (tx, rx) = oneshot::channel();
    (IoCompletion::Oneshot(tx), rx)
}

/// Returns the outcome already delivered to `rx`, panicking if none was.
pub fn delivered(rx: &mut IoOutcomeReceiver) -> Result<(), zx::Status> {
    rx.try_recv().expect("Completion dropped").expect("Completion not delivered").io_status()
}

/// NOP IN response for `task_tag`.
pub fn nop_in(task_tag: u8) -> NopInUpiu {
    let mut response = NopOutUpiu::new();
    response.header.trans_type = UpiuTransactionCode::NopIn as u8;
    response.header.task_tag = task_tag;
    response
}

/// RESPONSE UPIU for `task_tag` with SCSI `status`.
pub fn scsi_response(task_tag: u8, status: u8) -> ResponseUpiu {
    ResponseUpiu {
        header: UpiuHeader {
            trans_type: UpiuTransactionCode::Response as u8,
            task_tag,
            status,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// CHECK CONDITION response with fixed-format sense data.
pub fn check_condition(task_tag: u8, sense_key: u8) -> ResponseUpiu {
    let mut response = scsi_response(task_tag, ScsiStatus::CheckCondition.as_u8());
    response.sense_data_length = U16::new(18);
    response.sense_data[0] = 0x70;
    response.sense_data[2] = sense_key;
    response.sense_data[7] = 10;
    response
}

/// Yields to the executor until `condition` holds, for up to [`fake_ufs::FAKE_HW_TIMEOUT`]. Unlike
/// [`fake_ufs::wait_until`] this lets tasks sharing the test executor (the timeout task,
/// interrupt loops) make progress.
pub async fn wait_async(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = zx::MonotonicInstant::after(fake_ufs::FAKE_HW_TIMEOUT.into());
    while !condition() {
        if zx::MonotonicInstant::get() > deadline {
            return false;
        }
        fasync::Timer::new(zx::MonotonicDuration::from_millis(1)).await;
    }
    true
}

/// Returns the lowest slot whose doorbell bit is set, waiting for one.
pub fn wait_for_any_doorbell(utp: &FakeUtp) -> u8 {
    assert!(wait_until(|| utp.doorbell() != 0), "No doorbell rung");
    utp.doorbell().trailing_zeros() as u8
}

/// A device manager populated with `fake_ufs`'s device and geometry descriptors.
pub fn create_test_device_manager(fake_ufs: &FakeUfs) -> DeviceManager {
    let mut device_manager = DeviceManager::new(HostQuirks::default());
    device_manager.set_descriptors(fake_ufs.device_desc, fake_ufs.geometry_desc);
    device_manager
}

/// UIC commands issued by `DeviceManager::init_uic_power_mode` for an HS-G4 link: 3 `DME_GET`s, 1
/// `DME_PEER_GET`, 17 `DME_SET`s (HS-G4 adds `PA_TxHsAdaptType`) and the `PA_PWRMode` set.
pub const POWER_MODE_CHANGE_UIC_COMMANDS_G4: usize = 22;

/// Link capabilities for [`fake_uic_device`]: 2 lanes, local HS-G`local_gear` and peer
/// HS-G`peer_gear`.
pub fn link_capabilities(
    local_gear: u32,
    peer_gear: u32,
) -> impl Fn(u32, u16) -> u32 + Send + 'static {
    move |opcode, attribute| match attribute {
        PA_AVAILTXDATALANES | PA_AVAILRXDATALANES => 2,
        PA_MAXRXHSGEAR if opcode == UicCommandOpcode::DmePeerGet as u32 => peer_gear,
        PA_MAXRXHSGEAR => local_gear,
        _ => 0,
    }
}

/// A UIC command (`UICCMD`, `UICCMDARG1`, `UICCMDARG3`) seen by [`fake_uic_device`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UicRecord {
    pub opcode: u32,
    pub attribute: u16,
    pub arg3: u32,
}

/// Answers `count` UIC commands on `vmo`, returning `answer(opcode, attribute)` in `UICCMDARG3`. A
/// `PA_PWRMode` `DME_SET` also sets `IS.UPMS`. `HCS` is set to `hcs` (plus `UCRDY`).
pub fn fake_uic_device(
    vmo: &zx::Vmo,
    hcs: u32,
    count: usize,
    answer: impl Fn(u32, u16) -> u32 + Send + 'static,
) -> thread::JoinHandle<Vec<UicRecord>> {
    let mut regs = uic_hardware(vmo, hcs);
    thread::spawn(move || {
        let mut records = Vec::new();
        for _ in 0..count {
            assert!(wait_until(|| uic_opcode(&regs) != 0), "Only {} UIC commands", records.len());
            let opcode = uic_opcode(&regs);
            let attribute = UicCommandArgument1Reg::read(&regs).mib_attribute() as u16;
            let arg3 = UicCommandArgument3Reg::read(&regs).value();
            records.push(UicRecord { opcode, attribute, arg3 });
            let power_mode = opcode == UicCommandOpcode::DmeSet as u32 && attribute == PA_PWRMODE;
            complete_uic_command(&mut regs, 0, answer(opcode, attribute), |is| {
                if power_mode {
                    is.set_uic_power_mode_status(true);
                }
            });
        }
        records
    })
}

/// Answers `count` admin queries, letting `respond` edit each response (a copy of the request).
pub fn answer_queries(
    mut utp: FakeUtp,
    count: usize,
    respond: impl Fn(&mut QueryUpiu) + Send + 'static,
) -> thread::JoinHandle<Vec<QueryUpiu>> {
    thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..count {
            assert!(utp.wait_for_doorbell(ADMIN_SLOT));
            let request: QueryUpiu = utp.request(ADMIN_SLOT);
            let mut response = request;
            response.header.trans_type = UpiuTransactionCode::QueryResponse as u8;
            respond(&mut response);
            utp.complete(ADMIN_SLOT, response.as_bytes(), OCS_SUCCESS);
            requests.push(request);
        }
        requests
    })
}
/// Fake `UfsPhy` handle: proxy, recorded `SetPowerState` transitions, and server task.
pub type FakePhy = (fufs_phy::UfsPhyProxy, Arc<Mutex<Vec<fufs_phy::PowerState>>>, fasync::Task<()>);

/// Spawns a fake `UfsPhy` that records every `SetPowerState` and fails the first `fail_active`
/// requests for `PowerState::Active` with `ZX_ERR_IO`. `Init` and `PrePowerModeChange` always
/// succeed.
pub fn fake_phy(fail_active: usize) -> FakePhy {
    let (phy_proxy, mut phy_stream) = create_proxy_and_stream::<fufs_phy::UfsPhyMarker>();
    let phy_calls = Arc::new(Mutex::new(Vec::<fufs_phy::PowerState>::new()));
    let phy_calls_clone = Arc::clone(&phy_calls);
    let phy_task = fasync::Task::spawn(async move {
        let mut active_failures_left = fail_active;
        while let Some(req) = phy_stream.try_next().await.unwrap() {
            match req {
                fufs_phy::UfsPhyRequest::SetPowerState { state, responder } => {
                    phy_calls_clone.lock().push(state);
                    let fail = state == fufs_phy::PowerState::Active && active_failures_left > 0;
                    if fail {
                        active_failures_left -= 1;
                    }
                    let result = if fail { Err(zx::Status::IO.into_raw()) } else { Ok(()) };
                    responder.send(result).unwrap();
                }
                fufs_phy::UfsPhyRequest::Init { responder, .. } => {
                    responder.send(Ok(())).unwrap();
                }
                fufs_phy::UfsPhyRequest::PrePowerModeChange { responder } => {
                    responder.send(Ok(())).unwrap();
                }
                other => panic!("Unexpected UfsPhy request: {other:?}"),
            }
        }
    });
    (phy_proxy, phy_calls, phy_task)
}

/// Returns the total sample count in exponential histogram `name` under `/ufs` in `hierarchy`.
pub fn ufs_histogram_samples(hierarchy: &DiagnosticsHierarchy, name: &str) -> u64 {
    match hierarchy.get_child("ufs").and_then(|n| n.get_property(name)) {
        Some(Property::UintArray(_, ArrayContent::ExponentialHistogram(hist))) => {
            hist.counts.iter().sum()
        }
        other => panic!("unexpected `{name}` property: {other:?}"),
    }
}

/// Answers the admin-slot query rung on `utp` with `value`, checking `opcode` and `idn`.
pub fn answer_admin_query_val(utp: &mut FakeUtp, opcode: QueryOpcode, idn: u8, value: u32) {
    assert!(utp.wait_for_doorbell(ADMIN_SLOT), "admin query ({opcode:?}, {idn:#x}) not issued");
    let mut q: QueryUpiu = utp.request(ADMIN_SLOT);
    assert_eq!(q.opcode, opcode as u8);
    assert_eq!(q.idn, idn);
    q.header.trans_type = UpiuTransactionCode::QueryResponse as u8;
    q.value = U32::new(value);
    utp.complete(ADMIN_SLOT, q.as_bytes(), OCS_SUCCESS);
}

/// Rejects the admin-slot query rung on `utp` with a non-zero Query Response, checking `opcode`
/// and `idn`.
pub fn reject_admin_query(utp: &mut FakeUtp, opcode: QueryOpcode, idn: u8) {
    // Query Response "General failure" (UFS 4.0 section 10.7.9, Table 10.47).
    const QUERY_RESPONSE_GENERAL_FAILURE: u8 = 0xFF;
    assert!(utp.wait_for_doorbell(ADMIN_SLOT), "admin query ({opcode:?}, {idn:#x}) not issued");
    let mut q: QueryUpiu = utp.request(ADMIN_SLOT);
    assert_eq!(q.opcode, opcode as u8);
    assert_eq!(q.idn, idn);
    q.header.trans_type = UpiuTransactionCode::QueryResponse as u8;
    q.header.response = QUERY_RESPONSE_GENERAL_FAILURE;
    utp.complete(ADMIN_SLOT, q.as_bytes(), OCS_SUCCESS);
}

/// Answers the admin-slot query rung on `utp` with `0`, checking `opcode` and `idn`.
pub fn answer_admin_query(utp: &mut FakeUtp, opcode: QueryOpcode, idn: u8) {
    answer_admin_query_val(utp, opcode, idn, 0);
}

/// Answers the `START STOP UNIT` (`condition`) rung on the admin slot of `utp` for the Device
/// W-LUN.
pub fn answer_start_stop_unit(utp: &mut FakeUtp, condition: StartStopPowerCondition) {
    assert!(
        utp.wait_for_doorbell(ADMIN_SLOT),
        "START STOP UNIT ({condition:?}) not issued on ADMIN_SLOT"
    );
    let cmd: CommandUpiu = utp.request(ADMIN_SLOT);
    assert_eq!(cmd.header.lun, UFS_DEVICE_WLUN);
    assert_eq!(cmd.cdb[0], ScsiOpcode::StartStopUnit as u8);
    assert_eq!(cmd.cdb[4] >> START_STOP_POWER_CONDITION_SHIFT, condition as u8);
    utp.complete(ADMIN_SLOT, scsi_response(ADMIN_SLOT, 0).as_bytes(), OCS_SUCCESS);
}

/// Answers the `TEST UNIT READY` rung on the admin slot of `utp` for `expected_lun`.
pub fn answer_tur(utp: &mut FakeUtp, expected_lun: u8) {
    assert!(
        utp.wait_for_doorbell(ADMIN_SLOT),
        "TEST UNIT READY (lun {expected_lun:#x}) not issued on ADMIN_SLOT"
    );
    let cmd: CommandUpiu = utp.request(ADMIN_SLOT);
    assert_eq!(cmd.header.lun, expected_lun);
    assert_eq!(cmd.cdb[0], ScsiOpcode::TestUnitReady as u8);
    utp.complete(ADMIN_SLOT, scsi_response(ADMIN_SLOT, 0).as_bytes(), OCS_SUCCESS);
}

/// Answers the UIC command `opcode` once issued, setting `extra` in `IS`.
pub fn answer_uic(
    uic_regs: &mut DriverMmio,
    opcode: UicCommandOpcode,
    extra: fn(&mut InterruptStatusReg),
) {
    assert!(wait_until(|| uic_opcode(uic_regs) == opcode as u32), "{opcode:?} not issued");
    complete_uic_command(uic_regs, 0, 0, extra);
}

/// Answers a system suspend sequence (`ClearFlag(BackgroundOpsEn)`, `START STOP UNIT(condition)`,
/// `DME_HIBERNATE_ENTER`).
pub fn answer_system_suspend(
    utp: &mut FakeUtp,
    uic_regs: &mut DriverMmio,
    condition: StartStopPowerCondition,
) {
    answer_admin_query(utp, QueryOpcode::ClearFlag, Flag::BackgroundOpsEn as u8);
    answer_start_stop_unit(utp, condition);
    answer_uic(uic_regs, UicCommandOpcode::DmeHibernateEnter, |is| {
        is.set_uic_hibernate_enter_status(true);
    });
}

/// Completes one full `PowerDown` controller, link, and device re-initialization sequence.
pub fn complete_power_down_reinit(utp: &mut FakeUtp, uic_regs: &mut DriverMmio) {
    answer_link_startup_and_nop_out(utp, uic_regs);
    complete_reinit_after_nop_out(utp, uic_regs);
}

/// Answers the `DME_LINKSTARTUP` and `NOP OUT` that open a re-initialization sequence.
pub fn answer_link_startup_and_nop_out(utp: &mut FakeUtp, uic_regs: &mut DriverMmio) {
    answer_uic(uic_regs, UicCommandOpcode::DmeLinkStartup, |_| {});
    assert!(utp.wait_for_doorbell(ADMIN_SLOT), "NOP OUT not issued");
    utp.complete(ADMIN_SLOT, nop_in(ADMIN_SLOT).as_bytes(), OCS_SUCCESS);
}

/// Answers the rest of a re-initialization sequence after [`answer_link_startup_and_nop_out`].
pub fn complete_reinit_after_nop_out(utp: &mut FakeUtp, uic_regs: &mut DriverMmio) {
    answer_admin_query(utp, QueryOpcode::SetFlag, Flag::DeviceInit as u8);
    answer_admin_query(utp, QueryOpcode::ReadFlag, Flag::DeviceInit as u8);
    answer_start_stop_unit(utp, StartStopPowerCondition::Active);
    answer_admin_query_val(
        utp,
        QueryOpcode::ReadAttribute,
        Attribute::RefClkFreq as u8,
        RefClkFreq::Freq38_4MHz as u32,
    );
    loop {
        assert!(wait_until(|| uic_opcode(uic_regs) != 0), "UIC power-mode command not issued");
        let opcode = uic_opcode(uic_regs);
        let attr = UicCommandArgument1Reg::read(uic_regs).mib_attribute() as u16;
        let value = match attr {
            PA_AVAILTXDATALANES | PA_AVAILRXDATALANES => 2,
            PA_MAXRXHSGEAR => 4,
            _ => 0,
        };
        let is_pwr_mode = opcode == UicCommandOpcode::DmeSet as u32 && attr == PA_PWRMODE;
        complete_uic_command(uic_regs, 0, value, |is| {
            if is_pwr_mode {
                is.set_uic_power_mode_status(true);
            }
        });
        if is_pwr_mode {
            break;
        }
    }
    answer_admin_query(utp, QueryOpcode::WriteAttribute, Attribute::ActiveIccLevel as u8);
    answer_tur(utp, UFS_DEVICE_WLUN);
    answer_admin_query(utp, QueryOpcode::SetFlag, Flag::BackgroundOpsEn as u8);
}
