// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_next_fuchsia_hardware_usb_connector::Orientation;
use std::fmt;

/// `Rp` current advertisement of a Source (USB Type-C R2.5 §4.6.2.1, Table 4-20).
///
/// Variants are ordered by increasing pull-up current strength (`Default < Rp1_5A < Rp3_0A`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum RpValue {
    /// Rp default: Default USB Power (`00b`).
    #[default]
    Default = 0,
    /// Rp 1.5A: USB Type-C Current @ 1.5 A (`01b`).
    Rp1_5A = 1,
    /// Rp 3.0A: USB Type-C Current @ 3.0 A (`10b`).
    Rp3_0A = 2,
}

impl RpValue {
    /// USB Type-C Current this `Rp` advertises, in microamps (R2.5 §4.6.2.1, Table 4-20), or `None`
    /// for Default USB Power, whose current depends on the USB version, and which USB BC 1.2
    /// supersedes.
    pub const fn typec_current_ua(self) -> Option<u32> {
        match self {
            Self::Default => None,
            Self::Rp1_5A => Some(1_500_000),
            Self::Rp3_0A => Some(3_000_000),
        }
    }

    /// Returns the Inspect label for this level.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Rp1_5A => "1.5A",
            Self::Rp3_0A => "3.0A",
        }
    }
}

/// CC pin termination presented by a port controller (`Ra`, `Rp`, `Rd`, or `Open`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CcTermination {
    /// `Ra` pull-down termination (`00b`).
    Ra = 0,
    /// `Rp` pull-up termination (`01b`).
    Rp = 1,
    /// `Rd` pull-down termination (`10b`).
    Rd = 2,
    /// Open / high-Z termination (`11b` when `drp = false`; autonomous DRP toggling configures
    /// both CC pins as `Rp` or both as `Rd`).
    #[default]
    Open = 3,
}

/// CC pin terminations and autonomous DRP toggle configuration presented by a port controller.
///
/// [`RoleControl::default()`] returns [`Self::STATIC_RD`], the power-on default for Sink-only and
/// dead-battery DRP ports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RoleControl {
    /// Whether autonomous DRP toggling between `Rp` and `Rd` is enabled.
    pub drp: bool,
    /// Source `Rp` current advertisement presented on `Rp` pins.
    pub rp_value: RpValue,
    /// Termination presented on `CC1` (or the DRP toggle seed when `drp` is `true`).
    pub cc1: CcTermination,
    /// Termination presented on `CC2` (or the DRP toggle seed when `drp` is `true`).
    pub cc2: CcTermination,
}

impl Default for RoleControl {
    fn default() -> Self {
        Self::STATIC_RD
    }
}

impl RoleControl {
    /// Forces both CC pins open (`High-Z`) with DRP toggling disabled.
    pub const FORCE_OPEN: Self = Self {
        drp: false,
        rp_value: RpValue::Default,
        cc1: CcTermination::Open,
        cc2: CcTermination::Open,
    };

    /// Presents static Sink `Rd` on both CC pins with DRP toggling disabled.
    pub const STATIC_RD: Self = Self {
        drp: false,
        rp_value: RpValue::Default,
        cc1: CcTermination::Rd,
        cc2: CcTermination::Rd,
    };

    /// Configures autonomous DRP toggling seeded with Sink `Rd` on both CC pins and Default `Rp`.
    pub const DRP_RD: Self = Self {
        drp: true,
        rp_value: RpValue::Default,
        cc1: CcTermination::Rd,
        cc2: CcTermination::Rd,
    };
}

/// Type-C Configuration Channel (CC) line state sensed on a single CC pin, named as in TCPCI
/// Rev 2.0 Ver 1.3 Table 4-22 (`CC_STATUS`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum CcPinState {
    /// `SRC.Open` or `SNK.Open`: nothing sensed, or invalid while `Looking4Connection` is set.
    #[default]
    Open = 0,
    /// `SNK.Default`: presenting `Rd`, sensing a Source's Rp for Default USB Power.
    SnkDefault = 1,
    /// `SNK.Power1.5`: presenting `Rd`, sensing a Source's Rp for 1.5 A.
    SnkPower1_5 = 2,
    /// `SNK.Power3.0`: presenting `Rd`, sensing a Source's Rp for 3.0 A.
    SnkPower3_0 = 3,
    /// `SRC.Ra`: presenting `Rp`, sensing a cable's or accessory's `Ra`.
    SrcRa = 4,
    /// `SRC.Rd`: presenting `Rp`, sensing a Sink's `Rd`.
    SrcRd = 5,
}

impl CcPinState {
    /// Returns `true` if the pin senses a Source `Rp` termination while presenting `Rd`.
    pub(crate) const fn is_sink_rp(self) -> bool {
        matches!(self, Self::SnkDefault | Self::SnkPower1_5 | Self::SnkPower3_0)
    }

    /// Returns the partner Source `Rp` current advertisement if this pin senses `Rp*`
    /// while presenting `Rd`.
    pub const fn rp_value(self) -> Option<RpValue> {
        match self {
            Self::SnkDefault => Some(RpValue::Default),
            Self::SnkPower1_5 => Some(RpValue::Rp1_5A),
            Self::SnkPower3_0 => Some(RpValue::Rp3_0A),
            _ => None,
        }
    }

