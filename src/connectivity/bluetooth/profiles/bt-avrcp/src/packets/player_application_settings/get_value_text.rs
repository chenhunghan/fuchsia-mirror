// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use packet_encoding::{Decodable, Encodable};

use crate::packets::player_application_settings::{
    PlayerApplicationSettingAttributeId, RepeatStatusMode, TEXT_ENTRY_HEADER_LEN,
    decode_text_entries, encode_text_entries,
};
use crate::packets::{
    AvcCommandType, Error, PacketResult, PduId, ShuffleMode, UNKNOWN_VALUE_PLACEHOLDER,
    VendorCommand, VendorDependentPdu,
};

#[derive(Debug)]
/// AVRCP 1.6.1 section 6.5.6 GetPlayerApplicationSettingValueText command.
pub struct GetPlayerApplicationSettingValueTextCommand {
    attribute_id: PlayerApplicationSettingAttributeId,
    num_values: u8,
    value_ids: Vec<u8>,
}

impl GetPlayerApplicationSettingValueTextCommand {
    pub fn new(
        attribute_id: PlayerApplicationSettingAttributeId,
        value_ids: Vec<u8>,
    ) -> GetPlayerApplicationSettingValueTextCommand {
        Self { attribute_id, num_values: value_ids.len() as u8, value_ids }
    }

    pub fn attribute_id(&self) -> PlayerApplicationSettingAttributeId {
        self.attribute_id
    }

    pub fn value_ids(&self) -> Vec<u8> {
        self.value_ids.clone()
    }
}

/// Packet PDU ID for vendor dependent packet encoding.
impl VendorDependentPdu for GetPlayerApplicationSettingValueTextCommand {
    fn pdu_id(&self) -> PduId {
        PduId::GetPlayerApplicationSettingValueText
    }
}

/// Specifies the AVC command type for this AVC command packet
impl VendorCommand for GetPlayerApplicationSettingValueTextCommand {
    fn command_type(&self) -> AvcCommandType {
        AvcCommandType::Status
    }
}

impl Decodable for GetPlayerApplicationSettingValueTextCommand {
    type Error = Error;

    fn decode(buf: &[u8]) -> PacketResult<Self> {
        if buf.len() < 2 {
            return Err(Error::InvalidMessage);
        }

        let attribute_id = PlayerApplicationSettingAttributeId::try_from(buf[0])?;
        let num_values = buf[1];

        // There must be at least 1 attribute value id provided.
        // See AVRCP Sec 6.5.6
        if num_values < 1 {
            return Err(Error::InvalidMessage);
        }

        let mut value_ids = vec![];
        let mut idx = 2;
        while idx < buf.len() {
            value_ids.push(buf[idx]);
            idx += 1;
        }

        if num_values as usize != value_ids.len() {
            return Err(Error::InvalidMessage);
        }
        Ok(Self { attribute_id, num_values, value_ids })
    }
}

impl Encodable for GetPlayerApplicationSettingValueTextCommand {
    type Error = Error;

