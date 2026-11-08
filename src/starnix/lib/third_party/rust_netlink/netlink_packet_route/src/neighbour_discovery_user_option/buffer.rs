// SPDX-License-Identifier: MIT

use netlink_packet_utils::DecodeError;
use netlink_packet_utils::nla::{HasNlas, NlaBuffer, NlaError, NlasIterator};

pub const NEIGHBOUR_DISCOVERY_USER_OPTION_HEADER_LEN: usize = 16;

buffer!(NeighbourDiscoveryUserOptionMessageBuffer() {
    address_family: (u8, 0),
    padding_1: (u8, 1),
    options_length: (u16, 2..4),
    interface_index: (u32, 4..8),
    icmp_type: (u8, 8),
    icmp_code: (u8, 9),
    padding_2: (u16, 10..12),
    padding_3: (u32, 12..16),
    payload: (slice, NEIGHBOUR_DISCOVERY_USER_OPTION_HEADER_LEN..),
});

impl<'a, T: AsRef<[u8]> + ?Sized> HasNlas for NeighbourDiscoveryUserOptionMessageBuffer<&'a T> {
    fn attributes(&self) -> impl Iterator<Item = Result<NlaBuffer<&[u8]>, NlaError>> {
        NlasIterator::new(&self.payload()[self.options_length() as usize..])
    }
}

impl<T: AsRef<[u8]>> NeighbourDiscoveryUserOptionMessageBuffer<T> {
    pub fn new(buffer: T) -> Result<Self, DecodeError> {
        let packet = Self::new_unchecked(buffer);
        packet.check_buffer_length()?;
        Ok(packet)
    }

    fn check_buffer_length(&self) -> Result<(), DecodeError> {
        let len = self.buffer.as_ref().len();
        if len < NEIGHBOUR_DISCOVERY_USER_OPTION_HEADER_LEN {
            return Err(DecodeError::InvalidBufferLength {
                name: "NeighbourDiscoveryUserOptionMessageBuffer",
                len,
                buffer_len: NEIGHBOUR_DISCOVERY_USER_OPTION_HEADER_LEN,
            });
        }
        let required_len =
            NEIGHBOUR_DISCOVERY_USER_OPTION_HEADER_LEN + usize::from(self.options_length());
        if len < required_len {
            Err(DecodeError::InvalidBufferLength {
                name: "NeighbourDiscoveryUserOptionMessageBuffer",
                len,
                buffer_len: required_len,
            })
        } else {
            Ok(())
        }
    }
}

impl<'a, T: AsRef<[u8]> + ?Sized> NeighbourDiscoveryUserOptionMessageBuffer<&'a T> {
    pub fn option_body(&self) -> &[u8] {
        &self.payload()[..self.options_length() as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_short_header_and_truncated_options() {
        let short = [0u8; NEIGHBOUR_DISCOVERY_USER_OPTION_HEADER_LEN - 1];
        for len in [0, 2, 4, short.len()] {
            assert!(NeighbourDiscoveryUserOptionMessageBuffer::new(&short[..len]).is_err());
        }

        let mut truncated = [0u8; NEIGHBOUR_DISCOVERY_USER_OPTION_HEADER_LEN];
        truncated[2..4].copy_from_slice(&32u16.to_ne_bytes());
        assert!(NeighbourDiscoveryUserOptionMessageBuffer::new(&truncated[..]).is_err());

        let mut valid = [0u8; NEIGHBOUR_DISCOVERY_USER_OPTION_HEADER_LEN + 8];
        valid[2..4].copy_from_slice(&8u16.to_ne_bytes());
        let parsed = NeighbourDiscoveryUserOptionMessageBuffer::new(&valid[..]).unwrap();
        assert_eq!(parsed.option_body().len(), 8);
    }
}
