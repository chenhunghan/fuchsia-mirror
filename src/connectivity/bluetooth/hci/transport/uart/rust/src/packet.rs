// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_hardware_bluetooth::{
    ACL_PACKET_MAX, COMMAND_MAX, EVENT_MAX, ISO_PACKET_MAX, SCO_PACKET_MAX,
};
use log::{error, warn};
use sapphire_emboss::TryFromRaw;
use sapphire_emboss::hci_common::{CommandHeader, EventHeader, command_header, event_header};
use sapphire_emboss::hci_data::{
    AclDataFrameHeader, IsoDataFrameHeader, ScoDataHeader, acl_data_frame_header,
    iso_data_frame_header, sco_data_header,
};
pub use sapphire_emboss::hci_h4::H4PacketType as PacketType;

/// Extension methods for [`PacketType`] (H4 UART packet indicator).
///
/// Note that [`header_size`](PacketTypeExt::header_size), [`max_size`](PacketTypeExt::max_size),
/// and [`expected_size`](PacketTypeExt::expected_size) all exclude the 1-byte H4 UART packet
/// indicator prefix.
pub trait PacketTypeExt: Sized {
    /// Converts a raw H4 UART packet indicator byte into a [`PacketType`].
    ///
    /// Returns `Err(value)` if the byte does not correspond to a valid wire packet type.
    fn from_indicator(value: u8) -> Result<Self, u8>;

    /// Returns the HCI header size in bytes (excluding the 1-byte H4 indicator).
    fn header_size(self) -> usize;

    /// Returns the maximum valid HCI packet size in bytes (excluding the 1-byte H4 indicator).
    fn max_size(self) -> usize;

    /// Computes the expected total HCI packet length (excluding the 1-byte H4 indicator) from a
    /// buffer containing at least [`Self::header_size`] bytes.
    fn expected_size(self, header: &[u8]) -> Option<usize>;
}

impl PacketTypeExt for PacketType {
    fn from_indicator(value: u8) -> Result<Self, u8> {
        match PacketType::try_from_raw(value) {
            Ok(PacketType::UNKNOWN) | Err(_) => Err(value),
            Ok(packet_type) => Ok(packet_type),
        }
    }

    fn header_size(self) -> usize {
        match self {
            PacketType::COMMAND => command_header::SIZE_IN_BYTES,
            PacketType::ACL_DATA => acl_data_frame_header::SIZE_IN_BYTES,
            PacketType::SYNC_DATA => sco_data_header::SIZE_IN_BYTES,
            PacketType::EVENT => event_header::SIZE_IN_BYTES,
            PacketType::ISO_DATA => iso_data_frame_header::SIZE_IN_BYTES,
            PacketType::UNKNOWN => 0,
        }
    }

    fn max_size(self) -> usize {
        let max_packet_size = match self {
            PacketType::COMMAND => COMMAND_MAX,
            PacketType::ACL_DATA => ACL_PACKET_MAX,
            PacketType::SYNC_DATA => SCO_PACKET_MAX,
            PacketType::EVENT => EVENT_MAX,
            PacketType::ISO_DATA => ISO_PACKET_MAX,
            PacketType::UNKNOWN => return 0,
        };
        usize::try_from(max_packet_size).unwrap_or(usize::MAX)
    }

    fn expected_size(self, header: &[u8]) -> Option<usize> {
        let header_size = self.header_size();
        if header_size == 0 || header.len() < header_size {
            return None;
        }

        let payload_length = match self {
            PacketType::COMMAND => {
                usize::from(CommandHeader::new(header).parameter_total_size().try_read().ok()?)
            }
            PacketType::ACL_DATA => {
                usize::from(AclDataFrameHeader::new(header).data_total_length().try_read().ok()?)
            }
            PacketType::SYNC_DATA => {
                usize::try_from(ScoDataHeader::new(header).data_total_length().try_read().ok()?)
                    .ok()?
            }
            PacketType::EVENT => {
                usize::from(EventHeader::new(header).parameter_total_size().try_read().ok()?)
            }
            PacketType::ISO_DATA => usize::try_from(
                IsoDataFrameHeader::new(header).data_total_length().try_read().ok()?,
            )
            .ok()?,
            PacketType::UNKNOWN => return None,
        };

        Some(header_size + payload_length)
    }
}

/// Initial capacity for [`StreamingParser`]'s packet reassembly buffer.
///
/// Sized to fit all HCI event packets and most common ACL packets without reallocating.
const DEFAULT_BUFFER_CAPACITY: usize = 256;

