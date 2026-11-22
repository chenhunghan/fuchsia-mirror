// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#![warn(missing_docs, unsafe_op_in_unsafe_fn)]
//! A transport-agnostic library for implementing a vsock bridge over a usb bulk device.

mod connection;
mod packet;
/// In-memory byte transport implementations.
pub mod transport;

pub use connection::*;
pub use packet::*;
pub use transport::MpscTransport;

/// Protocol version. This determines what features a connection supports.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum ProtocolVersion {
    /// Protocol version 0
    V0,
    /// Protocol version 1
    V1,
    /// Protocol version 2
    V2,
}

impl ProtocolVersion {
    /// The latest protocol version.
    pub const LATEST: ProtocolVersion = ProtocolVersion::V2;

    /// Whether we support the pause protocol message.
    pub(crate) fn has_pause_packets(&self) -> bool {
        matches!(self, ProtocolVersion::V1 | ProtocolVersion::V2)
    }

    fn as_u32(&self) -> u32 {
        match self {
            ProtocolVersion::V0 => 0,
            ProtocolVersion::V1 => 1,
            ProtocolVersion::V2 => 2,
        }
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_u32())
    }
}

/// Magic payload of a sync packet, encoding a known protocol version and an
/// optional 32-bit nonce.
///
/// The format is `vsock:0` for version 0, `vsock:1` for version 1, and
/// `vsock:2:<nonce>` (with `<nonce>` as a 32-bit unsigned integer in
/// hexadecimal) for version 2.
///
/// A nonce of `0` in version 2 (`vsock:2:0`) indicates a reset (for example,
/// when the target does not recognize a newer host protocol's magic format and
/// requests that the host reset and downgrade to version 2).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct SyncMagic {
    version: ProtocolVersion,
    nonce: Option<u32>,
}

impl SyncMagic {
    /// Construct a [`SyncMagic`] for protocol version 0 (`vsock:0`).
    pub fn v0() -> Self {
        Self { version: ProtocolVersion::V0, nonce: None }
    }

    /// Construct a [`SyncMagic`] for protocol version 1 (`vsock:1`).
    pub fn v1() -> Self {
        Self { version: ProtocolVersion::V1, nonce: None }
    }

    /// Construct a [`SyncMagic`] for protocol version 2 with a non-zero
    /// session nonce (`vsock:2:<nonce>`).
    pub fn v2(nonce: std::num::NonZeroU32) -> Self {
        Self { version: ProtocolVersion::V2, nonce: Some(nonce.get()) }
    }

    /// Construct a reset [`SyncMagic`] for protocol version 2 (`vsock:2:0`).
    pub fn v2_reset() -> Self {
        Self { version: ProtocolVersion::V2, nonce: Some(0) }
    }

    /// Construct a [`SyncMagic`] for [`ProtocolVersion::LATEST`] with a
    /// non-zero session nonce.
    pub fn latest(nonce: std::num::NonZeroU32) -> Self {
        Self::v2(nonce)
    }

    /// Construct a reset [`SyncMagic`] for [`ProtocolVersion::LATEST`].
    pub fn latest_reset() -> Self {
        Self::v2_reset()
    }

    /// Returns `true` if this sync magic requests a reset (i.e. has a nonce of `0`).
    pub fn is_reset(&self) -> bool {
        self.nonce == Some(0)
    }

    /// Encode this sync magic into its wire byte representation.
    pub fn magic(&self) -> Vec<u8> {
        match self.nonce {
            Some(nonce) => format!("vsock:{}:{nonce:x}", self.version).into_bytes(),
            None => format!("vsock:{}", self.version).into_bytes(),
        }
    }

    /// Derive a [`SyncMagic`] from the magic bytes sent in a sync packet.
    ///
    /// Returns `None` if the magic is malformed or for an unknown protocol
    /// version.
    pub fn from_magic(magic: &[u8]) -> Option<Self> {
        if magic == b"vsock:0" {
            Some(Self { version: ProtocolVersion::V0, nonce: None })
        } else if magic == b"vsock:1" {
            Some(Self { version: ProtocolVersion::V1, nonce: None })
        } else if let Some(rest) = magic.strip_prefix(b"vsock:2:") {
            let s = std::str::from_utf8(rest).ok()?;
            if s.is_empty() || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let nonce = u32::from_str_radix(s, 16).ok()?;
            Some(Self { version: ProtocolVersion::V2, nonce: Some(nonce) })
        } else {
            None
        }
    }

