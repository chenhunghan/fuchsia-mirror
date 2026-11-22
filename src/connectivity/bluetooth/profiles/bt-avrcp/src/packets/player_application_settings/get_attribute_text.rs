// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use packet_encoding::{Decodable, Encodable};

use crate::packets::player_application_settings::{
    PlayerApplicationSettingAttributeId, TEXT_ENTRY_HEADER_LEN, decode_text_entries,
    encode_text_entries,
};
use crate::packets::{
    AvcCommandType, Error, PacketResult, PduId, VendorCommand, VendorDependentPdu,
};

#[derive(Debug)]
/// AVRCP 1.6.1 section 6.5.5 GetPlayerApplicationSettingAttributeText
pub struct GetPlayerApplicationSettingAttributeTextCommand {
    num_attributes: u8,
    pub attribute_ids: Vec<PlayerApplicationSettingAttributeId>,
}

impl GetPlayerApplicationSettingAttributeTextCommand {
    pub fn new(
        attribute_ids: Vec<PlayerApplicationSettingAttributeId>,
    ) -> GetPlayerApplicationSettingAttributeTextCommand {
        Self { num_attributes: attribute_ids.len() as u8, attribute_ids }
    }
}

/// Packet PDU ID for vendor dependent packet encoding.
impl VendorDependentPdu for GetPlayerApplicationSettingAttributeTextCommand {
    fn pdu_id(&self) -> PduId {
        PduId::GetPlayerApplicationSettingAttributeText
    }
}

/// Specifies the AVC command type for this AVC command packet
impl VendorCommand for GetPlayerApplicationSettingAttributeTextCommand {
    fn command_type(&self) -> AvcCommandType {
        AvcCommandType::Status
    }
}

impl Decodable for GetPlayerApplicationSettingAttributeTextCommand {
    type Error = Error;

    fn decode(buf: &[u8]) -> PacketResult<Self> {
        if buf.len() < 1 {
            return Err(Error::InvalidMessage);
        }

        let num_attributes = buf[0];
        // There must be at least 1 attribute ID provided.
        // See AVRCP Sec 6.5.3
        if num_attributes < 1 {
            return Err(Error::InvalidMessage);
        }

        let mut attribute_ids = vec![];
        let mut chunks = buf[1..].chunks(1);
        while let Some(chunk) = chunks.next() {
            attribute_ids.push(PlayerApplicationSettingAttributeId::try_from(chunk[0])?);
        }

        if attribute_ids.len() != num_attributes as usize {
            return Err(Error::InvalidMessage);
        }
        Ok(Self { num_attributes, attribute_ids })
    }
}

impl Encodable for GetPlayerApplicationSettingAttributeTextCommand {
    type Error = Error;

