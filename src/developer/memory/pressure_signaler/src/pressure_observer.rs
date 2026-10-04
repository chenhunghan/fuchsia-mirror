// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_memorypressure as fmp;
use fuchsia_async as fasync;
use futures::FutureExt as _;
use futures::future::select_all;
use log::{error, info};

const NUM_LEVELS: usize = 4;

/// Kernel memory pressure levels observed by [`PressureObserver`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[repr(usize)]
pub enum Level {
    ImminentOom = 0,
    Critical = 1,
    Warning = 2,
    Normal = 3,
}

impl Level {
    /// All levels in discriminant order. `PressureObserver::events` is indexed by `level as usize`.
    pub const ALL: [Self; NUM_LEVELS] =
        [Self::ImminentOom, Self::Critical, Self::Warning, Self::Normal];

    const fn event_kind(self) -> zx::SystemEventKind {
        match self {
            Self::ImminentOom => zx::SystemEventKind::ImminentOutOfMemory,
            Self::Critical => zx::SystemEventKind::MemoryPressureCritical,
            Self::Warning => zx::SystemEventKind::MemoryPressureWarning,
            Self::Normal => zx::SystemEventKind::MemoryPressureNormal,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::ImminentOom => "IMMINENT-OOM",
            Self::Critical => "CRITICAL",
            Self::Warning => "WARNING",
            Self::Normal => "NORMAL",
        }
    }

    /// Maps this internal level to the FIDL level exposed to `fuchsia.memorypressure` watchers.
    ///
    /// Watchers do not recognize `ImminentOom`; the highest level they can receive is `Critical`.
    pub const fn for_watcher(self) -> fmp::Level {
        match self {
            Self::ImminentOom | Self::Critical => fmp::Level::Critical,
            Self::Warning => fmp::Level::Warning,
            Self::Normal => fmp::Level::Normal,
        }
    }
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<fmp::Level> for Level {
    fn from(level: fmp::Level) -> Self {
        match level {
            fmp::Level::Critical => Self::Critical,
            fmp::Level::Warning => Self::Warning,
            fmp::Level::Normal => Self::Normal,
        }
    }
}

/// Provider of kernel system events for memory pressure levels.
pub trait GetSystemEvent {
    fn get_system_event(&self, kind: zx::SystemEventKind) -> Result<zx::Event, zx::Status>;
}

impl GetSystemEvent for zx::Job {
    fn get_system_event(&self, kind: zx::SystemEventKind) -> Result<zx::Event, zx::Status> {
        zx::system_get_event(self, kind)
    }
}

/// Observes kernel memory pressure events and reports level transitions.
#[derive(Debug)]
pub struct PressureObserver {
    events: [zx::Event; NUM_LEVELS],
    /// `None` until the first kernel signal is observed.
    level: Option<Level>,
}

impl PressureObserver {
    /// Initializes a new [`PressureObserver`] by retrieving kernel memory pressure events from
    /// `provider`.
    pub fn new(provider: &impl GetSystemEvent) -> Result<Self, zx::Status> {
        const { assert!(Level::ALL.len() == NUM_LEVELS, "Level::ALL length must match NUM_LEVELS") };
        let mut events = Vec::with_capacity(NUM_LEVELS);
        for level in Level::ALL {
            let event = provider.get_system_event(level.event_kind()).inspect_err(|status| {
                error!("get_system_event [{level}] returned {status}");
            })?;
            events.push(event);
        }

        // The conversion should never fail due to the const assert above.
        let events = events.try_into().expect("Level::ALL should have NUM_LEVELS entries");
        Ok(Self { events, level: None })
    }

    #[cfg(test)]
    pub fn current_level(&self) -> Option<Level> {
        self.level
    }

    /// Waits for the kernel to signal a transition to a new memory pressure level.
    pub async fn wait_on_level_change(&mut self) -> Result<Level, zx::Status> {
        let futures = Level::ALL
            .into_iter()
            // Wait on all events the first time around.
            .filter(|&level| self.level != Some(level))
            .map(|level| {
                Box::pin(
                    fasync::OnSignals::new(
                        &self.events[level as usize],
                        zx::Signals::EVENT_SIGNALED,
                    )
                    .map(move |res| res.map(|_| level)),
                )
            });

        let (result, _, _) = select_all(futures).await;
        let new_level = result?;
        self.on_level_changed(new_level);
        Ok(new_level)
    }

