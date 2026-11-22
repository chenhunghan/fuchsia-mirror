// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Shared [`FakeTcpc`] test fixture, synchronization gates, and fake VBUS model.

use super::*;

/// Returns `true` once the peer handle of `observer` has closed.
pub(in crate::tests) fn is_lease_closed(observer: &zx::EventPair) -> bool {
    match observer
        .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST)
        .to_result()
    {
        Ok(signals) => signals.contains(zx::Signals::EVENTPAIR_PEER_CLOSED),
        Err(zx::Status::TIMED_OUT) => false,
        Err(status) => panic!("unexpected wait_one status: {status:?}"),
    }
}

/// Fake-time test harness driving [`FakeTcpc`] and its background `Scope` tasks deterministically.
pub(in crate::tests) struct FakeTimeHarness {
    pub(in crate::tests) tcpc: Arc<FakeTcpc>,
    // Declared before `exec` so spawned `Scope` timers drop before their `TestExecutor`.
    pub(in crate::tests) scope: Option<fasync::Scope>,
    pub(in crate::tests) exec: fasync::TestExecutor,
}

impl FakeTimeHarness {
    pub(in crate::tests) fn new(
        initial_cc: CcStatus,
        configure: impl FnOnce(TypecPort) -> TypecPort,
    ) -> Self {
        let exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(0));
        let scope = fasync::Scope::new();
        let port = configure(
            TypecPort::new(scope.to_handle())
                .with_cc_debounce(zx::BootDuration::ZERO)
                .with_pd_debounce(zx::BootDuration::ZERO)
                .with_rp_value_change(zx::BootDuration::ZERO)
                .with_vsafe0v_debounce(zx::BootDuration::ZERO)
                .with_discharge_timeout(zx::BootDuration::INFINITE)
                .with_sink_debug_accessory_mode(true),
        );
        let tcpc = FakeTcpc::new_with_port(port, initial_cc);
        Self { tcpc, scope: Some(scope), exec }
    }

    pub(in crate::tests) fn run<T>(&mut self, fut: impl Future<Output = T>) -> T {
        let mut fut = pin!(fut);
        match self.exec.run_until_stalled(&mut fut) {
            Poll::Ready(val) => {
                self.flush();
                val
            }
            Poll::Pending => panic!("expected future to complete without stalling"),
        }
    }

    pub(in crate::tests) fn flush(&mut self) {
        let _ = self.exec.run_until_stalled(&mut pin!(futures::future::pending::<()>()));
    }

    pub(in crate::tests) fn sync(&mut self, lease: Option<zx::EventPair>) -> Result<()> {
        let tcpc = Arc::clone(&self.tcpc);
        self.run(async move { tcpc.sync_initial_state(lease).await })
    }

    pub(in crate::tests) fn cc_alert(
        &mut self,
        cc: CcStatus,
        lease: Option<zx::EventPair>,
    ) -> Result<()> {
        self.tcpc.set_cc(cc);
        let tcpc = Arc::clone(&self.tcpc);
        self.run(async move { tcpc.handle_cc_status(lease).await })
    }

    pub(in crate::tests) fn vbus_alert(&mut self, lease: Option<zx::EventPair>) -> Result<()> {
        let tcpc = Arc::clone(&self.tcpc);
        self.run(async move { tcpc.handle_vbus_change(lease).await })
    }

    /// Advances fake time by `duration`, returning whether any timer expired and flushing woken
    /// scope tasks.
    pub(in crate::tests) fn advance(&mut self, duration: zx::BootDuration) -> bool {
        self.flush();
        let delta = fasync::MonotonicDuration::from_nanos(duration.into_nanos());
        self.exec.set_fake_time(self.exec.now() + delta);
        let woke = self.exec.wake_expired_timers();
        if woke {
            self.flush();
        }
        woke
    }

    /// Asserts that no timer expires `1ns` before `duration` and that a timer expires on the final
    /// `1ns` of `duration`.
    pub(in crate::tests) fn advance_to_expiry(&mut self, duration: zx::BootDuration) {
        const ONE_NS: zx::BootDuration = zx::BootDuration::from_nanos(1);
        assert!(!self.advance(duration - ONE_NS), "timer expired before {duration:?}");
        assert!(self.advance(ONE_NS), "expected timer to expire at {duration:?}");
    }

    pub(in crate::tests) fn cancel_scope(&mut self) {
        let scope = self.scope.take().expect("scope already cancelled");
        let mut fut = pin!(scope.cancel());
        assert_matches!(self.exec.run_until_stalled(&mut fut), Poll::Ready(()));
    }
}

