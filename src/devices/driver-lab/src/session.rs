// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Session lifecycle: identity staleness, allowlist validation, and the
//! exclusive mutation lease.
//!
//! Opening a session validates the caller's identity expectations against
//! the proxy instance and the entire allowlist against the resources and
//! ceiling. A stale expectation or rejected allowlist creates no session
//! and performs no hardware operation.

use crate::access_policy::{
    AccessPolicy, AccessRule, Denial, MmioResource, ResourceCeiling, ResourceId,
};
use crate::config::{AccessLimitEnforcer, BASELINE_MAX_DEADLINE_NS};
use std::collections::BTreeMap;

/// Host-supplied run context recorded with each session. Values are
/// diagnostic provenance, not authorization.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunContext {
    /// Host run identifier.
    pub run_id: String,
    /// Host case identifier.
    pub case_id: String,
    /// Canonical plan digest.
    pub plan_digest: String,
    /// Host tool version.
    pub host_tool_version: String,
}

/// The proxy instance identity that session expectations are validated
/// against. Also the shape of those expectations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyIdentity {
    /// Boot identity of the target.
    pub boot_id: String,
    /// Generation of this proxy instance; changes on driver restart.
    pub proxy_generation: u64,
    /// Whole-instance resource-description digest.
    pub resource_digest: String,
    /// Target-policy digest.
    pub policy_digest: String,
}

pub use crate::access_policy::SessionMode;

/// Why a session could not be opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// The proxy is stopping and accepts no new sessions.
    NotAccepting,
    /// The expected boot identity does not match.
    StaleBootId,
    /// The expected proxy generation does not match.
    StaleProxyGeneration,
    /// The expected resource digest does not match.
    StaleResourceDigest,
    /// The expected policy digest does not match.
    StalePolicyDigest,
    /// An allowlist rule failed narrowing validation; no session exists.
    RejectedAllowlist {
        /// The first offending rule.
        rule: AccessRule,
        /// Why it was rejected.
        denial: Denial,
    },
    /// Another mutating session already holds the mutation lease.
    MutationLeaseHeld,
    /// Mutating sessions are not permitted by the target ceiling policy.
    MutationNotPermitted,
    /// The request set a reserved (phase 2) expectation field. Fail
    /// closed rather than silently ignoring the field.
    UnsupportedExpectation,
}

/// One open session.
#[derive(Debug)]
pub struct Session {
    /// Session identifier, unique for the proxy instance's lifetime.
    pub id: u64,
    /// Session mode.
    pub mode: SessionMode,
    /// Host-supplied run context.
    pub context: RunContext,
    /// The validated per-session policy engine.
    pub policy: AccessPolicy,
    /// Enforcer for access rate and deadline limits (Spec 12.1 step 9).
    pub limit_enforcer: AccessLimitEnforcer,
}

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Callback invoked when a mutating session acquires (`true`) or releases
/// (`false`) the exclusive mutation lease (Spec Phase 2 Section 2.2).
pub type QuiesceHook = Arc<dyn Fn(bool) + Send + Sync>;

/// Tracks open sessions, identity staleness, and the mutation lease.
pub struct SessionManager {
    identity: ProxyIdentity,
    resources: BTreeMap<ResourceId, MmioResource>,
    ceiling: BTreeMap<ResourceId, ResourceCeiling>,
    allow_mutating_sessions: bool,
    next_id: u64,
    sessions: BTreeMap<u64, Session>,
    mutating: Option<u64>,
    accepting: bool,
    limit_enforcer: AccessLimitEnforcer,
    quiesced: Arc<AtomicBool>,
    quiesce_hook: Option<QuiesceHook>,
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager")
            .field("identity", &self.identity)
            .field("resources", &self.resources)
            .field("ceiling", &self.ceiling)
            .field("allow_mutating_sessions", &self.allow_mutating_sessions)
            .field("next_id", &self.next_id)
            .field("sessions", &self.sessions)
            .field("mutating", &self.mutating)
            .field("accepting", &self.accepting)
            .field("quiesced", &self.quiesced.load(Ordering::SeqCst))
            .field("has_quiesce_hook", &self.quiesce_hook.is_some())
            .finish()
    }
}

impl SessionManager {
    /// Creates a manager for one proxy instance.
    pub fn new(
        identity: ProxyIdentity,
        resources: BTreeMap<ResourceId, MmioResource>,
        ceiling: BTreeMap<ResourceId, ResourceCeiling>,
        allow_mutating_sessions: bool,
    ) -> Self {
        Self::with_limits(
            identity,
            resources,
            ceiling,
            allow_mutating_sessions,
            AccessLimitEnforcer::new(0, BASELINE_MAX_DEADLINE_NS),
        )
    }

