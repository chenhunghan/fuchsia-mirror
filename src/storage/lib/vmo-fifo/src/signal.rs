// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Use Zircon user signals to coordinate queue states between producer and consumer.
// We use a pair of alternating signals for each event type (data available vs. space available)
// to eliminate the extra syscall required to clear signal bits after waking up.
pub const SIG_DATA_AVAILABLE_0: zx::Signals = zx::Signals::USER_0;
pub const SIG_DATA_AVAILABLE_1: zx::Signals = zx::Signals::USER_1;
pub const SIG_SPACE_AVAILABLE_0: zx::Signals = zx::Signals::USER_2;
pub const SIG_SPACE_AVAILABLE_1: zx::Signals = zx::Signals::USER_3;
pub const SIG_SHUTDOWN: zx::Signals = zx::Signals::USER_4;

// A synchronization helper that coordinates event waiting and signaling on a VMO using a pair of
// Zircon user signals and toggling between them.
pub struct EventSignal {
    set_next: zx::Signals,
    clear_next: zx::Signals,
}

impl EventSignal {
    pub const fn new(sig_0: zx::Signals, sig_1: zx::Signals) -> Self {
        Self { set_next: sig_0, clear_next: sig_1 }
    }

    pub fn signal(&mut self, vmo: &zx::Vmo) -> Result<(), zx::Status> {
        // Asserts the active signal bit and clears the alternate bit.
        vmo.signal(self.clear_next, self.set_next)?;
        std::mem::swap(&mut self.set_next, &mut self.clear_next);
        Ok(())
    }

    // Returns the signal mask combining the next expected signal bit and `shutdown_sig`.
    pub fn current_mask(&self, shutdown_sig: zx::Signals) -> zx::Signals {
        self.set_next | shutdown_sig
    }

    // Toggles the active and alternate signal bits after the current signal has been observed.
    pub fn advance(&mut self) {
        std::mem::swap(&mut self.set_next, &mut self.clear_next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_async::{DurationExt, OnSignals, Timer};

    #[fuchsia::test]
    fn test_event_signal() {
        let vmo = zx::Vmo::create(4096).expect("VMO creation failed");
        let mut event_sender = EventSignal::new(SIG_DATA_AVAILABLE_0, SIG_DATA_AVAILABLE_1);
        let mut event_receiver = EventSignal::new(SIG_DATA_AVAILABLE_0, SIG_DATA_AVAILABLE_1);

        // First cycle: sets `SIG_DATA_AVAILABLE_0` and clears `SIG_DATA_AVAILABLE_1`.
        event_sender.signal(&vmo).expect("signal failed");
        let mask1 = event_receiver.current_mask(SIG_SHUTDOWN);
        assert_eq!(mask1, SIG_DATA_AVAILABLE_0 | SIG_SHUTDOWN);
        let observed1 =
            vmo.wait_one(mask1, zx::MonotonicInstant::INFINITE_PAST).to_result().expect("wait 1");
        assert!(observed1.contains(SIG_DATA_AVAILABLE_0));
        assert!(!observed1.contains(SIG_DATA_AVAILABLE_1));
        event_receiver.advance();

        // Second cycle: sets `SIG_DATA_AVAILABLE_1` and clears `SIG_DATA_AVAILABLE_0`.
        event_sender.signal(&vmo).expect("signal failed");
        let mask2 = event_receiver.current_mask(SIG_SHUTDOWN);
        assert_eq!(mask2, SIG_DATA_AVAILABLE_1 | SIG_SHUTDOWN);
        let observed2 =
            vmo.wait_one(mask2, zx::MonotonicInstant::INFINITE_PAST).to_result().expect("wait 2");
        assert!(observed2.contains(SIG_DATA_AVAILABLE_1));
        assert!(!observed2.contains(SIG_DATA_AVAILABLE_0));
        event_receiver.advance();
    }

    #[fuchsia::test]
    async fn test_event_signal_async_concurrency() {
        let vmo = zx::Vmo::create(4096).expect("VMO creation failed");
        let mut event_sender = EventSignal::new(SIG_DATA_AVAILABLE_0, SIG_DATA_AVAILABLE_1);
        let mut event_receiver = EventSignal::new(SIG_DATA_AVAILABLE_0, SIG_DATA_AVAILABLE_1);

        let wait_task = async {
            let mask = event_receiver.current_mask(SIG_SHUTDOWN);
            let observed = OnSignals::new(&vmo, mask).await.expect("OnSignals failed");
            assert!(observed.contains(SIG_DATA_AVAILABLE_0));
            event_receiver.advance();
        };

        let signal_task = async {
            Timer::new(zx::MonotonicDuration::from_millis(1).after_now()).await;
            event_sender.signal(&vmo).expect("signal failed");
        };

        futures::join!(wait_task, signal_task);
    }
}