pub(in crate::tests) async fn poll_until_oneshot<F: Future>(
    fut: Pin<&mut F>,
    rx: oneshot::Receiver<()>,
) {
    match futures::future::select(fut, rx).await {
        futures::future::Either::Right((Ok(()), _)) => {}
        _ => panic!("expected oneshot to fire while future is still pending"),
    }
}

#[derive(Clone)]
pub(in crate::tests) struct Gate {
    pub(in crate::tests) mutex: Arc<AsyncMutex<()>>,
    pub(in crate::tests) started: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl Gate {
    pub(in crate::tests) fn new() -> Self {
        Self { mutex: Arc::new(AsyncMutex::new(())), started: Arc::new(Mutex::new(None)) }
    }

    pub(in crate::tests) async fn arm(&self) -> (AsyncMutexGuard<'_, ()>, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        *self.started.lock() = Some(tx);
        (self.mutex.lock().await, rx)
    }

    pub(in crate::tests) async fn pass(&self) {
        if let Some(tx) = self.started.lock().take() {
            let _ = tx.send(());
        }
        drop(self.mutex.lock().await);
    }
}

pub(in crate::tests) struct ActiveCallbackGuard {
    active_callbacks: Arc<AtomicUsize>,
}

impl ActiveCallbackGuard {
    pub(in crate::tests) fn enter(active_callbacks: &Arc<AtomicUsize>) -> Self {
        let current = active_callbacks.fetch_add(1, Ordering::SeqCst) + 1;
        assert_eq!(current, 1, "committed Typec callbacks must never overlap");
        Self { active_callbacks: Arc::clone(active_callbacks) }
    }
}

impl Drop for ActiveCallbackGuard {
    fn drop(&mut self) {
        self.active_callbacks.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Physical VBUS voltage band modeled by [`FakeTcpc`].
///
/// TCPCI Rev 2.0 Ver 1.3 Table 4-25 defines `vSafe0V` below `0.8 V` and Table 4-23 defines
/// `VbusPresent` above `4.0 V` on rise (clearing below `3.5 V` on fall), so `is_vbus_safe0v`
/// and `is_vbus_present` are mutually exclusive and both read `false` across the `0.8 V..3.5 V`
/// transition band (`Gap`, and up to `4.0 V` while rising).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tests) enum VbusLevel {
    Safe0V,
    Gap,
    Present,
}

pub(in crate::tests) struct FakeVbus {
    pub(in crate::tests) residual_gap: bool,
    pub(in crate::tests) partner_drives_vbus: bool,
    pub(in crate::tests) override_level: Option<VbusLevel>,
    pub(in crate::tests) vsafe0v_err: Option<&'static str>,
    pub(in crate::tests) vsafe0v_err_once: Option<&'static str>,
    pub(in crate::tests) vbus_present_err: Option<&'static str>,
    pub(in crate::tests) vbus_present_err_once: Option<&'static str>,
}

impl FakeVbus {
    pub(in crate::tests) const fn new() -> Self {
        Self {
            residual_gap: false,
            partner_drives_vbus: true,
            override_level: None,
            vsafe0v_err: None,
            vsafe0v_err_once: None,
            vbus_present_err: None,
            vbus_present_err_once: None,
        }
    }

