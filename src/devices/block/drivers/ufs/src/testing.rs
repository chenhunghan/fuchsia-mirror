// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Shared test fixtures: the fake UFS host controller and helpers for driving a
//! [`TransferRequestProcessor`] against it. Tests live alongside the code they exercise.

pub mod fake_ufs;

use crate::constants::{ADMIN_COMMAND_SLOT_NUMBER, PA_PWRMODE};
use crate::controller::UfsController;
use crate::device_manager::{DeviceManager, Quirks};
use crate::query::QueryUpiu;
use crate::registers::{UicCommandArgument1Reg, UicCommandArgument3Reg};
use crate::request_list::{OverallCommandStatus, SlotState};
use crate::scsi::ScsiStatus;
use crate::transfer_request_processor::{IoCompletion, ScsiOutcome, TransferRequestProcessor};
use crate::uic::UicCommandOpcode;
use crate::upiu::{NopInUpiu, NopOutUpiu, ResponseUpiu, UpiuHeader, UpiuTransactionCode};
use fake_bti::FakeBti;
use fake_ufs::{
    FakeMmio, FakeUfs, FakeUtp, complete_uic_command, uic_hardware, uic_opcode, wait_until,
};
use mmio::ReadableRegister;
use std::sync::Arc;
use zerocopy::IntoBytes;
use zerocopy::byteorder::big_endian::U16;

/// A processor over the fake register block.
pub(crate) type TestProcessor = TransferRequestProcessor<FakeMmio>;

/// Register block size shared by the single-doorbell unit tests.
pub(crate) const MMIO_SIZE: usize = 4096;
/// Page size assumed for test data VMOs.
pub(crate) const PAGE_SIZE: u64 = 4096;
/// The admin command slot.
pub(crate) const ADMIN_SLOT: u8 = ADMIN_COMMAND_SLOT_NUMBER;
/// Overall Command Status value reported by the fake for a successful request.
pub(crate) const OCS_SUCCESS: u8 = OverallCommandStatus::Success as u8;

/// Creates an initialized 32-slot processor and the VMO backing its registers.
pub(crate) fn create_test_processor() -> (zx::Vmo, Arc<TestProcessor>) {
    let bti = FakeBti::create().expect("Failed to create fake bti");
    let vmo = zx::Vmo::create(MMIO_SIZE as u64).unwrap();
    let controller = Arc::new(UfsController::new(FakeMmio::new(&vmo, MMIO_SIZE)));
    let processor = Arc::new(TransferRequestProcessor::new(&bti, controller).unwrap());
    processor.init().unwrap();
    (vmo, processor)
}

/// Maps the fake hardware view of `processor`'s registers and descriptors. Completions raised
/// through [`FakeUtp::complete`] are delivered to `processor` by interrupt.
pub(crate) fn fake_utp(vmo: &zx::Vmo, processor: &Arc<TestProcessor>) -> FakeUtp {
    FakeUtp::new(vmo, MMIO_SIZE, processor)
}

/// Returns the state of `slot`.
pub(crate) fn slot_state(processor: &TestProcessor, slot: u8) -> SlotState {
    processor.request_list.lock().slot(slot).unwrap().state()
}

/// Returns whether `slot` currently holds a pinned client buffer.
pub(crate) fn slot_pinned(processor: &TestProcessor, slot: u8) -> bool {
    processor.request_list.lock().slot(slot).unwrap().pmt.is_some()
}

/// Moves `slot`'s deadline into the past so the stall watchdog logs it on its next pass.
pub(crate) fn expire_deadline(processor: &TestProcessor, slot: u8) {
    processor.request_list.lock().slot_mut(slot).unwrap().deadline =
        Some(zx::MonotonicInstant::get() - zx::MonotonicDuration::from_seconds(1));
}

/// Receives the outcome delivered to an [`io_callback`].
pub type IoOutcomeReceiver = futures::channel::oneshot::Receiver<ScsiOutcome>;

/// A oneshot [`IoCompletion`] and the receiver that collects its delivered outcome.
pub fn io_callback() -> (IoCompletion, IoOutcomeReceiver) {
    let (tx, rx) = futures::channel::oneshot::channel();
    (IoCompletion::Oneshot(tx), rx)
}

/// Returns the outcome already delivered to `rx`, panicking if none was.
pub fn delivered(rx: &mut IoOutcomeReceiver) -> Result<(), zx::Status> {
    rx.try_recv().expect("Completion dropped").expect("Completion not delivered").into_io_status()
}

/// NOP IN response for `task_tag`.
pub(crate) fn nop_in(task_tag: u8) -> NopInUpiu {
    let mut response = NopOutUpiu::new();
    response.header.trans_type = UpiuTransactionCode::NopIn as u8;
    response.header.task_tag = task_tag;
    response
}

/// RESPONSE UPIU for `task_tag` with SCSI `status`.
pub(crate) fn scsi_response(task_tag: u8, status: u8) -> ResponseUpiu {
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
pub(crate) fn check_condition(task_tag: u8, sense_key: u8) -> ResponseUpiu {
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
pub(crate) async fn wait_async(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = zx::MonotonicInstant::after(fake_ufs::FAKE_HW_TIMEOUT.into());
    while !condition() {
        if zx::MonotonicInstant::get() > deadline {
            return false;
        }
        fuchsia_async::Timer::new(zx::MonotonicDuration::from_millis(1)).await;
    }
    true
}

/// Returns the lowest slot whose doorbell bit is set, waiting for one.
pub(crate) fn wait_for_any_doorbell(utp: &FakeUtp) -> u8 {
    assert!(wait_until(|| utp.doorbell() != 0), "No doorbell rung");
    utp.doorbell().trailing_zeros() as u8
}

/// A device manager populated with `fake_ufs`'s device and geometry descriptors.
pub(crate) fn create_test_device_manager(fake_ufs: &FakeUfs) -> DeviceManager {
    let mut device_manager = DeviceManager::new(Quirks::default());
    device_manager.set_descriptors(fake_ufs.device_desc, fake_ufs.geometry_desc);
    device_manager
}

/// A UIC command (`UICCMD`, `UICCMDARG1`, `UICCMDARG3`) seen by [`fake_uic_device`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UicRecord {
    pub opcode: u32,
    pub attribute: u16,
    pub arg3: u32,
}

/// Answers `count` UIC commands on `vmo`, returning `answer(opcode, attribute)` in `UICCMDARG3`. A
/// `PA_PWRMode` `DME_SET` also sets `IS.UPMS`. `HCS` is set to `hcs` (plus `UCRDY`).
pub(crate) fn fake_uic_device(
    vmo: &zx::Vmo,
    hcs: u32,
    count: usize,
    answer: impl Fn(u32, u16) -> u32 + Send + 'static,
) -> std::thread::JoinHandle<Vec<UicRecord>> {
    let mut regs = uic_hardware(vmo, hcs);
    std::thread::spawn(move || {
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
pub(crate) fn answer_queries(
    mut utp: FakeUtp,
    count: usize,
    respond: impl Fn(&mut QueryUpiu) + Send + 'static,
) -> std::thread::JoinHandle<Vec<QueryUpiu>> {
    std::thread::spawn(move || {
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