    /// Creates a manager with explicit access limit enforcer.
    pub fn with_limits(
        identity: ProxyIdentity,
        resources: BTreeMap<ResourceId, MmioResource>,
        ceiling: BTreeMap<ResourceId, ResourceCeiling>,
        allow_mutating_sessions: bool,
        limit_enforcer: AccessLimitEnforcer,
    ) -> Self {
        Self {
            identity,
            resources,
            ceiling,
            allow_mutating_sessions,
            next_id: 1,
            sessions: BTreeMap::new(),
            mutating: None,
            accepting: true,
            limit_enforcer,
            quiesced: Arc::new(AtomicBool::new(false)),
            quiesce_hook: None,
        }
    }

    /// Registers a synchronous quiesce callback invoked with `true` when a
    /// mutating session opens and `false` when it closes.
    pub fn set_quiesce_hook(&mut self, hook: QuiesceHook) {
        self.quiesce_hook = Some(hook);
    }

    /// Returns whether the driver is currently quiesced due to an active
    /// mutating session.
    pub fn is_quiesced(&self) -> bool {
        self.quiesced.load(Ordering::SeqCst)
    }

    /// Returns a cloneable handle to the atomic quiesce flag so driver
    /// background loops can check quiesce state without locking.
    pub fn quiesce_flag(&self) -> Arc<AtomicBool> {
        self.quiesced.clone()
    }

    /// The identity `Describe` reports and expectations are checked
    /// against.
    pub fn identity(&self) -> &ProxyIdentity {
        &self.identity
    }

    /// The resource descriptions offered by this instance.
    pub fn resources(&self) -> &BTreeMap<ResourceId, MmioResource> {
        &self.resources
    }

    /// Opens a session. Stale identity is rejected before allowlist
    /// validation; a rejected allowlist creates no session.
    pub fn open_session(
        &mut self,
        context: RunContext,
        mode: SessionMode,
        expectations: &ProxyIdentity,
        allowlist: impl IntoIterator<Item = AccessRule>,
    ) -> Result<u64, OpenError> {
        if !self.accepting {
            return Err(OpenError::NotAccepting);
        }
        if expectations.boot_id != self.identity.boot_id {
            return Err(OpenError::StaleBootId);
        }
        if expectations.proxy_generation != self.identity.proxy_generation {
            return Err(OpenError::StaleProxyGeneration);
        }
        if expectations.resource_digest != self.identity.resource_digest {
            return Err(OpenError::StaleResourceDigest);
        }
        if expectations.policy_digest != self.identity.policy_digest {
            return Err(OpenError::StalePolicyDigest);
        }
        if mode == SessionMode::Mutating {
            if !self.allow_mutating_sessions {
                return Err(OpenError::MutationNotPermitted);
            }
            if self.mutating.is_some() {
                return Err(OpenError::MutationLeaseHeld);
            }
        }
        let policy =
            AccessPolicy::new(mode, self.resources.clone(), self.ceiling.clone(), allowlist)
                .map_err(|(rule, denial)| OpenError::RejectedAllowlist { rule, denial })?;
        let id = self.next_id;
        self.next_id += 1;
        if mode == SessionMode::Mutating {
            self.mutating = Some(id);
            self.quiesced.store(true, Ordering::SeqCst);
            if let Some(hook) = &self.quiesce_hook {
                hook(true);
            }
        }
        self.sessions.insert(
            id,
            Session { id, mode, context, policy, limit_enforcer: self.limit_enforcer.clone() },
        );
        Ok(id)
    }

    /// Enforces access-class rate limit for `session_id`.
    pub fn check_access(
        &mut self,
        session_id: u64,
        class: crate::access_policy::AccessClass,
        now_ns: i64,
    ) -> Result<(), Denial> {
        let session = self.sessions.get_mut(&session_id).ok_or(Denial::UnknownResource)?;
        session.limit_enforcer.check_access(class, now_ns)
    }

    /// Enforces deadline limit for `session_id`.
    pub fn check_deadline(&self, session_id: u64, requested_timeout_ns: i64) -> Result<(), Denial> {
        let session = self.sessions.get(&session_id).ok_or(Denial::UnknownResource)?;
        session.limit_enforcer.check_deadline(requested_timeout_ns)
    }

    /// The session with `id`, if open.
    pub fn session(&self, id: u64) -> Option<&Session> {
        self.sessions.get(&id)
    }