    /// Returns the [`ProtocolVersion`] encoded in this sync magic.
    pub fn version(&self) -> ProtocolVersion {
        self.version
    }

    /// Returns the non-zero session nonce carried in this sync magic, or `None`
    /// if the version does not use nonces or this is a reset magic.
    pub fn nonce(&self) -> Option<std::num::NonZeroU32> {
        self.nonce.and_then(std::num::NonZeroU32::new)
    }

    /// Given `self` is a non-reset magic sent by the host as it connects and
    /// `supported` is the highest [`ProtocolVersion`] the target supports,
    /// return the negotiated [`ProtocolVersion`] and the [`SyncMagic`] to send
    /// in reply (preserving the host's nonce when applicable).
    ///
    /// Returns `None` if `self` is a reset magic (`is_reset()`).
    pub fn negotiate(&self, supported: ProtocolVersion) -> Option<(ProtocolVersion, SyncMagic)> {
        if self.is_reset() {
            return None;
        }
        if self.version == ProtocolVersion::V0 || supported == ProtocolVersion::V0 {
            Some((ProtocolVersion::V0, SyncMagic::v0()))
        } else if self.version == ProtocolVersion::V1 || supported == ProtocolVersion::V1 {
            Some((ProtocolVersion::V1, SyncMagic::v1()))
        } else {
            let nonce = self.nonce()?;
            Some((ProtocolVersion::V2, SyncMagic::v2(nonce)))
        }
    }
}

impl std::fmt::Display for SyncMagic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.nonce {
            Some(nonce) => write!(f, "{}:{nonce:x}", self.version),
            None => write!(f, "{}", self.version),
        }
    }
}

/// A placeholder CID indicating "any" CID is acceptable.
pub const CID_ANY: u32 = u32::MAX;

/// CID of the host.
pub const CID_HOST: u32 = 2;

/// The loopback CID.
pub const CID_LOOPBACK: u32 = 1;

/// An address for a vsock packet transmitted over USB. Since this library does not implement
/// policy decisions, it includes all four components of a vsock address pair even though some
/// may not be appropriate for some situations.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Address {
    /// For Connect, Reset, Accept, and Data packets this represents the device side's address.
    /// Usually this will be a special value representing either that it is simply "the device",
    /// or zero along with the rest of the cid and port fields to indicate that it's a control stream
    /// packet. Must be zero for any other packet type.
    pub device_cid: u32,
    /// For Connect, Reset, Accept, and Data packets this represents the host side's address.
    /// Usually this will be a special value representing either that it is simply "the host",
    /// or zero along with the rest of the cid and port fields to indicate that it's a control stream
    /// packet. Must be zero for any other packet type.
    pub host_cid: u32,
    /// For Connect, Reset, Accept, and Data packets this represents the device side's port.
    /// This must be a valid positive value for any of those packet types, unless all of the cid and
    /// port fields are also zero, in which case it is a control stream packet. Must be zero for any
    /// other packet type.
    pub device_port: u32,
    /// For Connect, Reset, Accept, and Data packets this represents the host side's port.
    /// This must be a valid positive value for any of those packet types, unless all of the cid and
    /// port fields are also zero, in which case it is a control stream packet. Must be zero for any
    /// other packet type.
    pub host_port: u32,
}

impl Address {
    /// Returns true if all the fields of this address are zero (which usually means it's a control
    /// packet of some sort).
    pub fn is_zeros(&self) -> bool {
        *self == Self::default()
    }
}

