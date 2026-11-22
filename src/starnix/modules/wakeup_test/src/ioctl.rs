// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use linux_uapi::{_IOC_NRMASK, _IOC_NRSHIFT};
use std::fmt;
use zerocopy::{FromBytes, Immutable, KnownLayout};

/// Maximum number of scheduled touch or swipe events allowed in a single `WakeupSetTimers` ioctl.
pub const MAX_WAKEUP_EVENTS: u32 = 1000;

/// Maximum number of scheduled [`WakeupMethod::PowerButton`] events allowed in a single
/// `WakeupSetTimers` ioctl.
///
/// Each scheduled `PowerButton` event performs two sequential FIDL round-trips during synchronous
/// `ioctl` setup and registers a distinct virtual `MediaButtonsDevice` and `WakeAlarms` slot in
/// `input_helper`.
pub const MAX_POWER_BUTTON_EVENTS: u32 = 64;

/// Error returned when converting an unrecognized raw `i32` discriminant into [`WakeupMethod`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct InvalidWakeupMethod(pub i32);

impl fmt::Display for InvalidWakeupMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid wakeup method value: {}", self.0)
    }
}

impl std::error::Error for InvalidWakeupMethod {}

/// Input gesture or button action scheduled by a `WakeupSetTimers` `ioctl`.
///
/// Only [`Self::PowerButton`] programs a hardware `WakeAlarms` wake source via `input_helper`.
/// [`Self::WakeupByTouch`] and the `WakeupBySwipe*` variants use `fuchsia_async` boot timers
/// without a wake lease, so standalone touch or swipe schedules do not wake a suspended device on
/// their own; they fire while the device is awake or once another wake source (such as a preceding
/// [`Self::PowerButton`] schedule) resumes the container.
///
/// When [`WakeupTimerInfo::num_events`] `> 1`, odd-indexed (`1, 3, ...`) swipe events reverse
/// `start` and `end` (returning scrollable UI toward its starting position) and odd-indexed tap
/// events shift `x` by `2` pixels (avoiding double-tap detection), while traces and logs continue
/// to report the configured `WakeupMethod` name.
#[repr(i32)]
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
pub enum WakeupMethod {
    #[default]
    PowerButton = 0,
    WakeupByTouch = 1,
    WakeupBySwipeUp = 2,
    WakeupBySwipeDown = 3,
    WakeupBySwipeLeft = 4,
    WakeupBySwipeRight = 5,
}

impl WakeupMethod {
    /// Converts a raw `i32` from userspace into a [`WakeupMethod`], returning `None` for
    /// out-of-range discriminants.
    pub const fn from_raw(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::PowerButton),
            1 => Some(Self::WakeupByTouch),
            2 => Some(Self::WakeupBySwipeUp),
            3 => Some(Self::WakeupBySwipeDown),
            4 => Some(Self::WakeupBySwipeLeft),
            5 => Some(Self::WakeupBySwipeRight),
            _ => None,
        }
    }

    /// Returns the canonical trace name for this [`WakeupMethod`].
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PowerButton => "WakeupByPowerButton",
            Self::WakeupByTouch => "WakeupByTouch",
            Self::WakeupBySwipeUp => "WakeupBySwipeUp",
            Self::WakeupBySwipeDown => "WakeupBySwipeDown",
            Self::WakeupBySwipeLeft => "WakeupBySwipeLeft",
            Self::WakeupBySwipeRight => "WakeupBySwipeRight",
        }
    }
}

impl TryFrom<i32> for WakeupMethod {
    type Error = InvalidWakeupMethod;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        Self::from_raw(value).ok_or(InvalidWakeupMethod(value))
    }
}

impl From<WakeupMethod> for &'static str {
    fn from(method: WakeupMethod) -> Self {
        method.as_str()
    }
}

/// Different wakeup CUJ test types started from user space.
#[repr(i32)]
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
pub enum WakeupTestType {
    #[default]
    TestRegularWakeup = 0,
    TestCujSwipeQss = 1,
    TestCujSwipeTile = 2,
    TestCujSwipeNotification = 3,
    TestCujTapToApp = 4,
}