    pub(in crate::tests) fn level(&self, partner_has_rp: bool) -> VbusLevel {
        if let Some(level) = self.override_level {
            return level;
        }
        if partner_has_rp && self.partner_drives_vbus {
            VbusLevel::Present
        } else if self.residual_gap {
            VbusLevel::Gap
        } else {
            VbusLevel::Safe0V
        }
    }
}

pub(in crate::tests) const fn has_partner_rp(cc: CcStatus) -> bool {
    cc.cc1.is_sink_rp() || cc.cc2.is_sink_rp()
}

/// A port controller fake.
pub(in crate::tests) struct FakeTcpc {
    pub(in crate::tests) port: TypecPort,
    pub(in crate::tests) raw_cc: Mutex<CcStatus>,
    pub(in crate::tests) cc_err: Mutex<Option<&'static str>>,
    pub(in crate::tests) cc_err_once: Mutex<Option<&'static str>>,
    pub(in crate::tests) vbus: Mutex<FakeVbus>,
    pub(in crate::tests) drp_toggling: AtomicBool,
    pub(in crate::tests) vbus_sourced: AtomicBool,
    pub(in crate::tests) cc_read_count: AtomicUsize,
    pub(in crate::tests) unattached_count: AtomicUsize,
    pub(in crate::tests) reentrant_query_in_callbacks: AtomicBool,
    pub(in crate::tests) yield_in_callbacks: AtomicBool,
    pub(in crate::tests) active_callbacks: Arc<AtomicUsize>,
    pub(in crate::tests) fail_attach: AtomicBool,
    pub(in crate::tests) supports_vbus_discharge: AtomicBool,
    pub(in crate::tests) fail_discharge_write: AtomicBool,
    pub(in crate::tests) fail_discharge_write_once: AtomicBool,
    pub(in crate::tests) discharge_stuck_on: AtomicBool,
    pub(in crate::tests) fail_sink_write: AtomicBool,
    pub(in crate::tests) fail_sink_write_once: AtomicBool,
    pub(in crate::tests) supports_vbus_sink: AtomicBool,
    pub(in crate::tests) sink_stuck_open: AtomicBool,
    pub(in crate::tests) hw_discharge: Mutex<Option<bool>>,
    pub(in crate::tests) hw_discharge_bleeding: AtomicBool,
    pub(in crate::tests) hw_sink: Mutex<Option<bool>>,
    pub(in crate::tests) discharge_writes: Mutex<Vec<bool>>,
    pub(in crate::tests) sink_writes: Mutex<Vec<bool>>,
    pub(in crate::tests) cc_read: Gate,
    pub(in crate::tests) attach: Gate,
    pub(in crate::tests) detach: Gate,
    pub(in crate::tests) vsafe0v_read: Gate,
    pub(in crate::tests) vbus_present_read: Gate,
    pub(in crate::tests) events: Mutex<Vec<String>>,
}

impl FakeTcpc {
    pub(in crate::tests) fn reset_to_disconnected(&self) {
        {
            let mut state = self.port.state.lock();
            assert!(
                !state.runner.actor_running,
                "reset_to_disconnected must not be called while the actor is running"
            );
            state.reset_to_disconnected();
        }
        self.vbus.lock().override_level = None;
        self.supports_vbus_discharge.store(true, Ordering::SeqCst);
        self.discharge_stuck_on.store(false, Ordering::SeqCst);
        self.supports_vbus_sink.store(true, Ordering::SeqCst);
        self.sink_stuck_open.store(false, Ordering::SeqCst);
        *self.hw_discharge.lock() = None;
        self.hw_discharge_bleeding.store(false, Ordering::SeqCst);
        *self.hw_sink.lock() = None;
    }

    pub(in crate::tests) fn new(scope: &fasync::Scope, initial_cc: CcStatus) -> Arc<Self> {
        Self::new_with_port(
            TypecPort::new(scope.to_handle())
                .with_cc_debounce(zx::BootDuration::ZERO)
                .with_pd_debounce(zx::BootDuration::ZERO)
                .with_rp_value_change(zx::BootDuration::ZERO)
                .with_vsafe0v_debounce(zx::BootDuration::ZERO)
                .with_discharge_timeout(zx::BootDuration::INFINITE)
                .with_sink_debug_accessory_mode(true),
            initial_cc,
        )
    }