/// Incrementally reassembles H4-framed HCI packets from arbitrary byte chunks read from UART.
#[derive(Debug)]
pub struct StreamingParser {
    buffer: Vec<u8>,
    current_type: Option<PacketType>,
}

impl Default for StreamingParser {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingParser {
    pub fn new() -> Self {
        Self { buffer: Vec::with_capacity(DEFAULT_BUFFER_CAPACITY), current_type: None }
    }

    /// Processes incoming UART bytes and returns all complete `(PacketType, Vec<u8>)` packets.
    ///
    /// The returned `Vec<u8>` for each packet contains the HCI header and payload *without* the
    /// leading 1-byte H4 UART indicator.
    pub fn parse(&mut self, data: &[u8]) -> Vec<(PacketType, Vec<u8>)> {
        let mut packets = Vec::new();
        let mut index = 0;

        while index < data.len() {
            let Some(packet_type) = self.current_type else {
                match PacketType::from_indicator(data[index]) {
                    Ok(packet_type) => {
                        self.current_type = Some(packet_type);
                        index += 1;
                    }
                    Err(indicator_byte) => {
                        warn!("Invalid packet indicator {indicator_byte:#04x}, dropping to resync");
                        index += 1;
                    }
                }
                continue;
            };

            index += self.fill_up_to(packet_type.header_size(), data, index);

            if self.buffer.len() >= packet_type.header_size() {
                let Some(expected_total_size) = packet_type.expected_size(&self.buffer) else {
                    error!("Failed to read header length for type {packet_type:?}, resyncing");
                    self.resync();
                    continue;
                };

                if expected_total_size > packet_type.max_size() {
                    error!(
                        "Packet size {expected_total_size} exceeds {packet_type:?} max, resyncing"
                    );
                    self.resync();
                    continue;
                }

                self.buffer.reserve_exact(expected_total_size.saturating_sub(self.buffer.len()));
                index += self.fill_up_to(expected_total_size, data, index);

                if self.buffer.len() == expected_total_size {
                    let complete_packet = std::mem::replace(
                        &mut self.buffer,
                        Vec::with_capacity(DEFAULT_BUFFER_CAPACITY),
                    );
                    packets.push((packet_type, complete_packet));
                    self.resync();
                }
            }
        }

        packets
    }

    /// Appends bytes from `data[index..]` into `self.buffer` until `self.buffer.len()` reaches
    /// `target_len` or `data` is exhausted, returning the number of bytes appended.
    fn fill_up_to(&mut self, target_len: usize, data: &[u8], index: usize) -> usize {
        let remaining = target_len.saturating_sub(self.buffer.len());
        let to_take = std::cmp::min(remaining, data.len() - index);
        self.buffer.extend(&data[index..index + to_take]);
        to_take
    }

    fn resync(&mut self) {
        self.buffer.clear();
        self.current_type = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_valid_command_packet() {
        let mut parser = StreamingParser::default();
        let data = [u8::from(PacketType::COMMAND), 1, 2, 2, 3, 4];
        let packets = parser.parse(&data);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].0, PacketType::COMMAND);
        assert_eq!(packets[0].1, vec![1, 2, 2, 3, 4]);
    }

