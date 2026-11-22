// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Sink `Rp` tracking and `tRpValueChange` debounce transitions for [`Machine`].

use super::{Effect, Event, Machine, Phase, Step};
use crate::cc::{CcStatus, PowerRole, RpValue};

/// Sub-stage within [`Phase::AttachedSnk`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SinkStage {
    /// Partner is steadily attached at the committed `Rp` level.
    Steady,
    /// Debouncing a Sink `Rp` advertisement rise (`tRpValueChange`); every sample must match
    /// `candidate`.
    DebouncingRpRise { candidate: CcStatus },
    /// Debouncing a Sink `Rp` advertisement fall (`tRpValueChange`); cancelled only if a valid
    /// partner sample returns `>= committed` (filtering short non-idle USB PD BMC dips, R2.5 Table
    /// 4-34), while lower partner samples lower `floor` and `SNK.Open` keeps the window running
    /// (§4.5.2.3.2.2, §4.5.2.3.3.2).
    DebouncingRpFall { floor: RpValue },
}

/// Lowest-`Rp` Sink sample seen in the active `AttachWait.SNK` qualification window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SinkAttachSample {
    pub(super) cc_status: CcStatus,
    pub(super) rp: RpValue,
}

impl Machine {
    /// Settles a persistent `CC_STATUS` read error when `tRpValueChange` expires without
    /// overwriting [`Self::cc_status`]: drops a pending Sink `Rp` rise to `Steady` and commits
    /// `floor` on a pending fall so `tSinkAdj` is not delayed.
    pub(crate) fn settle_rp_debounce_read_error(&mut self) -> Step {
        if self.disabled {
            return Step::Absorbed;
        }
        match self.phase {
            Phase::AttachedSnk { cc_status, rp, stage } => {
                self.on_sink_rp_lost(cc_status, rp, stage, true)
            }
            _ => Step::Absorbed,
        }
    }

    pub(super) fn set_sink_stage(
        &mut self,
        committed_cc: CcStatus,
        committed_rp: RpValue,
        stage: SinkStage,
    ) {
        self.set_phase(Phase::AttachedSnk { cc_status: committed_cc, rp: committed_rp, stage });
    }

    /// Commits a debounced Sink `Rp` level change while preserving the attach-time `cc_status`.
    fn commit_sink_rp_change(
        &mut self,
        committed_cc: CcStatus,
        next_rp: RpValue,
        stage: SinkStage,
    ) -> Step {
        self.telemetry.rp_change_count += 1;
        self.set_phase(Phase::AttachedSnk { cc_status: committed_cc, rp: next_rp, stage });
        Step::Advanced(Some(Effect::NotifySinkRp(next_rp)))
    }

    pub(super) fn sink_attached_transition(
        &mut self,
        committed_cc: CcStatus,
        committed_rp: RpValue,
        stage: SinkStage,
        event: Event,
    ) -> Step {
        match event {
            // `Attached.SNK` and `DebugAccessory.SNK` exit only on VBUS loss (`vSinkDisconnect`,
            // R2.5 §4.5.2.2.5.2, §4.5.2.2.19.2); once the runner re-samples CC in
            // `Unattached.SNK`, a still-asserted `Rp` re-enters `AttachWait.SNK` and `tCCDebounce`.
            Event::VbusChanged { .. } => {
                self.telemetry.disconnect_count += 1;
                self.enter_unattached(Some(PowerRole::Sink))
            }
            // Partner still present on the same CC pin(s): debounce `Rp` rises strictly by
            // pin-pair (`tRpValueChange`, R2.5 §4.5.2.3, §B.2.4.2.2.2, §B.2.4.2.4.2) and `Rp`
            // falls monotonically so lower-level steps never postpone `tSinkAdj` (Table 4-32),
            // while keeping the attach-time `committed_cc` (`orientation`) fixed.
            Event::CcChanged(cc) | Event::DebounceExpired { cc, .. } => {
                match self.keeps_attached_sink(committed_cc, cc) {
                    Some(new_rp) => match new_rp.cmp(&committed_rp) {
                        std::cmp::Ordering::Equal => {
                            self.set_sink_stage(committed_cc, committed_rp, SinkStage::Steady);
                            if stage == SinkStage::Steady {
                                Step::Absorbed
                            } else {
                                Step::Advanced(None)
                            }
                        }
                        std::cmp::Ordering::Greater => {
                            self.on_sink_rp_rise(committed_cc, committed_rp, stage, event, new_rp)
                        }
                        std::cmp::Ordering::Less => {
                            self.on_sink_rp_fall(committed_cc, committed_rp, stage, event, new_rp)
                        }
                    },
                    None => self.on_sink_rp_lost(
                        committed_cc,
                        committed_rp,
                        stage,
                        matches!(event, Event::DebounceExpired { .. }),
                    ),
                }
            }
        }
    }

