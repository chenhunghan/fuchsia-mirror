// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// A synthetic dispatcher used to test babeldiff. It is not part of Zircon.
//
// This port has mistakes planted on purpose; see tests/golden.rs.

use crate::object::dispatcher::{KernelHandle, StateLockGuard};
use zx_status::Status;
use zx_types::{zx_duration_t, zx_rights_t, ZX_DOORBELL_RUNG};

const MAX_TONE: u32 = 12;

/// What kind of doorbell a `DoorbellDispatcher` is.
pub enum DoorbellKind {
    Chime { notes: u32 },
    Buzzer { length: zx_duration_t },
}

impl DoorbellDispatcher {
    /// Rings the doorbell with `tone`.
    ///
    /// Returns ZX_ERR_OUT_OF_RANGE if `tone` is above MAX_TONE.
    pub fn ring(&self, tone: u32) -> Result<(), Status> {
        if tone == 0 || tone > MAX_TONE {
            return Err(Status::OUT_OF_RANGE);
        }
        match &self.kind {
            DoorbellKind::Chime { notes } => {
                ksync::lock!(let mut guard = self.lock());
                // Each note of the chime counts as a ring.
                for _ in 0..*notes {
                    self.record_ring_locked(&mut guard);
                }
            }
            DoorbellKind::Buzzer { .. } => {
                ksync::lock!(let mut guard = self.lock());
                self.record_ring_locked(&mut guard);
            }
        }
        Ok(())
    }

    /// Returns how many times the doorbell has rung.
    pub fn ring_count(&self) -> u64 {
        ksync::lock!(let guard = self.lock());
        guard.rings
    }

    fn record_ring_locked(&self, guard: &mut StateLockGuard<'_>) {
        guard.rings += 1;
        self.update_state_locked(guard, 0, ZX_DOORBELL_RUNG);
    }
}

impl ChimeDoorbellDispatcher {
    pub fn create(notes: u32) -> Result<(KernelHandle<DoorbellDispatcher>, zx_rights_t), Status> {
        if notes == 0 {
            return Err(Status::INVALID_ARGS);
        }
        let handle = KernelHandle::try_new(DoorbellDispatcher::new(DoorbellKind::Chime { notes }))
            .ok_or(Status::NO_MEMORY)?;
        Ok((handle, DoorbellDispatcher::default_rights()))
    }
}

impl BuzzerDoorbellDispatcher {
    pub fn create(
        length: zx_duration_t,
    ) -> Result<(KernelHandle<DoorbellDispatcher>, zx_rights_t), Status> {
        if length <= 0 {
            return Err(Status::INVALID_ARGS);
        }
        let handle =
            KernelHandle::try_new(DoorbellDispatcher::new(DoorbellKind::Buzzer { length }))
                .ok_or(Status::NO_RESOURCES)?;
        Ok((handle, DoorbellDispatcher::default_rights()))
    }
}
