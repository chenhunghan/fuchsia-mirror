// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Core interrupt observation, sequence tracking, waiter notification, and
//! acknowledgement.
//!
//! Preserves interrupt events, timestamps, sequence ordering, and coalescing
//! metrics per Spec Section 11.7 and Section 17 (Milestone P5).

use crate::access_policy::{Denial, ResourceId};
use crate::hardware_backend::BackendError;
use std::collections::BTreeMap;

/// One interrupt event report returned to waiters and audited (Spec 11.7, 17).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterruptEvent {
    /// The interrupt resource ID.
    pub resource: ResourceId,
    /// Monotonically increasing sequence number for this interrupt resource.
    pub sequence: u64,
    /// Total lifetime count of interrupts on this resource.
    pub count: u64,
    /// Target monotonic timestamp (in nanoseconds) when the interrupt triggered.
    pub timestamp_ns: i64,
    /// Number of coalesced events since the last waiter read/drain.
    pub coalesced_count: u64,
}

/// Abstract hardware interrupt backend (for acknowledging virtual or physical IRQs).
pub trait InterruptBackend: Send {
    /// Acknowledges the interrupt on every path (Spec 17 step 6).
    fn acknowledge(&mut self) -> Result<(), BackendError> {
        Ok(())
    }
}

/// In-memory fake interrupt backend for deterministic tests.
#[derive(Clone, Debug, Default)]
pub struct FakeInterrupt {
    pub acks: usize,
}

impl FakeInterrupt {
    pub fn new() -> Self {
        Self::default()
    }
}

impl InterruptBackend for FakeInterrupt {
    fn acknowledge(&mut self) -> Result<(), BackendError> {
        self.acks += 1;
        Ok(())
    }
}

/// A pending waiter on an interrupt resource.
struct Waiter {
    after_sequence: u64,
    sender: Box<dyn FnOnce(InterruptEvent) + Send>,
}

/// State tracking for one interrupt resource.
pub struct InterruptState {
    pub resource: ResourceId,
    pub sequence: u64,
    pub count: u64,
    pub last_timestamp_ns: i64,
    pub coalesced_count: u64,
    waiters: Vec<Waiter>,
}

impl InterruptState {
    pub fn new(resource: ResourceId) -> Self {
        Self {
            resource,
            sequence: 0,
            count: 0,
            last_timestamp_ns: 0,
            coalesced_count: 0,
            waiters: Vec::new(),
        }
    }

    /// Current event snapshot.
    pub fn current_event(&self) -> InterruptEvent {
        InterruptEvent {
            resource: self.resource,
            sequence: self.sequence,
            count: self.count,
            timestamp_ns: self.last_timestamp_ns,
            coalesced_count: self.coalesced_count,
        }
    }
}

/// Central interrupt manager coordinating interrupt observations and waiters.
pub struct InterruptManager {
    resources: BTreeMap<ResourceId, InterruptState>,
}

impl InterruptManager {
    pub fn new() -> Self {
        Self { resources: BTreeMap::new() }
    }

    /// Registers an offered interrupt resource.
    pub fn register(&mut self, resource: ResourceId) {
        self.resources.entry(resource).or_insert_with(|| InterruptState::new(resource));
    }

    /// Checks if a resource is registered.
    pub fn is_registered(&self, resource: ResourceId) -> bool {
        self.resources.contains_key(&resource)
    }

    /// Triggers an interrupt event on `resource`.
    ///
    /// Increments sequence and count, updates timestamp, calculates coalesced count,
    /// satisfies all eligible waiters, and invokes their callbacks (Spec 17).
    pub fn on_interrupt(
        &mut self,
        resource: ResourceId,
        timestamp_ns: i64,
        backend: Option<&mut dyn InterruptBackend>,
    ) -> Result<InterruptEvent, Denial> {
        let state = self.resources.get_mut(&resource).ok_or(Denial::UnknownResource)?;
        state.sequence += 1;
        state.count += 1;
        state.last_timestamp_ns = timestamp_ns;

        let mut satisfied = Vec::new();
        let mut remaining = Vec::new();
        for waiter in state.waiters.drain(..) {
            if state.sequence > waiter.after_sequence {
                satisfied.push(waiter);
            } else {
                remaining.push(waiter);
            }
        }
        state.waiters = remaining;

        if satisfied.is_empty() {
            state.coalesced_count += 1;
        } else {
            state.coalesced_count = 0;
        }

        let event = state.current_event();

        for waiter in satisfied {
            (waiter.sender)(event);
        }

        if let Some(backend) = backend {
            let _ = backend.acknowledge();
        }

        Ok(event)
    }