    fn on_sink_rp_rise(
        &mut self,
        committed_cc: CcStatus,
        committed_rp: RpValue,
        stage: SinkStage,
        event: Event,
        new_rp: RpValue,
    ) -> Step {
        let cc = event.cc();
        match (stage, event) {
            (SinkStage::DebouncingRpRise { candidate }, Event::DebounceExpired { .. })
                if candidate == cc =>
            {
                self.commit_sink_rp_change(committed_cc, new_rp, SinkStage::Steady)
            }
            (SinkStage::DebouncingRpRise { candidate }, Event::CcChanged(_)) if candidate == cc => {
                Step::Absorbed
            }
            _ => {
                self.set_sink_stage(
                    committed_cc,
                    committed_rp,
                    SinkStage::DebouncingRpRise { candidate: cc },
                );
                Step::Advanced(None)
            }
        }
    }

    fn on_sink_rp_fall(
        &mut self,
        committed_cc: CcStatus,
        committed_rp: RpValue,
        stage: SinkStage,
        event: Event,
        new_rp: RpValue,
    ) -> Step {
        let cc = event.cc();
        match (stage, event) {
            (SinkStage::DebouncingRpFall { floor }, Event::DebounceExpired { .. }) => {
                let (committed_floor, next_stage) = if new_rp <= floor {
                    (new_rp, SinkStage::Steady)
                } else {
                    // Commit `floor` within `tSinkAdj`, then debounce higher `cc`.
                    (floor, SinkStage::DebouncingRpRise { candidate: cc })
                };
                self.commit_sink_rp_change(committed_cc, committed_floor, next_stage)
            }
            (SinkStage::DebouncingRpFall { floor }, Event::CcChanged(_)) => {
                if new_rp < floor {
                    self.set_sink_stage(
                        committed_cc,
                        committed_rp,
                        SinkStage::DebouncingRpFall { floor: new_rp },
                    );
                }
                Step::Absorbed
            }
            _ => {
                self.set_sink_stage(
                    committed_cc,
                    committed_rp,
                    SinkStage::DebouncingRpFall { floor: new_rp },
                );
                Step::Advanced(None)
            }
        }
    }

    /// Without USB PD, CC changes that lose `Rp` while VBUS stays present (e.g. ground offset
    /// pulling Default `Rp` into `SNK.Open` under load) do not detach a Sink (R2.5 §4.5.2.2.5.2,
    /// §4.5.2.2.19.2): a pending rise is dropped, while a pending fall keeps its window running
    /// and commits `floor` at expiry.
    fn on_sink_rp_lost(
        &mut self,
        committed_cc: CcStatus,
        committed_rp: RpValue,
        stage: SinkStage,
        expired: bool,
    ) -> Step {
        match (stage, expired) {
            (SinkStage::DebouncingRpFall { floor }, true) => {
                self.commit_sink_rp_change(committed_cc, floor, SinkStage::Steady)
            }
            (SinkStage::DebouncingRpFall { .. }, false) | (SinkStage::Steady, _) => Step::Absorbed,
            _ => {
                self.set_sink_stage(committed_cc, committed_rp, SinkStage::Steady);
                Step::Advanced(None)
            }
        }
    }

    /// In `AttachWait.SNK`, tracks the earliest sample at the lowest `Rp` level seen since
    /// `tCCDebounce` started so [`Self::commit_attached`] commits both its CC pins and `Rp` when a
    /// rise has not yet completed `tRpValueChange` (R2.5 §4.5.2.3).
    pub(super) fn note_sink_attach_sample(
        &mut self,
        role: PowerRole,
        prev_cc: CcStatus,
        cc: CcStatus,
    ) {
        if role != PowerRole::Sink {
            return;
        }
        for sample in [prev_cc, cc] {
            if let Some(rp) = sample.sink_rp_level()
                && self.sink_attach_base.is_none_or(|base| rp < base.rp)
            {
                self.sink_attach_base = Some(SinkAttachSample { cc_status: sample, rp });
            }
        }
    }

    /// Returns `cc`'s Sink `Rp` level when `cc` keeps an `Attached.SNK` or `DebugAccessory.SNK`
    /// partner on its committed CC pin(s).
    fn keeps_attached_sink(&self, committed_cc: CcStatus, cc: CcStatus) -> Option<RpValue> {
        self.matches_partner(PowerRole::Sink, committed_cc, cc)
            .then(|| cc.sink_rp_level())
            .flatten()
    }
}

#[cfg(test)]
#[path = "../tests/state_sink.rs"]
mod tests;