    fn encoded_len(&self) -> usize {
        2 + self.num_values as usize
    }
    fn encode(&self, buf: &mut [u8]) -> PacketResult<()> {
        if buf.len() < self.encoded_len() {
            return Err(Error::InvalidMessageLength);
        }

        buf[0] = u8::from(&self.attribute_id);
        buf[1] = self.num_values;

        if self.num_values != self.value_ids.len() as u8 {
            return Err(Error::ParameterEncodingError);
        }

        for (i, v_id) in self.value_ids.iter().enumerate() {
            buf[i + 2] = u8::from(v_id.clone());
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValueInfo {
    value_id: u8,
    description: String,
}

impl ValueInfo {
    pub fn new(value_id: u8, description: impl Into<String>) -> Self {
        Self { value_id, description: description.into() }
    }

    /// Returns the encoded size of this entry in bytes.
    pub fn num_bytes(&self) -> usize {
        TEXT_ENTRY_HEADER_LEN + self.description.len()
    }
}

/// Attempts to convert attribute value IDs into a vector of ValueInfo objects.
pub fn repeat_status_mode_to_value_info(value_ids: Vec<u8>) -> Result<Vec<ValueInfo>, Error> {
    let mut infos = vec![];
    for value in value_ids {
        let value_id = RepeatStatusMode::try_from(value).map_err(|_| Error::OutOfRange)?;
        let desc = match value_id {
            RepeatStatusMode::Off => "Off",
            RepeatStatusMode::SingleTrackRepeat => "Single track repeat",
            RepeatStatusMode::AllTrackRepeat => "All track repeat",
            RepeatStatusMode::GroupRepeat => "Group repeat",
        };
        infos.push(ValueInfo::new(value, desc));
    }
    Ok(infos)
}

/// Attempts to convert attribute value IDs into a vector of ValueInfo objects.
pub fn shuffle_mode_to_value_info(value_ids: Vec<u8>) -> Result<Vec<ValueInfo>, Error> {
    let mut infos = vec![];
    for value in value_ids {
        let value_id = ShuffleMode::try_from(value).map_err(|_| Error::OutOfRange)?;
        let desc = match value_id {
            ShuffleMode::Off => "Off",
            ShuffleMode::AllTrackShuffle => "All tracks shuffle",
            ShuffleMode::GroupShuffle => "Group shuffle",
        };
        infos.push(ValueInfo::new(value, desc));
    }
    Ok(infos)
}

#[derive(Debug)]
/// AVRCP 1.6.1 section 6.5.6 GetPlayerApplicationSettingValueText response.
pub struct GetPlayerApplicationSettingValueTextResponse {
    value_infos: Vec<ValueInfo>,
}

impl GetPlayerApplicationSettingValueTextResponse {
    pub fn new(value_infos: Vec<ValueInfo>) -> GetPlayerApplicationSettingValueTextResponse {
        Self { value_infos }
    }
}

/// Packet PDU ID for vendor dependent packet encoding.
impl VendorDependentPdu for GetPlayerApplicationSettingValueTextResponse {
    fn pdu_id(&self) -> PduId {
        PduId::GetPlayerApplicationSettingValueText
    }
}

/// Specifies the AVC command type for this AVC command packet
impl VendorCommand for GetPlayerApplicationSettingValueTextResponse {
    fn command_type(&self) -> AvcCommandType {
        AvcCommandType::Status
    }
}

impl Decodable for GetPlayerApplicationSettingValueTextResponse {
    type Error = Error;

    fn decode(buf: &[u8]) -> PacketResult<Self> {
        let value_infos =
            decode_text_entries(buf, UNKNOWN_VALUE_PLACEHOLDER, |value_id, description| {
                Ok(ValueInfo::new(value_id, description))
            })?;
        Ok(Self { value_infos })
    }
}

impl Encodable for GetPlayerApplicationSettingValueTextResponse {
    type Error = Error;

    fn encoded_len(&self) -> usize {
        1 + self.value_infos.iter().map(ValueInfo::num_bytes).sum::<usize>()
    }

    fn encode(&self, buf: &mut [u8]) -> PacketResult<()> {
        encode_text_entries(
            self.encoded_len(),
            self.value_infos.iter().map(|info| (info.value_id, info.description.as_str())),
            buf,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packets::VendorDependentRawPdu;
    use crate::packets::player_application_settings::encode_error;

    fn decode_infos(buf: &[u8]) -> Vec<ValueInfo> {
        GetPlayerApplicationSettingValueTextResponse::decode(buf)
            .expect("should decode")
            .value_infos
    }

    #[test]
    // Test GetPlayerApplicationSettingValueTextCommand encoding success.
    fn test_get_player_application_setting_value_text_command_encode() {
        let command = GetPlayerApplicationSettingValueTextCommand::new(
            PlayerApplicationSettingAttributeId::Equalizer,
            vec![0x01, 0x02],
        );
        assert_eq!(command.raw_pdu_id(), u8::from(&PduId::GetPlayerApplicationSettingValueText));
        assert_eq!(command.encoded_len(), 4);
        let mut buf = vec![0; command.encoded_len()];
        assert!(command.encode(&mut buf[..]).is_ok());
        assert_eq!(buf, vec![0x01, 0x02, 0x01, 0x02]);
    }

    #[test]
    // Test GetPlayerApplicationSettingValueTextCommand encoding failure due
    // to too small buffer.
    fn test_get_player_application_setting_value_text_command_encode_error() {
        let command = GetPlayerApplicationSettingValueTextCommand::new(
            PlayerApplicationSettingAttributeId::Equalizer,
            vec![0x01, 0x02],
        );
        assert_eq!(command.raw_pdu_id(), u8::from(&PduId::GetPlayerApplicationSettingValueText));
        assert_eq!(command.encoded_len(), 4);
        // Smaller buffer size.
        let mut buf = vec![0; command.encoded_len() - 1];
        assert!(command.encode(&mut buf[..]).is_err());
    }

    #[test]
    // Test GetPlayerApplicationSettingValueTextCommand decoding success.
    fn test_get_player_application_setting_value_text_command_decode() {
        let command = [0x02, 0x03, 0x01, 0x02, 0x03];
        let result = GetPlayerApplicationSettingValueTextCommand::decode(&command);
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.attribute_id, PlayerApplicationSettingAttributeId::RepeatStatusMode);
        assert_eq!(result.num_values, 3);
        assert_eq!(result.value_ids, vec![0x01, 0x02, 0x03]);
    }

    #[test]
    // Test GetPlayerApplicationSettingValueTextCommand decoding failure due
    // to invalid `attribute_id` in buffer.
    fn test_get_player_application_setting_value_text_command_decode_error() {
        let command = [0x09, 0x03, 0x01, 0x02, 0x03];
        let result = GetPlayerApplicationSettingValueTextCommand::decode(&command);
        assert!(result.is_err());
    }

    #[test]
    // Test GetPlayerApplicationSettingValueTextCommand decoding failure due
    // to invalid buffer.
    fn test_get_player_application_setting_value_text_command_decode_buf_error() {
        let command = [0x03, 0x04, 0x01, 0x02, 0x03];
        let result = GetPlayerApplicationSettingValueTextCommand::decode(&command);
        assert!(result.is_err());
    }

    #[test]
    // Test GetPlayerApplicationSettingValueTextResponse encoding success.
    fn test_get_player_application_setting_value_text_response_encode() {
        let value_infos = vec![
            // 10 bytes.
            ValueInfo::new(0x01, "abcdef"),
            // 8 bytes.
            ValueInfo::new(0x04, "plop"),
            // 5 bytes.
            ValueInfo::new(0x05, "t"),
        ];
        let response = GetPlayerApplicationSettingValueTextResponse::new(value_infos);
        assert_eq!(response.raw_pdu_id(), u8::from(&PduId::GetPlayerApplicationSettingValueText));
        assert_eq!(response.encoded_len(), 24);
        let mut buf = vec![0; response.encoded_len()];
        assert!(response.encode(&mut buf[..]).is_ok());
        assert_eq!(
            buf,
            vec![
                0x03, 0x01, 0x00, 0x6A, 0x06, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x04, 0x00, 0x6A,
                0x04, 0x70, 0x6C, 0x6F, 0x70, 0x05, 0x00, 0x6A, 0x01, 0x74,
            ]
        );
    }

    #[fuchsia::test]
    // Test GetPlayerApplicationSettingValueTextResponse encoding failure cases.
    fn test_get_player_application_setting_value_text_response_encode_errors() {
        // Empty values list.
        let empty = GetPlayerApplicationSettingValueTextResponse::new(vec![]);
        assert_eq!(encode_error(&empty), Error::ParameterEncodingError);

        // More than u8::MAX values.
        let info = ValueInfo::new(0x01, "on");
        let too_many =
            GetPlayerApplicationSettingValueTextResponse::new(vec![info; usize::from(u8::MAX) + 1]);
        assert_eq!(encode_error(&too_many), Error::ParameterEncodingError);

        // Description exceeding u8::MAX bytes.
        let too_long = GetPlayerApplicationSettingValueTextResponse::new(vec![ValueInfo::new(
            0x01,
            "a".repeat(usize::from(u8::MAX) + 1),
        )]);
        assert_eq!(encode_error(&too_long), Error::ParameterEncodingError);

        // Buffer smaller than encoded length.
        let valid =
            GetPlayerApplicationSettingValueTextResponse::new(vec![ValueInfo::new(0x01, "on")]);
        let mut buf = vec![0; valid.encoded_len() - 1];
        assert_eq!(valid.encode(&mut buf[..]), Err(Error::InvalidMessageLength));
    }

    #[test]
    // Test GetPlayerApplicationSettingValueTextResponse decoding success for UTF-8, ASCII,
    // lossy UTF-8, and round-trip encoding.
    fn test_get_player_application_setting_value_text_response_decode() {
        // UTF-8 charset ID (0x006A).
        let command = [0x01, 0x10, 0x00, 0x6A, 0x05, 0x61, 0x62, 0x63, 0x64, 0x65];
        assert_eq!(decode_infos(&command), vec![ValueInfo::new(0x10, "abcde")]);

        // ASCII charset ID (0x0003) and an empty UTF-8 description.
        let command = [0x02, 0x01, 0x00, 0x03, 0x02, 0x6F, 0x6E, 0x02, 0x00, 0x6A, 0x00];
        assert_eq!(
            decode_infos(&command),
            vec![ValueInfo::new(0x01, "on"), ValueInfo::new(0x02, "")]
        );

        // Invalid UTF-8 bytes labelled as UTF-8 are replaced lossily instead of failing decode.
        let command = [0x01, 0x02, 0x00, 0x6A, 0x02, 0xFF, 0xFE];
        assert_eq!(decode_infos(&command), vec![ValueInfo::new(0x02, "\u{FFFD}\u{FFFD}")]);

        // Encode then decode round trip.
        let expected = vec![
            ValueInfo::new(0x01, "abcdef"),
            ValueInfo::new(0x04, "plop"),
            ValueInfo::new(0x05, "t"),
        ];
        let response = GetPlayerApplicationSettingValueTextResponse::new(expected.clone());
        let mut encoded = vec![0; response.encoded_len()];
        response.encode(&mut encoded).expect("should encode");
        assert_eq!(decode_infos(&encoded), expected);
    }

    #[fuchsia::test]
    // Test GetPlayerApplicationSettingValueTextResponse decoding gracefully handles
    // non-UTF-8 and unknown CharsetIds by substituting a placeholder description.
    fn test_get_player_application_setting_value_text_response_decode_non_utf8_charset() {
        #[rustfmt::skip]
        let command = [
            0x03,
            // Known non-UTF-8 charset (UTF-16 = 0x03F7).
            0x10, 0x03, 0xF7, 0x05, 0x61, 0x62, 0x63, 0x64, 0x65,
            // Unknown / out-of-range charset (0xFFFF) with non-UTF-8 bytes.
            0x02, 0xFF, 0xFF, 0x02, 0xFF, 0xFE,
            // Valid UTF-8 entry in the same response.
            0x03, 0x00, 0x6A, 0x03, 0x4F, 0x66, 0x66,
        ];
        assert_eq!(
            decode_infos(&command),
            vec![
                ValueInfo::new(0x10, UNKNOWN_VALUE_PLACEHOLDER),
                ValueInfo::new(0x02, UNKNOWN_VALUE_PLACEHOLDER),
                ValueInfo::new(0x03, "Off"),
            ]
        );
    }

    #[test]
    // Test GetPlayerApplicationSettingValueTextResponse decoding failure due
    // to invalid `num_values` or trailing bytes.
    fn test_get_player_application_setting_value_text_response_decode_attrs_error() {
        // Empty buffer and zero count.
        assert_eq!(
            GetPlayerApplicationSettingValueTextResponse::decode(&[]).unwrap_err(),
            Error::InvalidMessage
        );
        assert_eq!(
            GetPlayerApplicationSettingValueTextResponse::decode(&[0x00]).unwrap_err(),
            Error::InvalidMessage
        );

        // Count exceeds present entries.
        let command = [0x02, 0x02, 0x00, 0x04, 0x02, 0x62, 0x78];
        assert_eq!(
            GetPlayerApplicationSettingValueTextResponse::decode(&command).unwrap_err(),
            Error::InvalidMessageLength
        );

        // Trailing unconsumed byte.
        let trailing = [0x01, 0x02, 0x00, 0x6A, 0x02, 0x62, 0x78, 0x00];
        assert_eq!(
            GetPlayerApplicationSettingValueTextResponse::decode(&trailing).unwrap_err(),
            Error::InvalidMessage
        );
    }

    #[test]
    // Test GetPlayerApplicationSettingValueTextResponse decoding failure due
    // to buffer that is too small.
    fn test_get_player_application_setting_value_text_response_decode_buf_error() {
        let command = [0x01, 0x01, 0x00, 0x04, 0x05, 0x01];
        assert_eq!(
            GetPlayerApplicationSettingValueTextResponse::decode(&command).unwrap_err(),
            Error::InvalidMessageLength
        );
    }

    #[test]
    /// Test converting a byte buffer (in the context of RepeatStatusMode) to a
    /// ValueInfo object works as expected.
    fn test_repeat_status_mode_to_value_info() {
        let buf = vec![1, 4];
        let expected = vec![ValueInfo::new(1, "Off"), ValueInfo::new(4, "Group repeat")];
        let res = repeat_status_mode_to_value_info(buf);
        assert_eq!(Ok(expected), res);

        // Contains an invalid valueID among valid ones. Should error.
        let invalid_buf = vec![0, 2, 9];
        let res = repeat_status_mode_to_value_info(invalid_buf);
        assert_eq!(Err("OutOfRange".to_string()), res.map_err(|e| format!("{:?}", e)));
    }

    #[test]
    /// Test converting a byte buffer (in the context of ShuffleMode) to a
    /// ValueInfo object works as expected.
    fn test_shuffle_mode_to_value_info() {
        let buf = vec![1, 2];
        let expected = vec![ValueInfo::new(1, "Off"), ValueInfo::new(2, "All tracks shuffle")];
        let res = shuffle_mode_to_value_info(buf);
        assert_eq!(Ok(expected), res);

        // Contains an invalid valueID among valid ones. Should error.
        let invalid_buf = vec![4];
        let res = shuffle_mode_to_value_info(invalid_buf);
        assert_eq!(Err("OutOfRange".to_string()), res.map_err(|e| format!("{:?}", e)));
    }
}