    /// Returns `true` if this pin carries an active partner CC termination (`Rp*` or `SrcRd`).
    ///
    /// [`CcPinState::SrcRa`] indicates an active cable `Ra` (`VCONN`) load rather than the active
    /// CC signal wire, so it returns `false`.
    const fn is_cc_connected(self) -> bool {
        self.is_sink_rp() || matches!(self, Self::SrcRd)
    }

    /// Returns the spec name of this state (used in Inspect and logs).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::SnkDefault => "SNK.Default",
            Self::SnkPower1_5 => "SNK.Power1.5",
            Self::SnkPower3_0 => "SNK.Power3.0",
            Self::SrcRa => "SRC.Ra",
            Self::SrcRd => "SRC.Rd",
        }
    }
}

impl fmt::Display for CcPinState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Debug Accessory Mode detected when both `CC1` and `CC2` carry a partner termination (USB
/// Type-C R2.5 Appendix B).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Accessory {
    /// `DebugAccessory.SNK`: both `CC1` and `CC2` sense `Rp` (a DTS Source).
    DebugAccessorySnk,
    /// `UnorientedDebugAccessory.SRC`: both `CC1` and `CC2` sense `Rd` (a DTS Sink).
    UnorientedDebugAccessorySrc,
}

/// Type-C port power role resolved from CC line terminations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PowerRole {
    /// Port is acting as a power Sink (presenting `Rd`, partner presenting `Rp`).
    Sink,
    /// Port is acting as a power Source (presenting `Rp`, partner presenting `Rd`).
    Source,
}

/// Decoded snapshot of both Type-C CC pins and hardware connection-search status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CcStatus {
    /// Decoded state of the `CC1` pin.
    pub cc1: CcPinState,
    /// Decoded state of the `CC2` pin.
    pub cc2: CcPinState,
    /// Whether the TCPC is actively searching for a connection (`Looking4Connection == 1`).
    pub looking4connection: bool,
}

impl CcStatus {
    /// Port is actively searching / DRP toggling (`Looking4Connection == 1`, `CC1 = Open`,
    /// `CC2 = Open`).
    pub const LOOKING4CONNECTION: Self =
        Self { cc1: CcPinState::Open, cc2: CcPinState::Open, looking4connection: true };

    /// Creates a settled (`looking4connection = false`) [`CcStatus`] snapshot with `cc1` and
    /// `cc2`.
    pub const fn new(cc1: CcPinState, cc2: CcPinState) -> Self {
        Self { cc1, cc2, looking4connection: false }
    }

    /// Returns the [`Accessory`] indicated when both `CC1` and `CC2` carry a partner termination
    /// (`Rp/Rp` => [`Accessory::DebugAccessorySnk`], `SRC.Rd/SRC.Rd` =>
    /// [`Accessory::UnorientedDebugAccessorySrc`]), or `None` otherwise.
    pub const fn accessory(self) -> Option<Accessory> {
        if self.looking4connection {
            return None;
        }
        match (self.cc1, self.cc2) {
            (cc1, cc2) if cc1.is_sink_rp() && cc2.is_sink_rp() => {
                Some(Accessory::DebugAccessorySnk)
            }
            (CcPinState::SrcRd, CcPinState::SrcRd) => Some(Accessory::UnorientedDebugAccessorySrc),
            _ => None,
        }
    }

    /// Resolves the port's [`PowerRole`] if a supported partner termination is present.
    ///
    /// - Returns `Some(PowerRole::Sink)` when one CC pin senses `Rp*` and the other is `Open`, or
    ///   when both CC pins sense `Rp*` ([`Accessory::DebugAccessorySnk`]).
    /// - Returns `Some(PowerRole::Source)` when one CC pin senses `SrcRd` and the other is `Open`
    ///   or `SrcRa`.
    /// - Returns `None` when disconnected, searching, sensing invalid mixed terminations, or
    ///   sensing [`Accessory::UnorientedDebugAccessorySrc`] or `SRC.Ra/SRC.Ra`.
    pub const fn power_role(self) -> Option<PowerRole> {
        if self.looking4connection {
            return None;
        }
        match (self.cc1, self.cc2) {
            (cc, CcPinState::Open) | (CcPinState::Open, cc) if cc.is_sink_rp() => {
                Some(PowerRole::Sink)
            }
            (cc1, cc2) if cc1.is_sink_rp() && cc2.is_sink_rp() => Some(PowerRole::Sink),
            (CcPinState::SrcRd, CcPinState::Open | CcPinState::SrcRa)
            | (CcPinState::Open | CcPinState::SrcRa, CcPinState::SrcRd) => Some(PowerRole::Source),
            // TODO(https://fxbug.dev/575661321): Support `UnorientedDebugAccessory.SRC`
            // (`SRC.Rd/SRC.Rd`). `SRC.Ra/SRC.Ra` enters `AttachWait.SRC` but never attaches (R2.5
            // §4.5.2.2.7.2, §4.5.2.2.8.2).
            _ => None,
        }
    }