    /// Closes a session, releasing the mutation lease and unquiescing the
    /// driver if it held the lease. Returns whether the session existed.
    pub fn close_session(&mut self, id: u64) -> bool {
        let existed = self.sessions.remove(&id).is_some();
        if existed && self.mutating == Some(id) {
            self.mutating = None;
            self.quiesced.store(false, Ordering::SeqCst);
            if let Some(hook) = &self.quiesce_hook {
                hook(false);
            }
        }
        existed
    }

    /// Stop path: reject all new sessions and release any active mutation
    /// lease / quiesce state.
    pub fn reject_new_sessions(&mut self) {
        self.accepting = false;
        if self.mutating.take().is_some() {
            self.quiesced.store(false, Ordering::SeqCst);
            if let Some(hook) = &self.quiesce_hook {
                hook(false);
            }
        }
    }

    /// Whether new sessions are being accepted.
    pub fn is_accepting(&self) -> bool {
        self.accepting
    }

    /// Number of open sessions.
    pub fn open_count(&self) -> usize {
        self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access_policy::{AccessClass, WIDTH32};

    const CTRL: ResourceId = 1;

    fn identity() -> ProxyIdentity {
        ProxyIdentity {
            boot_id: "boot-1".to_string(),
            proxy_generation: 7,
            resource_digest: "sha256:aa".to_string(),
            policy_digest: "sha256:bb".to_string(),
        }
    }

    fn manager() -> SessionManager {
        let resources = BTreeMap::from([(
            CTRL,
            MmioResource {
                name: "ctrl".to_string(),
                kind: crate::access_policy::ResourceKind::Mmio,
                logical_size: 0x100,
                mapped_size: 0x100,
            },
        )]);
        let ceiling = BTreeMap::from([(
            CTRL,
            ResourceCeiling {
                hard_denied: vec![],
                allow_unknown_reads: true,
                allow_poll: false,
                writable_registers: vec![],
                protocol: None,
                allow_interrupt: false,
            },
        )]);
        SessionManager::new(identity(), resources, ceiling, true)
    }

    fn open(manager: &mut SessionManager, mode: SessionMode) -> Result<u64, OpenError> {
        manager.open_session(RunContext::default(), mode, &identity(), [])
    }

    #[test]
    fn open_and_close() {
        let mut manager = manager();
        let id = open(&mut manager, SessionMode::ReadOnly).unwrap();
        assert_eq!(manager.open_count(), 1);
        assert!(manager.session(id).is_some());
        assert!(manager.close_session(id));
        assert_eq!(manager.open_count(), 0);
        assert!(!manager.close_session(id));
    }

    #[test]
    fn session_ids_are_unique_and_increasing() {
        let mut manager = manager();
        let first = open(&mut manager, SessionMode::ReadOnly).unwrap();
        let second = open(&mut manager, SessionMode::ReadOnly).unwrap();
        assert!(second > first);
    }

    #[test]
    fn each_stale_expectation_is_distinct() {
        let mut manager = manager();

        let mut stale = identity();
        stale.boot_id = "boot-2".to_string();
        assert_eq!(
            manager.open_session(RunContext::default(), SessionMode::ReadOnly, &stale, []),
            Err(OpenError::StaleBootId)
        );

        let mut stale = identity();
        stale.proxy_generation = 8;
        assert_eq!(
            manager.open_session(RunContext::default(), SessionMode::ReadOnly, &stale, []),
            Err(OpenError::StaleProxyGeneration)
        );

        let mut stale = identity();
        stale.resource_digest = "sha256:cc".to_string();
        assert_eq!(
            manager.open_session(RunContext::default(), SessionMode::ReadOnly, &stale, []),
            Err(OpenError::StaleResourceDigest)
        );

        let mut stale = identity();
        stale.policy_digest = "sha256:dd".to_string();
        assert_eq!(
            manager.open_session(RunContext::default(), SessionMode::ReadOnly, &stale, []),
            Err(OpenError::StalePolicyDigest)
        );

        assert_eq!(manager.open_count(), 0);
    }

    #[test]
    fn rejected_allowlist_creates_no_session() {
        let mut manager = manager();
        let rule =
            AccessRule { resource: 99, offset: 0, width: WIDTH32, class: AccessClass::ReadOnce };
        let result =
            manager.open_session(RunContext::default(), SessionMode::ReadOnly, &identity(), [rule]);
        assert_eq!(
            result,
            Err(OpenError::RejectedAllowlist { rule, denial: Denial::UnknownResource })
        );
        assert_eq!(manager.open_count(), 0);
    }

    #[test]
    fn mutation_lease_is_exclusive() {
        let mut manager = manager();
        let holder = open(&mut manager, SessionMode::Mutating).unwrap();
        assert_eq!(open(&mut manager, SessionMode::Mutating), Err(OpenError::MutationLeaseHeld));
        // Read-only sessions still coexist.
        assert!(open(&mut manager, SessionMode::ReadOnly).is_ok());
        // Closing the holder releases the lease.
        assert!(manager.close_session(holder));
        assert!(open(&mut manager, SessionMode::Mutating).is_ok());
    }

    #[test]
    fn reject_new_sessions_fails_closed() {
        let mut manager = manager();
        manager.reject_new_sessions();
        assert_eq!(open(&mut manager, SessionMode::ReadOnly), Err(OpenError::NotAccepting));
    }

    #[test]
    fn mutating_session_rejected_when_not_permitted() {
        let resources = BTreeMap::from([(
            CTRL,
            MmioResource {
                name: "ctrl".to_string(),
                kind: crate::access_policy::ResourceKind::Mmio,
                logical_size: 0x100,
                mapped_size: 0x100,
            },
        )]);
        let ceiling = BTreeMap::from([(
            CTRL,
            ResourceCeiling {
                hard_denied: vec![],
                allow_unknown_reads: true,
                allow_poll: false,
                writable_registers: vec![],
                protocol: None,
                allow_interrupt: false,
            },
        )]);
        let mut manager = SessionManager::new(identity(), resources, ceiling, false);
        assert_eq!(open(&mut manager, SessionMode::Mutating), Err(OpenError::MutationNotPermitted));
        // Read-only sessions still work.
        assert!(open(&mut manager, SessionMode::ReadOnly).is_ok());
    }

    #[test]
    fn session_rate_and_deadline_limits() {
        let mut mgr = manager();
        mgr.limit_enforcer = AccessLimitEnforcer::new(2, 500_000_000);
        let id = open(&mut mgr, SessionMode::ReadOnly).unwrap();

        let t0 = 1_000_000_000;
        assert!(mgr.check_access(id, AccessClass::ReadOnce, t0).is_ok());
        assert!(mgr.check_access(id, AccessClass::ReadOnce, t0 + 100).is_ok());
        assert_eq!(
            mgr.check_access(id, AccessClass::ReadOnce, t0 + 200),
            Err(Denial::LimitExceeded)
        );

        // Deadline check
        assert!(mgr.check_deadline(id, 400_000_000).is_ok());
        assert_eq!(mgr.check_deadline(id, 600_000_000), Err(Denial::LimitExceeded));
    }

    #[test]
    fn quiesce_hook_engages_on_mutating_session_and_releases_on_close_or_stop() {
        let mut mgr = manager();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_clone = events.clone();
        mgr.set_quiesce_hook(Arc::new(move |paused| {
            events_clone.lock().unwrap().push(paused);
        }));
        let flag = mgr.quiesce_flag();
        assert!(!mgr.is_quiesced());
        assert!(!flag.load(Ordering::SeqCst));

        // Read-only session does not trigger quiesce
        let ro_id = open(&mut mgr, SessionMode::ReadOnly).unwrap();
        assert!(!mgr.is_quiesced());
        assert!(events.lock().unwrap().is_empty());

        // Mutating session engages quiesce
        let mut_id = open(&mut mgr, SessionMode::Mutating).unwrap();
        assert!(mgr.is_quiesced());
        assert!(flag.load(Ordering::SeqCst));
        assert_eq!(*events.lock().unwrap(), vec![true]);

        // Second mutating session fails without re-firing quiesce
        assert_eq!(open(&mut mgr, SessionMode::Mutating), Err(OpenError::MutationLeaseHeld));
        assert_eq!(*events.lock().unwrap(), vec![true]);

        // Closing read-only session does not release quiesce
        assert!(mgr.close_session(ro_id));
        assert!(mgr.is_quiesced());
        assert_eq!(*events.lock().unwrap(), vec![true]);

        // Closing mutating session releases quiesce
        assert!(mgr.close_session(mut_id));
        assert!(!mgr.is_quiesced());
        assert!(!flag.load(Ordering::SeqCst));
        assert_eq!(*events.lock().unwrap(), vec![true, false]);

        // Re-opening mutating session and stopping driver also releases quiesce
        let _mut_id2 = open(&mut mgr, SessionMode::Mutating).unwrap();
        assert!(mgr.is_quiesced());
        mgr.reject_new_sessions();
        assert!(!mgr.is_quiesced());
        assert_eq!(*events.lock().unwrap(), vec![true, false, true, false]);
    }
}