    fn encoded_len(&self) -> usize {
        1 + self.num_attributes as usize
    }
    fn encode(&self, buf: &mut [u8]) -> PacketResult<()> {
        if buf.len() < self.encoded_len() {
            return Err(Error::InvalidMessageLength);
        }
        buf[0] = u8::from(self.num_attributes);
        if self.num_attributes as usize != self.attribute_ids.len() {
            return Err(Error::ParameterEncodingError);
        }
        for (i, id) in self.attribute_ids.iter().enumerate() {
            buf[i + 1] = u8::from(id);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttributeInfo {
    attribute_id: PlayerApplicationSettingAttributeId,
    description: String,
}

impl AttributeInfo {
    /// Placeholder description used when the charset ID is not UTF-8 compatible.
    const UNKNOWN_ATTRIBUTE_PLACEHOLDER: &str = "Unknown Attribute";

    pub fn new(
        attribute_id: PlayerApplicationSettingAttributeId,
        description: impl Into<String>,
    ) -> Self {
        Self { attribute_id, description: description.into() }
    }

    /// Returns the encoded size of this entry in bytes.
    pub fn num_bytes(&self) -> usize {
        TEXT_ENTRY_HEADER_LEN + self.description.len()
    }
}

#[derive(Debug)]
/// AVRCP 1.6.1 section 6.5.5 GetPlayerApplicationSettingAttributeText
pub struct GetPlayerApplicationSettingAttributeTextResponse {
    attribute_infos: Vec<AttributeInfo>,
}

impl GetPlayerApplicationSettingAttributeTextResponse {
    pub fn new(
        attribute_infos: Vec<AttributeInfo>,
    ) -> GetPlayerApplicationSettingAttributeTextResponse {
        Self { attribute_infos }
    }
}

/// Packet PDU ID for vendor dependent packet encoding.
impl VendorDependentPdu for GetPlayerApplicationSettingAttributeTextResponse {
    fn pdu_id(&self) -> PduId {
        PduId::GetPlayerApplicationSettingAttributeText
    }
}

/// Specifies the AVC command type for this AVC command packet
impl VendorCommand for GetPlayerApplicationSettingAttributeTextResponse {
    fn command_type(&self) -> AvcCommandType {
        AvcCommandType::Status
    }
}

impl Decodable for GetPlayerApplicationSettingAttributeTextResponse {
    type Error = Error;

    fn decode(buf: &[u8]) -> PacketResult<Self> {
        let attribute_infos = decode_text_entries(
            buf,
            AttributeInfo::UNKNOWN_ATTRIBUTE_PLACEHOLDER,
            |raw_id, description| {
                Ok(AttributeInfo::new(
                    PlayerApplicationSettingAttributeId::try_from(raw_id)?,
                    description,
                ))
            },
        )?;
        Ok(Self { attribute_infos })
    }
}

impl Encodable for GetPlayerApplicationSettingAttributeTextResponse {
    type Error = Error;

    fn encoded_len(&self) -> usize {
        1 + self.attribute_infos.iter().map(AttributeInfo::num_bytes).sum::<usize>()
    }

    fn encode(&self, buf: &mut [u8]) -> PacketResult<()> {
        encode_text_entries(
            self.encoded_len(),
            self.attribute_infos
                .iter()
                .map(|info| (u8::from(&info.attribute_id), info.description.as_str())),
            buf,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packets::VendorDependentRawPdu;
    use crate::packets::player_application_settings::encode_error;

    fn decode_infos(buf: &[u8]) -> Vec<AttributeInfo> {
        GetPlayerApplicationSettingAttributeTextResponse::decode(buf)
            .expect("should decode")
            .attribute_infos
    }

    #[test]
    // Test GetPlayerApplicationSettingAttributeTextCommand encoding success.
    fn test_get_player_application_setting_attribute_text_command_encode() {
        let command = GetPlayerApplicationSettingAttributeTextCommand::new(vec![
            PlayerApplicationSettingAttributeId::Equalizer,
        ]);
        assert_eq!(
            command.raw_pdu_id(),
            u8::from(&PduId::GetPlayerApplicationSettingAttributeText)
        );
        assert_eq!(command.encoded_len(), 2); // 1 + 1 for the 1 id.
        let mut buf = vec![0; command.encoded_len()];
        assert!(command.encode(&mut buf[..]).is_ok());
        assert_eq!(buf, vec![0x01, 0x01]);
    }

    #[test]
    // Test GetPlayerApplicationSettingAttributeTextCommand encoding failure due
    // to too small buffer.
    fn test_get_player_application_setting_attribute_text_command_encode_error() {
        let command = GetPlayerApplicationSettingAttributeTextCommand::new(vec![
            PlayerApplicationSettingAttributeId::Equalizer,
        ]);
        assert_eq!(
            command.raw_pdu_id(),
            u8::from(&PduId::GetPlayerApplicationSettingAttributeText)
        );
        assert_eq!(command.encoded_len(), 2);
        // Smaller buffer size.
        let mut buf = vec![0; command.encoded_len() - 1];
        assert!(command.encode(&mut buf[..]).is_err());
    }

    #[test]
    // Test GetPlayerApplicationSettingAttributeTextCommand decoding success.
    fn test_get_player_application_setting_attribute_text_command_decode() {
        let command = [0x01, 0x01];
        let result = GetPlayerApplicationSettingAttributeTextCommand::decode(&command);
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.num_attributes, 1);
        assert_eq!(result.attribute_ids, vec![PlayerApplicationSettingAttributeId::Equalizer]);
    }

    #[test]
    // Test GetPlayerApplicationSettingAttributeTextCommand decoding failure due
    // to invalid `num_attributes` in buffer.
    fn test_get_player_application_setting_attribute_text_command_decode_error() {
        let command = [0x02, 0x01];
        let result = GetPlayerApplicationSettingAttributeTextCommand::decode(&command);
        assert!(result.is_err());
    }

    #[test]
    // Test GetPlayerApplicationSettingAttributeTextResponse encoding success.
    fn test_get_player_application_setting_attribute_text_response_encode() {
        let attr_infos = vec![
            // 10 bytes.
            AttributeInfo::new(PlayerApplicationSettingAttributeId::Equalizer, "Foobar"),
            // 8 bytes.
            AttributeInfo::new(PlayerApplicationSettingAttributeId::ScanMode, "test"),
        ];
        let response = GetPlayerApplicationSettingAttributeTextResponse::new(attr_infos);
        assert_eq!(
            response.raw_pdu_id(),
            u8::from(&PduId::GetPlayerApplicationSettingAttributeText)
        );
        assert_eq!(response.encoded_len(), 19);
        let mut buf = vec![0; response.encoded_len()];
        assert!(response.encode(&mut buf[..]).is_ok());
        assert_eq!(
            buf,
            vec![
                0x02, 0x01, 0x00, 0x6A, 0x06, 0x46, 0x6F, 0x6F, 0x62, 0x61, 0x72, 0x04, 0x00, 0x6A,
                0x04, 0x74, 0x65, 0x73, 0x74,
            ]
        );
    }

    #[fuchsia::test]
    // Test GetPlayerApplicationSettingAttributeTextResponse encoding failure cases.
    fn test_get_player_application_setting_attribute_text_response_encode_errors() {
        // Empty attributes list.
        let empty = GetPlayerApplicationSettingAttributeTextResponse::new(vec![]);
        assert_eq!(encode_error(&empty), Error::ParameterEncodingError);

        // More than u8::MAX attributes.
        let info = AttributeInfo::new(PlayerApplicationSettingAttributeId::Equalizer, "eq");
        let too_many =
            GetPlayerApplicationSettingAttributeTextResponse::new(vec![
                info;
                usize::from(u8::MAX) + 1
            ]);
        assert_eq!(encode_error(&too_many), Error::ParameterEncodingError);

        // Description exceeding u8::MAX bytes.
        let too_long =
            GetPlayerApplicationSettingAttributeTextResponse::new(vec![AttributeInfo::new(
                PlayerApplicationSettingAttributeId::Equalizer,
                "a".repeat(usize::from(u8::MAX) + 1),
            )]);
        assert_eq!(encode_error(&too_long), Error::ParameterEncodingError);

        // Buffer smaller than encoded length.
        let valid =
            GetPlayerApplicationSettingAttributeTextResponse::new(vec![AttributeInfo::new(
                PlayerApplicationSettingAttributeId::Equalizer,
                "eq",
            )]);
        let mut buf = vec![0; valid.encoded_len() - 1];
        assert_eq!(valid.encode(&mut buf[..]), Err(Error::InvalidMessageLength));
    }

    #[test]
    // Test GetPlayerApplicationSettingAttributeTextResponse decoding success for UTF-8, ASCII,
    // lossy UTF-8, and round-trip encoding.
    fn test_get_player_application_setting_attribute_text_response_decode() {
        // UTF-8 charset ID (0x006A).
        let command = [0x01, 0x02, 0x00, 0x6A, 0x02, 0x62, 0x78];
        assert_eq!(
            decode_infos(&command),
            vec![AttributeInfo::new(PlayerApplicationSettingAttributeId::RepeatStatusMode, "bx")]
        );

        // ASCII charset ID (0x0003) and an empty UTF-8 description.
        let command = [0x02, 0x01, 0x00, 0x03, 0x02, 0x62, 0x78, 0x03, 0x00, 0x6A, 0x00];
        assert_eq!(
            decode_infos(&command),
            vec![
                AttributeInfo::new(PlayerApplicationSettingAttributeId::Equalizer, "bx"),
                AttributeInfo::new(PlayerApplicationSettingAttributeId::ShuffleMode, ""),
            ]
        );

        // Invalid UTF-8 bytes labelled as UTF-8 are replaced lossily instead of failing decode.
        let command = [0x01, 0x02, 0x00, 0x6A, 0x02, 0xFF, 0xFE];
        assert_eq!(
            decode_infos(&command),
            vec![AttributeInfo::new(
                PlayerApplicationSettingAttributeId::RepeatStatusMode,
                "\u{FFFD}\u{FFFD}",
            )]
        );

        // Encode then decode round trip.
        let expected = vec![
            AttributeInfo::new(PlayerApplicationSettingAttributeId::Equalizer, "Foobar"),
            AttributeInfo::new(PlayerApplicationSettingAttributeId::ScanMode, "test"),
        ];
        let response = GetPlayerApplicationSettingAttributeTextResponse::new(expected.clone());
        let mut encoded = vec![0; response.encoded_len()];
        response.encode(&mut encoded).expect("should encode");
        assert_eq!(decode_infos(&encoded), expected);
    }

    #[fuchsia::test]
    // Test GetPlayerApplicationSettingAttributeTextResponse decoding gracefully handles
    // non-UTF-8 and unknown CharsetIds by substituting a placeholder description.
    fn test_get_player_application_setting_attribute_text_response_decode_non_utf8_charset() {
        #[rustfmt::skip]
        let command = [
            0x03,
            // Known non-UTF-8 charset (ISO-8859-1 = 0x0004).
            0x02, 0x00, 0x04, 0x02, 0x62, 0x78,
            // Unknown / out-of-range charset (0xFFFF) with non-UTF-8 bytes.
            0x01, 0xFF, 0xFF, 0x02, 0xFF, 0xFE,
            // Valid UTF-8 entry in the same response.
            0x04, 0x00, 0x6A, 0x04, 0x74, 0x65, 0x73, 0x74,
        ];
        assert_eq!(
            decode_infos(&command),
            vec![
                AttributeInfo::new(
                    PlayerApplicationSettingAttributeId::RepeatStatusMode,
                    AttributeInfo::UNKNOWN_ATTRIBUTE_PLACEHOLDER,
                ),
                AttributeInfo::new(
                    PlayerApplicationSettingAttributeId::Equalizer,
                    AttributeInfo::UNKNOWN_ATTRIBUTE_PLACEHOLDER,
                ),
                AttributeInfo::new(PlayerApplicationSettingAttributeId::ScanMode, "test"),
            ]
        );
    }

    #[test]
    // Test GetPlayerApplicationSettingAttributeTextResponse decoding failure due
    // to invalid `num_attributes` or trailing bytes.
    fn test_get_player_application_setting_attribute_text_response_decode_attrs_error() {
        // Empty buffer and zero count.
        assert_eq!(
            GetPlayerApplicationSettingAttributeTextResponse::decode(&[]).unwrap_err(),
            Error::InvalidMessage
        );
        assert_eq!(
            GetPlayerApplicationSettingAttributeTextResponse::decode(&[0x00]).unwrap_err(),
            Error::InvalidMessage
        );

        // Count exceeds present entries.
        let command = [0x02, 0x02, 0x00, 0x04, 0x02, 0x62, 0x78];
        assert_eq!(
            GetPlayerApplicationSettingAttributeTextResponse::decode(&command).unwrap_err(),
            Error::InvalidMessageLength
        );

        // Trailing unconsumed byte.
        let trailing = [0x01, 0x02, 0x00, 0x6A, 0x02, 0x62, 0x78, 0x00];
        assert_eq!(
            GetPlayerApplicationSettingAttributeTextResponse::decode(&trailing).unwrap_err(),
            Error::InvalidMessage
        );
    }

    #[test]
    // Test GetPlayerApplicationSettingAttributeTextResponse decoding failure due
    // to invalid `attribute_id`.
    fn test_get_player_application_setting_attribute_text_response_decode_attr_id_error() {
        let command = [0x01, 0xFF, 0x00, 0x04, 0x02, 0x62, 0x78];
        assert_eq!(
            GetPlayerApplicationSettingAttributeTextResponse::decode(&command).unwrap_err(),
            Error::InvalidParameter
        );
    }

    #[test]
    // Test GetPlayerApplicationSettingAttributeTextResponse decoding failure due
    // to buffer that is too small.
    fn test_get_player_application_setting_attribute_text_response_decode_buf_error() {
        let command = [0x01, 0x01, 0x00, 0x04, 0x05, 0x02, 0x04];
        assert_eq!(
            GetPlayerApplicationSettingAttributeTextResponse::decode(&command).unwrap_err(),
            Error::InvalidMessageLength
        );
    }
}