    pub(in crate::tests) fn new_with_port(port: TypecPort, initial_cc: CcStatus) -> Arc<Self> {
        Arc::new(Self {
            port,
            raw_cc: Mutex::new(initial_cc),
            cc_err: Mutex::new(None),
            cc_err_once: Mutex::new(None),
            vbus: Mutex::new(FakeVbus::new()),
            drp_toggling: AtomicBool::new(initial_cc.looking4connection),
            vbus_sourced: AtomicBool::new(false),
            cc_read_count: AtomicUsize::new(0),
            unattached_count: AtomicUsize::new(0),
            reentrant_query_in_callbacks: AtomicBool::new(false),
            yield_in_callbacks: AtomicBool::new(false),
            active_callbacks: Arc::new(AtomicUsize::new(0)),
            fail_attach: AtomicBool::new(false),
            supports_vbus_discharge: AtomicBool::new(true),
            fail_discharge_write: AtomicBool::new(false),
            fail_discharge_write_once: AtomicBool::new(false),
            discharge_stuck_on: AtomicBool::new(false),
            fail_sink_write: AtomicBool::new(false),
            fail_sink_write_once: AtomicBool::new(false),
            supports_vbus_sink: AtomicBool::new(true),
            sink_stuck_open: AtomicBool::new(false),
            hw_discharge: Mutex::new(None),
            hw_discharge_bleeding: AtomicBool::new(false),
            hw_sink: Mutex::new(None),
            discharge_writes: Mutex::new(Vec::new()),
            sink_writes: Mutex::new(Vec::new()),
            cc_read: Gate::new(),
            attach: Gate::new(),
            detach: Gate::new(),
            vsafe0v_read: Gate::new(),
            vbus_present_read: Gate::new(),
            events: Mutex::new(Vec::new()),
        })
    }

    pub(in crate::tests) fn set_cc(&self, cc: CcStatus) {
        *self.raw_cc.lock() = cc;
        *self.cc_err.lock() = None;
    }

    pub(in crate::tests) fn set_cc_err(&self, msg: &'static str) {
        *self.cc_err.lock() = Some(msg);
    }

    pub(in crate::tests) fn set_cc_err_once(&self, msg: &'static str) {
        *self.cc_err_once.lock() = Some(msg);
    }

    pub(in crate::tests) fn set_vsafe0v(&self, is_vsafe0v: bool) {
        {
            let mut vbus = self.vbus.lock();
            vbus.vsafe0v_err = None;
            vbus.override_level = None;
            vbus.residual_gap = !is_vsafe0v;
        }
        if self.current_vbus_level() == VbusLevel::Safe0V {
            self.hw_discharge_bleeding.store(false, Ordering::SeqCst);
        }
    }

    pub(in crate::tests) fn set_vsafe0v_err(&self, msg: &'static str) {
        self.vbus.lock().vsafe0v_err = Some(msg);
    }

    pub(in crate::tests) fn set_vsafe0v_err_once(&self, msg: &'static str) {
        self.vbus.lock().vsafe0v_err_once = Some(msg);
    }

    pub(in crate::tests) fn set_vbus_present(&self, is_present: bool) {
        {
            let mut vbus = self.vbus.lock();
            vbus.vbus_present_err = None;
            vbus.override_level = None;
            vbus.partner_drives_vbus = is_present;
        }
        if self.current_vbus_level() == VbusLevel::Safe0V {
            self.hw_discharge_bleeding.store(false, Ordering::SeqCst);
        }
    }

    pub(in crate::tests) fn set_vbus_level(&self, level: VbusLevel) {
        {
            let mut vbus = self.vbus.lock();
            vbus.vsafe0v_err = None;
            vbus.vbus_present_err = None;
            vbus.override_level = Some(level);
        }
        if level == VbusLevel::Safe0V {
            self.hw_discharge_bleeding.store(false, Ordering::SeqCst);
        }
    }

    pub(in crate::tests) fn set_vbus_present_err(&self, msg: &'static str) {
        self.vbus.lock().vbus_present_err = Some(msg);
    }