    #[fuchsia::test]
    fn test_valid_acl_packet() {
        let mut parser = StreamingParser::new();
        // Indicator (2), handle/flags (0x01, 0x00), 16-bit LE length (3 -> 0x03, 0x00),
        // payload (0xaa, 0xbb, 0xcc)
        let data = [u8::from(PacketType::ACL_DATA), 0x01, 0x00, 0x03, 0x00, 0xaa, 0xbb, 0xcc];
        let packets = parser.parse(&data);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].0, PacketType::ACL_DATA);
        assert_eq!(packets[0].1, vec![0x01, 0x00, 0x03, 0x00, 0xaa, 0xbb, 0xcc]);
    }

    #[fuchsia::test]
    fn test_valid_sync_packet() {
        let mut parser = StreamingParser::new();
        // Indicator (3), handle/flags (0x07, 0x08), 1-byte length (2), payload (0x11, 0x22)
        let data = [u8::from(PacketType::SYNC_DATA), 0x07, 0x08, 0x02, 0x11, 0x22];
        let packets = parser.parse(&data);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].0, PacketType::SYNC_DATA);
        assert_eq!(packets[0].1, vec![0x07, 0x08, 0x02, 0x11, 0x22]);
    }

    #[fuchsia::test]
    fn test_valid_event_packet() {
        let mut parser = StreamingParser::new();
        // Indicator (4), event code (0x0e), length (2), payload (0x01, 0x00)
        let data = [u8::from(PacketType::EVENT), 0x0e, 0x02, 0x01, 0x00];
        let packets = parser.parse(&data);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].0, PacketType::EVENT);
        assert_eq!(packets[0].1, vec![0x0e, 0x02, 0x01, 0x00]);
    }

    #[fuchsia::test]
    fn test_valid_iso_packet_with_flags() {
        let mut parser = StreamingParser::new();
        // Indicator (5), handle/flags (0x09, 0x0a), length with upper 2 RFU/flag bits set
        // (0x03, 0xc0 -> length 3), payload (0x0b, 0x0c, 0x0d)
        let data = [u8::from(PacketType::ISO_DATA), 0x09, 0x0a, 0x03, 0xc0, 0x0b, 0x0c, 0x0d];
        let packets = parser.parse(&data);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].0, PacketType::ISO_DATA);
        assert_eq!(packets[0].1, vec![0x09, 0x0a, 0x03, 0xc0, 0x0b, 0x0c, 0x0d]);
    }

    #[fuchsia::test]
    fn test_fragmented_packet() {
        let mut parser = StreamingParser::new();
        let chunk1 = [u8::from(PacketType::EVENT), 0x0e];
        let chunk2 = [1, 0x05];

        let packets1 = parser.parse(&chunk1);
        assert_eq!(packets1.len(), 0);

        let packets2 = parser.parse(&chunk2);
        assert_eq!(packets2.len(), 1);
        assert_eq!(packets2[0].0, PacketType::EVENT);
        assert_eq!(packets2[0].1, vec![0x0e, 1, 0x05]);

        // Test splitting ACL packet across length bytes and payload bytes.
        let acl_bytes = [u8::from(PacketType::ACL_DATA), 0x00, 0x00, 0x02, 0x00, 0x01, 0x02];
        assert!(parser.parse(&acl_bytes[..4]).is_empty());
        assert!(parser.parse(&acl_bytes[4..6]).is_empty());
        let acl_packets = parser.parse(&acl_bytes[6..]);
        assert_eq!(acl_packets.len(), 1);
        assert_eq!(
            acl_packets[0],
            (PacketType::ACL_DATA, vec![0x00, 0x00, 0x02, 0x00, 0x01, 0x02])
        );
    }

    #[fuchsia::test]
    fn test_multiple_packets_including_zero_length_payloads() {
        let mut parser = StreamingParser::new();
        let data = [
            u8::from(PacketType::COMMAND),
            0x03,
            0x0c,
            0x00,
            u8::from(PacketType::ACL_DATA),
            0x01,
            0x00,
            0x00,
            0x00,
            u8::from(PacketType::SYNC_DATA),
            0x02,
            0x00,
            0x00,
            u8::from(PacketType::EVENT),
            0x0e,
            0x01,
            0x99,
            u8::from(PacketType::ISO_DATA),
            0x03,
            0x00,
            0x00,
            0x00,
        ];
        let packets = parser.parse(&data);
        assert_eq!(packets.len(), 5);
        assert_eq!(packets[0], (PacketType::COMMAND, vec![0x03, 0x0c, 0x00]));
        assert_eq!(packets[1], (PacketType::ACL_DATA, vec![0x01, 0x00, 0x00, 0x00]));
        assert_eq!(packets[2], (PacketType::SYNC_DATA, vec![0x02, 0x00, 0x00]));
        assert_eq!(packets[3], (PacketType::EVENT, vec![0x0e, 0x01, 0x99]));
        assert_eq!(packets[4], (PacketType::ISO_DATA, vec![0x03, 0x00, 0x00, 0x00]));
    }

    #[fuchsia::test]
    fn test_invalid_indicator_resync() {
        let mut parser = StreamingParser::new();
        let data = [
            0x00,
            0xff,
            0x06,
            u8::from(PacketType::EVENT),
            0x0e,
            0x01,
            0x42,
            0x99,
            u8::from(PacketType::COMMAND),
            0x03,
            0x0c,
            0x00,
        ];
        let packets = parser.parse(&data);
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0], (PacketType::EVENT, vec![0x0e, 0x01, 0x42]));
        assert_eq!(packets[1], (PacketType::COMMAND, vec![0x03, 0x0c, 0x00]));
    }
}