impl From<&Header> for Address {
    fn from(header: &Header) -> Self {
        Self {
            device_cid: header.device_cid.get(),
            host_cid: header.host_cid.get(),
            device_port: header.device_port.get(),
            host_port: header.host_port.get(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_version_magic_roundtrip() {
        let nz = |n| std::num::NonZeroU32::new(n).unwrap();

        let v0 = SyncMagic::v0();
        assert_eq!(v0.magic(), b"vsock:0");
        assert_eq!(SyncMagic::from_magic(b"vsock:0"), Some(v0));
        assert_eq!(v0.version(), ProtocolVersion::V0);
        assert_eq!(v0.nonce(), None);
        assert!(!v0.is_reset());

        let v1 = SyncMagic::v1();
        assert_eq!(v1.magic(), b"vsock:1");
        assert_eq!(SyncMagic::from_magic(b"vsock:1"), Some(v1));
        assert_eq!(v1.version(), ProtocolVersion::V1);
        assert_eq!(v1.nonce(), None);
        assert!(!v1.is_reset());

        let v2_zero = SyncMagic::v2_reset();
        assert_eq!(v2_zero.magic(), b"vsock:2:0");
        assert_eq!(SyncMagic::from_magic(b"vsock:2:0"), Some(v2_zero));
        assert_eq!(v2_zero.version(), ProtocolVersion::V2);
        assert_eq!(v2_zero.nonce(), None);
        assert!(v2_zero.is_reset());

        let v2 = SyncMagic::v2(nz(0xdeadbeef));
        assert_eq!(v2.magic(), b"vsock:2:deadbeef");
        assert_eq!(SyncMagic::from_magic(b"vsock:2:deadbeef"), Some(v2));
        assert_eq!(v2.nonce(), Some(nz(0xdeadbeef)));
        assert!(!v2.is_reset());

        // Also test parsing with upper case / padded hex
        assert_eq!(SyncMagic::from_magic(b"vsock:2:DEADBEEF"), Some(SyncMagic::v2(nz(0xdeadbeef))));
        assert_eq!(SyncMagic::from_magic(b"vsock:2:00000001"), Some(SyncMagic::v2(nz(1))));

        // Invalid or unknown magics
        assert_eq!(SyncMagic::from_magic(b""), None);
        assert_eq!(SyncMagic::from_magic(b"vsock"), None);
        assert_eq!(SyncMagic::from_magic(b"vsock:3"), None);
        assert_eq!(SyncMagic::from_magic(b"vsock:9999:1234abcd"), None);
        assert_eq!(SyncMagic::from_magic(b"vsock:0:1"), None);
        assert_eq!(SyncMagic::from_magic(b"vsock:1:1"), None);
        assert_eq!(SyncMagic::from_magic(b"vsock:2"), None);
        assert_eq!(SyncMagic::from_magic(b"vsock:2:"), None);
        assert_eq!(SyncMagic::from_magic(b"vsock:2:invalid"), None);
        assert_eq!(SyncMagic::from_magic(b"vsock:2:+1"), None);
    }

    #[test]
    fn test_protocol_version_negotiate() {
        let nz = |n| std::num::NonZeroU32::new(n).unwrap();
        let host_v2 = SyncMagic::v2(nz(0x12345678));
        let host_v2_reset = SyncMagic::v2_reset();
        let host_v1 = SyncMagic::v1();
        let host_v0 = SyncMagic::v0();

        assert_eq!(host_v2_reset.negotiate(ProtocolVersion::V2), None);
        assert_eq!(
            host_v2.negotiate(ProtocolVersion::V2),
            Some((ProtocolVersion::V2, SyncMagic::v2(nz(0x12345678))))
        );
        assert_eq!(host_v1.negotiate(ProtocolVersion::V2), Some((ProtocolVersion::V1, host_v1)));
        assert_eq!(host_v0.negotiate(ProtocolVersion::V2), Some((ProtocolVersion::V0, host_v0)));

        assert_eq!(host_v2.negotiate(ProtocolVersion::V1), Some((ProtocolVersion::V1, host_v1)));
        assert_eq!(host_v1.negotiate(ProtocolVersion::V1), Some((ProtocolVersion::V1, host_v1)));
        assert_eq!(host_v0.negotiate(ProtocolVersion::V1), Some((ProtocolVersion::V0, host_v0)));

        assert_eq!(host_v2.negotiate(ProtocolVersion::V0), Some((ProtocolVersion::V0, host_v0)));
        assert_eq!(host_v1.negotiate(ProtocolVersion::V0), Some((ProtocolVersion::V0, host_v0)));
        assert_eq!(host_v0.negotiate(ProtocolVersion::V0), Some((ProtocolVersion::V0, host_v0)));
    }

    #[test]
    fn test_protocol_version_features() {
        assert!(!ProtocolVersion::V0.has_pause_packets());
        assert!(ProtocolVersion::V1.has_pause_packets());
        assert!(ProtocolVersion::V2.has_pause_packets());
    }

    #[test]
    fn test_protocol_version_display() {
        let nz = |n| std::num::NonZeroU32::new(n).unwrap();
        assert_eq!(format!("{}", ProtocolVersion::V0), "0");
        assert_eq!(format!("{}", ProtocolVersion::V1), "1");
        assert_eq!(format!("{}", ProtocolVersion::V2), "2");
        assert_eq!(format!("{}", SyncMagic::v2(nz(0x1a2b))), "2:1a2b");
    }
}