    fn on_level_changed(&mut self, new_level: Level) {
        match self.level.replace(new_level) {
            None => info!("starting at {new_level}"),
            Some(old_level) => info!("{old_level} -> {new_level}"),
        }

        fuchsia_trace::counter!(
            c"memory:kernel",
            c"memory_pressure",
            0,
            "level" => new_level as u32
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use zx::Rights;

    struct FakeSystemEventProvider {
        events: [zx::Event; NUM_LEVELS],
    }

    impl FakeSystemEventProvider {
        fn new() -> Self {
            Self { events: std::array::from_fn(|_| zx::Event::create()) }
        }

        fn set_level(&self, target_kind: zx::SystemEventKind) {
            for level in Level::ALL {
                let event = &self.events[level as usize];
                if level.event_kind() == target_kind {
                    // Set the signal.
                    event
                        .signal(zx::Signals::NONE, zx::Signals::EVENT_SIGNALED)
                        .expect("signal failed");
                } else {
                    // Clear the signal.
                    event
                        .signal(zx::Signals::EVENT_SIGNALED, zx::Signals::NONE)
                        .expect("clear failed");
                }
            }
        }
    }

    impl GetSystemEvent for FakeSystemEventProvider {
        fn get_system_event(&self, kind: zx::SystemEventKind) -> Result<zx::Event, zx::Status> {
            let level =
                Level::ALL.into_iter().find(|l| l.event_kind() == kind).expect("Event not found");
            self.events[level as usize]
                .duplicate_handle(Rights::WAIT | Rights::DUPLICATE | Rights::TRANSFER)
        }
    }

    #[fuchsia::test]
    async fn events() {
        let fake = FakeSystemEventProvider::new();
        let observer = PressureObserver::new(&fake).expect("Failed to initialize observer");

        let mut koids = HashSet::with_capacity(NUM_LEVELS);
        for level in Level::ALL {
            let observer_koid = observer.events[level as usize]
                .koid()
                .expect("Failed to get koid for observer event");
            let expected_koid =
                fake.events[level as usize].koid().expect("Failed to get koid for fake event");
            assert_eq!(observer_koid, expected_koid);
            assert!(koids.insert(observer_koid));
        }
    }

    #[fuchsia::test]
    async fn initial_level() {
        let fake = FakeSystemEventProvider::new();
        fake.set_level(zx::SystemEventKind::MemoryPressureNormal);

        let mut observer = PressureObserver::new(&fake).expect("Failed to initialize observer");
        assert_eq!(observer.current_level(), None);
        let level =
            observer.wait_on_level_change().await.expect("Failed to wait on initial level change");
        assert_eq!(level, Level::Normal);
        assert_eq!(observer.current_level(), Some(Level::Normal));
    }

    #[fuchsia::test]
    async fn wait_on_events() {
        let fake = FakeSystemEventProvider::new();
        fake.set_level(zx::SystemEventKind::MemoryPressureNormal);

        let mut observer = PressureObserver::new(&fake).expect("Failed to initialize observer");
        let level =
            observer.wait_on_level_change().await.expect("Failed to wait on initial level change");
        assert_eq!(level, Level::Normal);

        // While Level::Normal is still asserted, wait_on_level_change must not return Normal;
        // it must only wait on the remaining (NUM_LEVELS - 1) events.
        {
            let mut wait_fut = std::pin::pin!(observer.wait_on_level_change());
            assert_eq!(futures::poll!(&mut wait_fut), std::task::Poll::Pending);

            // Signaled transition to Warning resolves the wait.
            fake.set_level(zx::SystemEventKind::MemoryPressureWarning);
            assert_eq!(futures::poll!(&mut wait_fut), std::task::Poll::Ready(Ok(Level::Warning)));
        }
        assert_eq!(observer.current_level(), Some(Level::Warning));

        // While Level::Warning is still asserted, subsequent wait is pending.
        {
            let mut wait_fut = std::pin::pin!(observer.wait_on_level_change());
            assert_eq!(futures::poll!(&mut wait_fut), std::task::Poll::Pending);

            // Signaled transition to Critical resolves the wait.
            fake.set_level(zx::SystemEventKind::MemoryPressureCritical);
            assert_eq!(futures::poll!(&mut wait_fut), std::task::Poll::Ready(Ok(Level::Critical)));
        }
        assert_eq!(observer.current_level(), Some(Level::Critical));

        // While Level::Critical is still asserted, subsequent wait is pending.
        {
            let mut wait_fut = std::pin::pin!(observer.wait_on_level_change());
            assert_eq!(futures::poll!(&mut wait_fut), std::task::Poll::Pending);

            // Signaled transition to ImminentOom resolves the wait.
            fake.set_level(zx::SystemEventKind::ImminentOutOfMemory);
            assert_eq!(
                futures::poll!(&mut wait_fut),
                std::task::Poll::Ready(Ok(Level::ImminentOom))
            );
        }
        assert_eq!(observer.current_level(), Some(Level::ImminentOom));

        // While Level::ImminentOom is still asserted, subsequent wait is pending.
        {
            let mut wait_fut = std::pin::pin!(observer.wait_on_level_change());
            assert_eq!(futures::poll!(&mut wait_fut), std::task::Poll::Pending);

            // Signaled transition back to Normal resolves the wait.
            fake.set_level(zx::SystemEventKind::MemoryPressureNormal);
            assert_eq!(futures::poll!(&mut wait_fut), std::task::Poll::Ready(Ok(Level::Normal)));
        }
        assert_eq!(observer.current_level(), Some(Level::Normal));
    }

    #[fuchsia::test]
    fn level_display() {
        assert_eq!(Level::Normal.to_string(), "NORMAL");
        assert_eq!(Level::Warning.to_string(), "WARNING");
        assert_eq!(Level::Critical.to_string(), "CRITICAL");
        assert_eq!(Level::ImminentOom.to_string(), "IMMINENT-OOM");
    }

    #[fuchsia::test]
    fn level_conversions() {
        assert_eq!(Level::from(fmp::Level::Normal), Level::Normal);
        assert_eq!(Level::from(fmp::Level::Warning), Level::Warning);
        assert_eq!(Level::from(fmp::Level::Critical), Level::Critical);

        assert_eq!(Level::Normal.for_watcher(), fmp::Level::Normal);
        assert_eq!(Level::Warning.for_watcher(), fmp::Level::Warning);
        assert_eq!(Level::Critical.for_watcher(), fmp::Level::Critical);
        assert_eq!(Level::ImminentOom.for_watcher(), fmp::Level::Critical);
    }
}