    pub(in crate::tests) fn set_vbus_present_err_once(&self, msg: &'static str) {
        self.vbus.lock().vbus_present_err_once = Some(msg);
    }

    pub(in crate::tests) fn current_vbus_level(&self) -> VbusLevel {
        let partner_rp = has_partner_rp(*self.raw_cc.lock());
        self.vbus.lock().level(partner_rp)
    }

    /// Asserts cross-register hardware-model invariants across `typec_state`, `vbus_sourced`,
    /// `hw_discharge`, and `hw_sink`.
    pub(in crate::tests) fn assert_tcpci_invariants(&self) {
        let state = self.typec_state();
        let hw_discharge = *self.hw_discharge.lock();
        let hw_sink = *self.hw_sink.lock();
        let expected_sink = |open| self.supports_vbus_sink.load(Ordering::SeqCst).then_some(open);
        if self.vbus_sourced.load(Ordering::SeqCst) {
            assert_eq!(state, TypecState::AttachedSrc);
            assert_eq!(hw_sink, expected_sink(false));
            assert_ne!(hw_discharge, Some(true));
        }
        match state {
            TypecState::AttachedSrc => {
                assert_eq!(hw_sink, expected_sink(false));
                assert_ne!(hw_discharge, Some(true));
                assert!(!self.is_vbus_discharge_armed());
            }
            TypecState::AttachedSnk | TypecState::DebugAccessorySnk => {
                assert_eq!(hw_sink, expected_sink(true));
                assert_ne!(
                    self.port.state.lock().runner.hw.vbus_discharge,
                    PowerPathState::Enabled { confirmed: true }
                );
            }
            _ => {}
        }
        if !self.port.state.lock().machine.sink_debug_accessory_mode {
            assert_eq!(self.accessory(), None);
        }
    }
}

impl PortController for FakeTcpc {
    async fn read_cc_status(&self) -> Result<CcStatus> {
        self.cc_read_count.fetch_add(1, Ordering::SeqCst);
        let sample = match self.cc_err_once.lock().take().or_else(|| *self.cc_err.lock()) {
            Some(msg) => Err(msg),
            None => Ok(*self.raw_cc.lock()),
        };
        self.cc_read.pass().await;
        sample.map_err(|msg| anyhow::anyhow!("{msg}"))
    }

    async fn is_vbus_safe0v(&self) -> Result<bool> {
        let err = {
            let mut vbus = self.vbus.lock();
            vbus.vsafe0v_err_once.take().or(vbus.vsafe0v_err)
        };
        let sample = match err {
            Some(msg) => Err(msg),
            None => Ok(self.current_vbus_level() == VbusLevel::Safe0V),
        };
        if matches!(sample, Ok(true)) {
            self.hw_discharge_bleeding.store(false, Ordering::SeqCst);
        }
        self.vsafe0v_read.pass().await;
        sample.map_err(|msg| anyhow::anyhow!("{msg}"))
    }

    async fn is_vbus_present(&self) -> Result<bool> {
        let err = {
            let mut vbus = self.vbus.lock();
            vbus.vbus_present_err_once.take().or(vbus.vbus_present_err)
        };
        let sample = match err {
            Some(msg) => Err(msg),
            None => Ok(self.current_vbus_level() == VbusLevel::Present),
        };
        self.vbus_present_read.pass().await;
        sample.map_err(|msg| anyhow::anyhow!("{msg}"))
    }

    async fn write_vbus_discharge(&self, armed: bool) -> Result<bool> {
        if self.fail_discharge_write_once.swap(false, Ordering::SeqCst)
            || self.fail_discharge_write.load(Ordering::SeqCst)
        {
            return Err(anyhow::anyhow!("simulated POWER_CONTROL write failure"));
        }
        if !self.supports_vbus_discharge.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let active = armed || self.discharge_stuck_on.load(Ordering::SeqCst);
        *self.hw_discharge.lock() = Some(active);
        self.hw_discharge_bleeding
            .store(active && self.current_vbus_level() != VbusLevel::Safe0V, Ordering::SeqCst);
        self.discharge_writes.lock().push(armed);
        Ok(active)
    }

