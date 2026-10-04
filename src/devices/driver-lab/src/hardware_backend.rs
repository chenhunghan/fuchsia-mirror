// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Hardware access abstraction for the executor.
//!
//! The real driver implements [`MmioBackend`] over a locally mapped MMIO
//! region; tests and driver realm fakes use [`FakeMmio`]. Keeping the trait
//! in the core crate lets the entire read path run on the build host.

use std::collections::{BTreeMap, BTreeSet};

/// Errors surfaced by a hardware backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendError {
    /// The backend could not complete the access; the proxy treats this as
    /// fatal backend state for the current operation.
    Fault,
}

/// A source of target monotonic time, injected so core logic is
/// deterministic under test. The driver supplies zx monotonic time.
pub trait Clock {
    /// Current timestamp in nanoseconds.
    fn now_ns(&mut self) -> i64;
}

/// An asynchronous timer for delays and polling.
pub trait Timer: Clock + Send {
    /// Asynchronously sleeps for `duration_ns` nanoseconds.
    fn sleep(&mut self, duration_ns: i64) -> impl std::future::Future<Output = ()> + Send;
}

/// Deterministic test clock advancing by a fixed step per reading.
#[derive(Clone, Debug, Default)]
pub struct FakeClock {
    /// Timestamp returned by the next call.
    pub now: i64,
    /// Amount added after each call.
    pub step: i64,
    /// Record of sleeps requested.
    pub sleeps: Vec<i64>,
}

impl FakeClock {
    /// Creates a fake clock.
    pub fn new(now: i64, step: i64) -> Self {
        Self { now, step, sleeps: Vec::new() }
    }
}

impl Clock for FakeClock {
    fn now_ns(&mut self) -> i64 {
        let now = self.now;
        self.now += self.step;
        now
    }
}

impl Timer for FakeClock {
    async fn sleep(&mut self, duration_ns: i64) {
        if duration_ns > 0 {
            self.now += duration_ns;
            self.sleeps.push(duration_ns);
        }
    }
}

/// Volatile 32-bit MMIO access to one resource.
pub trait MmioBackend {
    /// Performs one volatile 32-bit read at `offset` bytes from the start
    /// of the logical resource.
    fn read32(&mut self, offset: u64) -> Result<u32, BackendError>;

    /// Performs one volatile 32-bit write at `offset` bytes from the start
    /// of the logical resource.
    fn write32(&mut self, offset: u64, value: u32) -> Result<(), BackendError>;

    /// Applies a platform MMIO memory barrier.
    fn barrier(&mut self);
}

/// In-memory fake MMIO region that records every access, for unit tests
/// and driver realm fakes. Reads of unset offsets return zero.
#[derive(Clone, Debug, Default)]
pub struct FakeMmio {
    values: BTreeMap<u64, u32>,
    faults: BTreeSet<u64>,
    /// Offsets read, in order.
    pub accesses: Vec<u64>,
    /// Writes performed, in order: (offset, value).
    pub write_accesses: Vec<(u64, u32)>,
    /// Number of memory barriers performed.
    pub barriers: usize,
}

impl FakeMmio {
    /// Creates an empty fake region.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the value returned for reads at `offset`.
    pub fn set(&mut self, offset: u64, value: u32) {
        self.values.insert(offset, value);
    }

    /// Makes reads or writes at `offset` fail with [`BackendError::Fault`].
    pub fn fail_at(&mut self, offset: u64) {
        self.faults.insert(offset);
    }
}

impl MmioBackend for FakeMmio {
    fn read32(&mut self, offset: u64) -> Result<u32, BackendError> {
        self.accesses.push(offset);
        if self.faults.contains(&offset) {
            return Err(BackendError::Fault);
        }
        Ok(self.values.get(&offset).copied().unwrap_or(0))
    }

    fn write32(&mut self, offset: u64, value: u32) -> Result<(), BackendError> {
        self.write_accesses.push((offset, value));
        if self.faults.contains(&offset) {
            return Err(BackendError::Fault);
        }
        self.values.insert(offset, value);
        Ok(())
    }

    fn barrier(&mut self) {
        self.barriers += 1;
    }
}