impl WakeupTestType {
    /// Returns the canonical trace name for this [`WakeupTestType`].
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TestRegularWakeup => "TestRegularWakeup",
            Self::TestCujSwipeQss => "TestCujSwipeQss",
            Self::TestCujSwipeTile => "TestCujSwipeTile",
            Self::TestCujSwipeNotification => "TestCujSwipeNotification",
            Self::TestCujTapToApp => "TestCujTapToApp",
        }
    }
}

impl From<u32> for WakeupTestType {
    fn from(value: u32) -> Self {
        match value {
            0 => Self::TestRegularWakeup,
            1 => Self::TestCujSwipeQss,
            2 => Self::TestCujSwipeTile,
            3 => Self::TestCujSwipeNotification,
            4 => Self::TestCujTapToApp,
            _ => Self::TestRegularWakeup,
        }
    }
}

impl From<WakeupTestType> for &'static str {
    fn from(test_type: WakeupTestType) -> Self {
        test_type.as_str()
    }
}

/// Userspace payload for the `WakeupSetTimers` (`ioctl` number 1) command on `/dev/wakeup_test0`.
///
/// Callers must run at most one touch or swipe schedule at a time: concurrent touch
/// `WakeupSetTimers` schedules are not serialized and share both the cached virtual touchscreen
/// and `DEFAULT_TOUCH_CONTACT_ID`.
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, FromBytes, Immutable, KnownLayout)]
pub struct WakeupTimerInfo {
    /// Protocol version reported by the userspace caller.
    pub version: i64,
    /// Raw [`WakeupTestType`] discriminant identifying the test scenario for trace annotations.
    pub test_type: u32,
    /// Interval in nanoseconds between consecutive events when `num_events > 1` (`>= 0`).
    pub interval: i64,
    /// Delay in nanoseconds (`>= 0`) before the first scheduled event.
    ///
    /// For standalone schedules, this is measured from the `ioctl` call on `ZX_CLOCK_BOOT`. When a
    /// preceding [`WakeupMethod::PowerButton`] wake is still pending (`now_boot <
    /// power_target_boot`) and a touch or swipe `ioctl` specifies `offset >= power_offset`, both
    /// `offset`s share the same reference (such as the start of a CUJ/CLI invocation) and the
    /// first touch or swipe fires `offset - power_offset` after actual container resume,
    /// eliminating inter-`ioctl` setup latency and hardware RTC rounding.
    pub offset: i64,
    /// Raw [`WakeupMethod`] discriminant (`0..=5`); out-of-range values fail with `EINVAL`.
    pub method: i32,
    /// Number of input events to fire: `1..=MAX_POWER_BUTTON_EVENTS` (`64`) for
    /// [`WakeupMethod::PowerButton`], or `1..=MAX_WAKEUP_EVENTS` (`1000`) for touch and swipe
    /// methods.
    pub num_events: u32,
    /// Tap or swipe X anchor coordinate. If both `coord_x` and `coord_y` are `0`, defaults to
    /// the horizontal center of the virtual screen (`max_x / 2`).
    pub coord_x: u32,
    /// Tap or swipe Y anchor coordinate. If both `coord_x` and `coord_y` are `0`, defaults to
    /// the vertical center of the virtual screen (`max_y / 2`).
    pub coord_y: u32,
    /// Optional gesture duration override in milliseconds. When `> 0`, overrides the default
    /// `15 ms` tap hold duration or `80 ms` swipe duration.
    pub dev_option: u32,
    /// Optional virtual touchscreen width. Both `screen_x` and `screen_y` must either be `0`
    /// (defaulting to `[0, 1000] x [0, 1000]`) or both be `>= 2` to register `[0, screen_x] x
    /// [0, screen_y]`; mismatched or `< 2` custom dimensions fail with `EINVAL`.
    pub screen_x: u32,
    /// Optional virtual touchscreen height. Must be `0` when `screen_x == 0`, or `>= 2` when
    /// `screen_x >= 2`.
    pub screen_y: u32,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CommandCode {
    WakeupSetTimers = 1,
    WakeupHowManyTimers = 2,
    WakeupCancelTimers = 3,
    WakeupTest = 4,
    Unknown,
}

impl From<u32> for CommandCode {
    fn from(value: u32) -> Self {
        match (value & _IOC_NRMASK) >> _IOC_NRSHIFT {
            1 => Self::WakeupSetTimers,
            2 => Self::WakeupHowManyTimers,
            3 => Self::WakeupCancelTimers,
            4 => Self::WakeupTest,
            _ => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CommandCode, InvalidWakeupMethod, WakeupMethod, WakeupTestType, WakeupTimerInfo};
    use linux_uapi::_IOC_NRSHIFT;
    use zerocopy::FromBytes;

    #[fuchsia::test]
    fn wakeup_method_try_from_validates_discriminants() {
        assert_eq!(WakeupMethod::try_from(0), Ok(WakeupMethod::PowerButton));
        assert_eq!(WakeupMethod::try_from(1), Ok(WakeupMethod::WakeupByTouch));
        assert_eq!(WakeupMethod::try_from(2), Ok(WakeupMethod::WakeupBySwipeUp));
        assert_eq!(WakeupMethod::try_from(3), Ok(WakeupMethod::WakeupBySwipeDown));
        assert_eq!(WakeupMethod::try_from(4), Ok(WakeupMethod::WakeupBySwipeLeft));
        assert_eq!(WakeupMethod::try_from(5), Ok(WakeupMethod::WakeupBySwipeRight));

        assert_eq!(WakeupMethod::try_from(-1), Err(InvalidWakeupMethod(-1)));
        assert_eq!(WakeupMethod::try_from(6), Err(InvalidWakeupMethod(6)));
        assert_eq!(WakeupMethod::try_from(i32::MAX), Err(InvalidWakeupMethod(i32::MAX)));
        assert_eq!(WakeupMethod::from_raw(99), None);
    }

    #[fuchsia::test]
    fn wakeup_method_trace_names_match_expected() {
        let cases = [
            (WakeupMethod::PowerButton, "WakeupByPowerButton"),
            (WakeupMethod::WakeupByTouch, "WakeupByTouch"),
            (WakeupMethod::WakeupBySwipeUp, "WakeupBySwipeUp"),
            (WakeupMethod::WakeupBySwipeDown, "WakeupBySwipeDown"),
            (WakeupMethod::WakeupBySwipeLeft, "WakeupBySwipeLeft"),
            (WakeupMethod::WakeupBySwipeRight, "WakeupBySwipeRight"),
        ];
        for (method, expected) in cases {
            let actual: &'static str = method.into();
            assert_eq!(actual, expected);
        }
    }

    #[fuchsia::test]
    fn wakeup_test_type_conversions() {
        assert_eq!(WakeupTestType::from(0), WakeupTestType::TestRegularWakeup);
        assert_eq!(WakeupTestType::from(1), WakeupTestType::TestCujSwipeQss);
        assert_eq!(WakeupTestType::from(2), WakeupTestType::TestCujSwipeTile);
        assert_eq!(WakeupTestType::from(3), WakeupTestType::TestCujSwipeNotification);
        assert_eq!(WakeupTestType::from(4), WakeupTestType::TestCujTapToApp);
        assert_eq!(WakeupTestType::from(99), WakeupTestType::TestRegularWakeup);

        let name: &'static str = WakeupTestType::TestCujSwipeTile.into();
        assert_eq!(name, "TestCujSwipeTile");
    }

    #[fuchsia::test]
    fn command_code_extracts_ioc_nr() {
        assert_eq!(CommandCode::from(1 << _IOC_NRSHIFT), CommandCode::WakeupSetTimers);
        assert_eq!(CommandCode::from(2 << _IOC_NRSHIFT), CommandCode::WakeupHowManyTimers);
        assert_eq!(CommandCode::from(3 << _IOC_NRSHIFT), CommandCode::WakeupCancelTimers);
        assert_eq!(CommandCode::from(4 << _IOC_NRSHIFT), CommandCode::WakeupTest);
        assert_eq!(CommandCode::from(0 << _IOC_NRSHIFT), CommandCode::Unknown);
        assert_eq!(CommandCode::from(5 << _IOC_NRSHIFT), CommandCode::Unknown);
    }

    #[fuchsia::test]
    fn wakeup_timer_info_layout_and_zero_decode() {
        assert_eq!(std::mem::size_of::<WakeupTimerInfo>(), 64);
        let zero_bytes = [0u8; 64];
        let decoded = WakeupTimerInfo::read_from_bytes(&zero_bytes).expect("valid 64-byte layout");
        assert_eq!(decoded, WakeupTimerInfo::default());
    }
}