    async fn write_vbus_sink(&self, enable: bool) -> Result<bool> {
        if self.fail_sink_write_once.swap(false, Ordering::SeqCst)
            || self.fail_sink_write.load(Ordering::SeqCst)
        {
            return Err(anyhow::anyhow!("simulated SinkVbus write failure"));
        }
        if !self.supports_vbus_sink.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let open = enable || self.sink_stuck_open.load(Ordering::SeqCst);
        *self.hw_sink.lock() = Some(open);
        self.sink_writes.lock().push(enable);
        Ok(open)
    }
}

impl Typec for FakeTcpc {
    fn typec(&self) -> &TypecPort {
        &self.port
    }

    async fn on_sink_rp_changed(&self, rp: RpValue, wake_lease: Option<&zx::EventPair>) {
        let _guard = ActiveCallbackGuard::enter(&self.active_callbacks);
        if self.yield_in_callbacks.load(Ordering::SeqCst) {
            fasync::yield_now().await;
        }
        self.events.lock().push(format!("sink_rp:{rp:?}:lease={}", wake_lease.is_some()));
    }

    async fn on_unattached(&self) {
        let _guard = ActiveCallbackGuard::enter(&self.active_callbacks);
        self.vbus_sourced.store(false, Ordering::SeqCst);
        self.drp_toggling.store(true, Ordering::SeqCst);
        self.unattached_count.fetch_add(1, Ordering::SeqCst);
        if self.yield_in_callbacks.load(Ordering::SeqCst) {
            fasync::yield_now().await;
        }
    }

    async fn on_partner_attached(
        &self,
        role: PowerRole,
        wake_lease: Option<&zx::EventPair>,
    ) -> Result<()> {
        let _guard = ActiveCallbackGuard::enter(&self.active_callbacks);
        let expected_sink = |open| self.supports_vbus_sink.load(Ordering::SeqCst).then_some(open);
        match role {
            PowerRole::Source => {
                assert_eq!(*self.hw_sink.lock(), expected_sink(false));
                assert_ne!(*self.hw_discharge.lock(), Some(true));
            }
            PowerRole::Sink => {
                assert_eq!(*self.hw_sink.lock(), expected_sink(true));
            }
        }
        if self.reentrant_query_in_callbacks.load(Ordering::SeqCst) {
            let _ = self.sample_attachable_role().await.unwrap();
            let _ = self.typec_state();
            let _ = self.orientation();
            let _ = self.accessory();
        }
        self.attach.pass().await;
        if self.yield_in_callbacks.load(Ordering::SeqCst) {
            fasync::yield_now().await;
        }
        if self.fail_attach.load(Ordering::SeqCst) {
            return Err(anyhow::anyhow!("simulated attach failure"));
        }
        self.vbus_sourced.store(role == PowerRole::Source, Ordering::SeqCst);
        self.events.lock().push(format!("attached:{role:?}:lease={}", wake_lease.is_some()));
        Ok(())
    }

    async fn on_partner_detached(&self, wake_lease: Option<&zx::EventPair>) {
        let _guard = ActiveCallbackGuard::enter(&self.active_callbacks);
        if self.reentrant_query_in_callbacks.load(Ordering::SeqCst) {
            let _ = self.sample_attachable_role().await.unwrap();
            let _ = self.typec_state();
        }
        self.detach.pass().await;
        if self.yield_in_callbacks.load(Ordering::SeqCst) {
            fasync::yield_now().await;
        }
        self.vbus_sourced.store(false, Ordering::SeqCst);
        self.events.lock().push(format!("detached:lease={}", wake_lease.is_some()));
    }

    async fn on_vsafe0v(&self, wake_lease: Option<&zx::EventPair>) {
        let _guard = ActiveCallbackGuard::enter(&self.active_callbacks);
        self.events.lock().push(format!("vsafe0v:lease={}", wake_lease.is_some()));
    }
}