    /// Returns the [`PowerRole`] the port may attach as, or `None` for an `Rp/Rp` Sink Debug
    /// Accessory when `sink_debug_accessory_mode` is disabled.
    pub(crate) const fn attachable_role(
        self,
        sink_debug_accessory_mode: bool,
    ) -> Option<PowerRole> {
        if !sink_debug_accessory_mode
            && matches!(self.accessory(), Some(Accessory::DebugAccessorySnk))
        {
            None
        } else {
            self.power_role()
        }
    }

    /// Returns `true` if both CC pins sense `SRC.Ra`, which enters `AttachWait.SRC` without a
    /// path to `Attached.SRC` (R2.5 §4.5.2.2.7.2).
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "called by Machine::transition once wired to the async runner")
    )]
    pub(crate) const fn is_ra_ra(self) -> bool {
        !self.looking4connection
            && matches!((self.cc1, self.cc2), (CcPinState::SrcRa, CcPinState::SrcRa))
    }

    /// Returns `true` if `self` and `other` resolve to the same [`PowerRole`] and the exact same
    /// set of CC signal wires (`CC1`, `CC2`, or both for [`Accessory::DebugAccessorySnk`]) remain
    /// connected across both snapshots.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "called by Machine::transition once wired to the async runner")
    )]
    pub(crate) const fn shares_connected_cc_pin(self, other: Self) -> bool {
        matches!(
            (self.power_role(), other.power_role()),
            (Some(PowerRole::Sink), Some(PowerRole::Sink))
                | (Some(PowerRole::Source), Some(PowerRole::Source))
        ) && self.cc1.is_cc_connected() == other.cc1.is_cc_connected()
            && self.cc2.is_cc_connected() == other.cc2.is_cc_connected()
    }

    /// Returns `true` if a Source attached with `committed` has not seen `SRC.Open` on its single
    /// monitored CC pin (USB Type-C R2.5 §4.5.2.2.9.1–2).
    ///
    /// Without VCONN, only the connected CC pin from `committed` is monitored; terminations on the
    /// unmonitored pin or `SRC.Ra` on the monitored pin do not detach VBUS.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "called by Machine::transition once wired to the async runner")
    )]
    pub(crate) const fn keeps_attached_source(self, committed: Self) -> bool {
        let monitored = match (committed.cc1, committed.cc2) {
            (CcPinState::SrcRd, CcPinState::Open | CcPinState::SrcRa) => self.cc1,
            (CcPinState::Open | CcPinState::SrcRa, CcPinState::SrcRd) => self.cc2,
            _ => return false,
        };
        !self.looking4connection && matches!(monitored, CcPinState::SrcRd | CcPinState::SrcRa)
    }

    /// Returns the current a Sink may draw from the partner Source's `Rp` (USB Type-C R2.5
    /// §4.6.2.1), or `None` if not attached as a Sink.
    ///
    /// A DTS Source (`Rp/Rp`) encodes its current in the pair of `Rp` values (R2.5 §B.2.4.2.1
    /// Table B-2); any other pair means Default USB Power (§B.2.4.2.2.1).
    pub const fn sink_rp_level(self) -> Option<RpValue> {
        if !matches!(self.power_role(), Some(PowerRole::Sink)) {
            return None;
        }
        match (self.cc1.rp_value(), self.cc2.rp_value()) {
            (Some(cc1), Some(cc2)) => {
                let (stronger, weaker) =
                    if (cc1 as u8) >= (cc2 as u8) { (cc1, cc2) } else { (cc2, cc1) };
                Some(match (stronger, weaker) {
                    (RpValue::Rp1_5A, RpValue::Default) => RpValue::Rp1_5A,
                    (RpValue::Rp3_0A, RpValue::Default) => RpValue::Rp3_0A,
                    _ => RpValue::Default,
                })
            }
            (rp, None) | (None, rp) => rp,
        }
    }

    /// Resolves the Type-C cable plug [`Orientation`] from the CC pin states.
    ///
    /// - For a standard Sink or Source connection, the pin carrying `Rp*` or `SrcRd` determines
    ///   orientation (`CC1 => Normal`, `CC2 => Reverse`).
    /// - For an oriented `DebugAccessory.SNK` (`Rp` on both pins with `CC1 != CC2`), the pin with
    ///   the stronger `Rp` termination determines orientation (`CC1 > CC2 => Normal`,
    ///   `CC2 > CC1 => Reverse`, R2.5 §B.2.6.1.1); equal `Rp` values resolve to `None`.
    pub const fn orientation(self) -> Option<Orientation> {
        if self.power_role().is_none() {
            return None;
        }
        match (self.cc1.is_cc_connected(), self.cc2.is_cc_connected()) {
            (true, false) => Some(Orientation::Normal),
            (false, true) => Some(Orientation::Reverse),
            _ if (self.cc1 as u8) > (self.cc2 as u8) => Some(Orientation::Normal),
            _ if (self.cc2 as u8) > (self.cc1 as u8) => Some(Orientation::Reverse),
            _ => None,
        }
    }
}