    /// Requests an interrupt observation.
    ///
    /// If `state.sequence > after_sequence`, returns `Ok(Some(event))` immediately.
    /// Otherwise, registers the callback to be notified when the next event arrives.
    pub fn wait_for_interrupt(
        &mut self,
        resource: ResourceId,
        after_sequence: u64,
        sender: Box<dyn FnOnce(InterruptEvent) + Send>,
    ) -> Result<Option<InterruptEvent>, Denial> {
        let state = self.resources.get_mut(&resource).ok_or(Denial::UnknownResource)?;
        if state.sequence > after_sequence {
            Ok(Some(state.current_event()))
        } else {
            state.waiters.push(Waiter { after_sequence, sender });
            Ok(None)
        }
    }

    /// Clears all pending waiters (for example during session close or PrepareStop).
    pub fn cancel_waiters(&mut self) {
        for state in self.resources.values_mut() {
            state.waiters.clear();
        }
    }

    /// Number of active waiters for a given resource.
    pub fn waiter_count(&self, resource: ResourceId) -> usize {
        self.resources.get(&resource).map(|s| s.waiters.len()).unwrap_or(0)
    }
}

impl std::fmt::Debug for InterruptState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InterruptState")
            .field("resource", &self.resource)
            .field("sequence", &self.sequence)
            .field("count", &self.count)
            .field("last_timestamp_ns", &self.last_timestamp_ns)
            .field("coalesced_count", &self.coalesced_count)
            .field("waiters", &self.waiters.len())
            .finish()
    }
}

impl std::fmt::Debug for InterruptManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InterruptManager").field("resources", &self.resources).finish()
    }
}

impl Default for InterruptManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    #[test]
    fn test_interrupt_registration_and_trigger() {
        let mut manager = InterruptManager::new();
        manager.register(1);
        assert!(manager.is_registered(1));
        assert!(!manager.is_registered(2));

        let mut fake_backend = FakeInterrupt::new();
        let event = manager.on_interrupt(1, 100_000, Some(&mut fake_backend)).unwrap();
        assert_eq!(event.resource, 1);
        assert_eq!(event.sequence, 1);
        assert_eq!(event.count, 1);
        assert_eq!(event.timestamp_ns, 100_000);
        assert_eq!(event.coalesced_count, 1);
        assert_eq!(fake_backend.acks, 1);
    }

    #[test]
    fn test_interrupt_immediate_wait_when_sequence_greater() {
        let mut manager = InterruptManager::new();
        manager.register(1);
        manager.on_interrupt(1, 100_000, None).unwrap();

        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();
        let immediate = manager
            .wait_for_interrupt(
                1,
                0,
                Box::new(move |_| {
                    called_clone.store(true, Ordering::SeqCst);
                }),
            )
            .unwrap();

        assert!(immediate.is_some());
        let event = immediate.unwrap();
        assert_eq!(event.sequence, 1);
        assert!(!called.load(Ordering::SeqCst));
    }

    #[test]
    fn test_interrupt_waiter_satisfied_on_trigger() {
        let mut manager = InterruptManager::new();
        manager.register(1);

        let received = Arc::new(Mutex::new(None));
        let received_clone = received.clone();
        let immediate = manager
            .wait_for_interrupt(
                1,
                0,
                Box::new(move |event| {
                    *received_clone.lock().unwrap() = Some(event);
                }),
            )
            .unwrap();

        assert!(immediate.is_none());
        assert_eq!(manager.waiter_count(1), 1);

        let mut fake_backend = FakeInterrupt::new();
        manager.on_interrupt(1, 200_000, Some(&mut fake_backend)).unwrap();

        assert_eq!(manager.waiter_count(1), 0);
        let event = received.lock().unwrap().take().expect("waiter should receive event");
        assert_eq!(event.sequence, 1);
        assert_eq!(event.count, 1);
        assert_eq!(event.timestamp_ns, 200_000);
        assert_eq!(event.coalesced_count, 0);
        assert_eq!(fake_backend.acks, 1);
    }

    #[test]
    fn test_interrupt_cancel_waiters() {
        let mut manager = InterruptManager::new();
        manager.register(1);

        manager.wait_for_interrupt(1, 0, Box::new(|_| {})).unwrap();
        assert_eq!(manager.waiter_count(1), 1);

        manager.cancel_waiters();
        assert_eq!(manager.waiter_count(1), 0);
    }
}
