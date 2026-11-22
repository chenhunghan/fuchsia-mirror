// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_bluetooth_avrcp as fidl_avrcp;
use packet_encoding::decodable_enum;

pub mod get_attribute_text;
pub mod get_current_settings;
pub mod get_value_text;
pub mod list_settings;
pub mod set_current_settings;

pub use self::get_attribute_text::*;
pub use self::get_current_settings::*;
pub use self::get_value_text::*;
pub use self::list_settings::*;
pub use self::set_current_settings::*;
use crate::packets::{CharsetId, Error, PacketResult};

/// Size in bytes of the fixed header in a player application setting text entry:
/// 1 byte for the attribute or value ID, 2 bytes for `character_set_id`, and
/// 1 byte for the string length.
pub(super) const TEXT_ENTRY_HEADER_LEN: usize = 4;

/// Decodes a count-prefixed list of player application setting text entries from `buf`.
pub(super) fn decode_text_entries<T>(
    buf: &[u8],
    placeholder: &str,
    mut build_entry: impl FnMut(u8, String) -> PacketResult<T>,
) -> PacketResult<Vec<T>> {
    let Some((&num_entries, rest)) = buf.split_first() else {
        return Err(Error::InvalidMessage);
    };
    let num_entries = usize::from(num_entries);
    // There must be at least 1 entry provided (AVRCP 1.6.2 Sections 6.5.5 and 6.5.6).
    if num_entries < 1 {
        return Err(Error::InvalidMessage);
    }

    let mut entries = Vec::with_capacity(num_entries);
    let mut idx = 0;
    for _ in 0..num_entries {
        let Some(&[id, charset_hi, charset_lo, len]) = rest.get(idx..idx + TEXT_ENTRY_HEADER_LEN)
        else {
            return Err(Error::InvalidMessageLength);
        };
        let charset_id = CharsetId::from_be_bytes([charset_hi, charset_lo]);
        let str_start = idx + TEXT_ENTRY_HEADER_LEN;
        let str_end = str_start + usize::from(len);
        let bytes = rest.get(str_start..str_end).ok_or(Error::InvalidMessageLength)?;
        let description = charset_id.decode_str(bytes, placeholder);
        entries.push(build_entry(id, description)?);
        idx = str_end;
    }

    if idx != rest.len() {
        return Err(Error::InvalidMessage);
    }

    Ok(entries)
}

/// Encodes a count-prefixed list of `(id, description)` player application setting text entries
/// into `buf`.
pub(super) fn encode_text_entries<'a>(
    encoded_len: usize,
    entries: impl ExactSizeIterator<Item = (u8, &'a str)>,
    buf: &mut [u8],
) -> PacketResult<()> {
    if buf.len() < encoded_len {
        return Err(Error::InvalidMessageLength);
    }
    // There must be at least 1 entry provided (AVRCP 1.6.2 Sections 6.5.5 and 6.5.6).
    if entries.len() == 0 {
        return Err(Error::ParameterEncodingError);
    }
    buf[0] = u8::try_from(entries.len()).map_err(|_| Error::ParameterEncodingError)?;

    let mut idx = 1;
    for (id, description) in entries {
        let desc_len =
            u8::try_from(description.len()).map_err(|_| Error::ParameterEncodingError)?;
        let str_start = idx + TEXT_ENTRY_HEADER_LEN;
        let entry_end = str_start + description.len();
        buf[idx] = id;
        buf[idx + 1..idx + 3].copy_from_slice(&CharsetId::Utf8.to_be_bytes());
        buf[idx + 3] = desc_len;
        buf[str_start..entry_end].copy_from_slice(description.as_bytes());
        idx = entry_end;
    }
    Ok(())
}

#[cfg(test)]
#[track_caller]
pub(super) fn encode_error(packet: &impl packet_encoding::Encodable<Error = Error>) -> Error {
    let mut buf = vec![0; packet.encoded_len()];
    packet.encode(&mut buf).expect_err("encoding should fail")
}

decodable_enum! {
    pub enum PlayerApplicationSettingAttributeId <u8, Error, InvalidParameter> {
        Equalizer = 0x01,
        RepeatStatusMode = 0x02,
        ShuffleMode = 0x03,
        ScanMode = 0x04,
    }
}

impl From<fidl_avrcp::PlayerApplicationSettingAttributeId> for PlayerApplicationSettingAttributeId {
    fn from(
        src: fidl_avrcp::PlayerApplicationSettingAttributeId,
    ) -> PlayerApplicationSettingAttributeId {
        match src {
            fidl_avrcp::PlayerApplicationSettingAttributeId::Equalizer => {
                PlayerApplicationSettingAttributeId::Equalizer
            }
            fidl_avrcp::PlayerApplicationSettingAttributeId::RepeatStatusMode => {
                PlayerApplicationSettingAttributeId::RepeatStatusMode
            }
            fidl_avrcp::PlayerApplicationSettingAttributeId::ShuffleMode => {
                PlayerApplicationSettingAttributeId::ShuffleMode
            }
            fidl_avrcp::PlayerApplicationSettingAttributeId::ScanMode => {
                PlayerApplicationSettingAttributeId::ScanMode
            }
        }
    }
}

impl From<PlayerApplicationSettingAttributeId> for fidl_avrcp::PlayerApplicationSettingAttributeId {
    fn from(
        src: PlayerApplicationSettingAttributeId,
    ) -> fidl_avrcp::PlayerApplicationSettingAttributeId {
        match src {
            PlayerApplicationSettingAttributeId::Equalizer => {
                fidl_avrcp::PlayerApplicationSettingAttributeId::Equalizer
            }
            PlayerApplicationSettingAttributeId::RepeatStatusMode => {
                fidl_avrcp::PlayerApplicationSettingAttributeId::RepeatStatusMode
            }
            PlayerApplicationSettingAttributeId::ShuffleMode => {
                fidl_avrcp::PlayerApplicationSettingAttributeId::ShuffleMode
            }
            PlayerApplicationSettingAttributeId::ScanMode => {
                fidl_avrcp::PlayerApplicationSettingAttributeId::ScanMode
            }
        }
    }
}

decodable_enum! {
    pub enum RepeatStatusMode <u8, Error, OutOfRange> {
        Off = 0x01,
        SingleTrackRepeat = 0x02,
        AllTrackRepeat = 0x03,
        GroupRepeat = 0x04,
    }
}

impl From<fidl_avrcp::RepeatStatusMode> for RepeatStatusMode {
    fn from(src: fidl_avrcp::RepeatStatusMode) -> RepeatStatusMode {
        match src {
            fidl_avrcp::RepeatStatusMode::Off => RepeatStatusMode::Off,
            fidl_avrcp::RepeatStatusMode::SingleTrackRepeat => RepeatStatusMode::SingleTrackRepeat,
            fidl_avrcp::RepeatStatusMode::AllTrackRepeat => RepeatStatusMode::AllTrackRepeat,
            fidl_avrcp::RepeatStatusMode::GroupRepeat => RepeatStatusMode::GroupRepeat,
        }
    }
}

impl From<RepeatStatusMode> for fidl_avrcp::RepeatStatusMode {
    fn from(src: RepeatStatusMode) -> fidl_avrcp::RepeatStatusMode {
        match src {
            RepeatStatusMode::Off => fidl_avrcp::RepeatStatusMode::Off,
            RepeatStatusMode::SingleTrackRepeat => fidl_avrcp::RepeatStatusMode::SingleTrackRepeat,
            RepeatStatusMode::AllTrackRepeat => fidl_avrcp::RepeatStatusMode::AllTrackRepeat,
            RepeatStatusMode::GroupRepeat => fidl_avrcp::RepeatStatusMode::GroupRepeat,
        }
    }
}

decodable_enum! {
    pub enum ShuffleMode <u8, Error, OutOfRange> {
        Off = 0x01,
        AllTrackShuffle = 0x02,
        GroupShuffle = 0x03,
    }
}

impl From<fidl_avrcp::ShuffleMode> for ShuffleMode {
    fn from(src: fidl_avrcp::ShuffleMode) -> ShuffleMode {
        match src {
            fidl_avrcp::ShuffleMode::Off => ShuffleMode::Off,
            fidl_avrcp::ShuffleMode::AllTrackShuffle => ShuffleMode::AllTrackShuffle,
            fidl_avrcp::ShuffleMode::GroupShuffle => ShuffleMode::GroupShuffle,
        }
    }
}

impl From<ShuffleMode> for fidl_avrcp::ShuffleMode {
    fn from(src: ShuffleMode) -> fidl_avrcp::ShuffleMode {
        match src {
            ShuffleMode::Off => fidl_avrcp::ShuffleMode::Off,
            ShuffleMode::AllTrackShuffle => fidl_avrcp::ShuffleMode::AllTrackShuffle,
            ShuffleMode::GroupShuffle => fidl_avrcp::ShuffleMode::GroupShuffle,
        }
    }
}

decodable_enum! {
    pub enum ScanMode <u8, Error, OutOfRange> {
        Off = 0x01,
        AllTrackScan = 0x02,
        GroupScan = 0x03,
    }
}

impl From<fidl_avrcp::ScanMode> for ScanMode {
    fn from(src: fidl_avrcp::ScanMode) -> ScanMode {
        match src {
            fidl_avrcp::ScanMode::Off => ScanMode::Off,
            fidl_avrcp::ScanMode::AllTrackScan => ScanMode::AllTrackScan,
            fidl_avrcp::ScanMode::GroupScan => ScanMode::GroupScan,
        }
    }
}

impl From<ScanMode> for fidl_avrcp::ScanMode {
    fn from(src: ScanMode) -> fidl_avrcp::ScanMode {
        match src {
            ScanMode::Off => fidl_avrcp::ScanMode::Off,
            ScanMode::AllTrackScan => fidl_avrcp::ScanMode::AllTrackScan,
            ScanMode::GroupScan => fidl_avrcp::ScanMode::GroupScan,
        }
    }
}

decodable_enum! {
    pub enum Equalizer <u8, Error, OutOfRange> {
        Off = 0x01,
        On = 0x02,
    }
}

impl From<fidl_avrcp::Equalizer> for Equalizer {
    fn from(src: fidl_avrcp::Equalizer) -> Equalizer {
        match src {
            fidl_avrcp::Equalizer::Off => Equalizer::Off,
            fidl_avrcp::Equalizer::On => Equalizer::On,
        }
    }
}

impl From<Equalizer> for fidl_avrcp::Equalizer {
    fn from(src: Equalizer) -> fidl_avrcp::Equalizer {
        match src {
            Equalizer::Off => fidl_avrcp::Equalizer::Off,
            Equalizer::On => fidl_avrcp::Equalizer::On,
        }
    }
}

// TODO(https://fxbug.dev/42117316): Add support to handle custom attributes.
#[derive(Clone, Debug)]
pub struct PlayerApplicationSettings {
    pub equalizer: Option<Equalizer>,
    pub repeat_status_mode: Option<RepeatStatusMode>,
    pub shuffle_mode: Option<ShuffleMode>,
    pub scan_mode: Option<ScanMode>,
}

impl PlayerApplicationSettings {
    pub fn new(
        equalizer: Option<Equalizer>,
        repeat_status_mode: Option<RepeatStatusMode>,
        shuffle_mode: Option<ShuffleMode>,
        scan_mode: Option<ScanMode>,
    ) -> Self {
        Self { equalizer, repeat_status_mode, shuffle_mode, scan_mode }
    }

    // Given an attribute specified by `attribute_id`, sets the field to None.
    pub fn clear_attribute(&mut self, attribute_id: PlayerApplicationSettingAttributeId) {
        match attribute_id {
            PlayerApplicationSettingAttributeId::Equalizer => self.equalizer = None,
            PlayerApplicationSettingAttributeId::ScanMode => self.scan_mode = None,
            PlayerApplicationSettingAttributeId::RepeatStatusMode => self.repeat_status_mode = None,
            PlayerApplicationSettingAttributeId::ShuffleMode => self.shuffle_mode = None,
        };
    }
}

impl From<&fidl_avrcp::PlayerApplicationSettings> for PlayerApplicationSettings {
    fn from(src: &fidl_avrcp::PlayerApplicationSettings) -> PlayerApplicationSettings {
        let mut equalizer: Option<Equalizer> = None;
        let mut repeat_status_mode: Option<RepeatStatusMode> = None;
        let mut shuffle_mode: Option<ShuffleMode> = None;
        let mut scan_mode: Option<ScanMode> = None;

        if let Some(eq) = src.equalizer {
            equalizer = Some(eq.into());
        }
        if let Some(rsm) = src.repeat_status_mode {
            repeat_status_mode = Some(rsm.into());
        }
        if let Some(shm) = src.shuffle_mode {
            shuffle_mode = Some(shm.into());
        }
        if let Some(scm) = src.scan_mode {
            scan_mode = Some(scm.into());
        }

        PlayerApplicationSettings::new(equalizer, repeat_status_mode, shuffle_mode, scan_mode)
    }
}

impl From<PlayerApplicationSettings> for fidl_avrcp::PlayerApplicationSettings {
    fn from(src: PlayerApplicationSettings) -> fidl_avrcp::PlayerApplicationSettings {
        fidl_avrcp::PlayerApplicationSettings {
            equalizer: src.equalizer.map(Into::into),
            repeat_status_mode: src.repeat_status_mode.map(Into::into),
            shuffle_mode: src.shuffle_mode.map(Into::into),
            scan_mode: src.scan_mode.map(Into::into),
            ..Default::default()
        }
    }
}

/// Takes a PlayerApplicationSettings and returns a vector of (attribute_id, value).
// TODO(https://fxbug.dev/42117316): Add support to handle custom attributes.
pub fn settings_to_vec(
    settings: &PlayerApplicationSettings,
) -> Vec<(PlayerApplicationSettingAttributeId, u8)> {
    let mut attribute_id_vals = vec![];
    if let Some(equalizer) = settings.equalizer {
        let value: Equalizer = equalizer.into();
        attribute_id_vals.push((PlayerApplicationSettingAttributeId::Equalizer, u8::from(&value)));
    }
    if let Some(repeat_status_mode) = settings.repeat_status_mode {
        let value: RepeatStatusMode = repeat_status_mode.into();
        attribute_id_vals
            .push((PlayerApplicationSettingAttributeId::RepeatStatusMode, u8::from(&value)));
    }
    if let Some(shuffle_mode) = settings.shuffle_mode {
        let value: ShuffleMode = shuffle_mode.into();
        attribute_id_vals
            .push((PlayerApplicationSettingAttributeId::ShuffleMode, u8::from(&value)));
    }
    if let Some(scan_mode) = settings.scan_mode {
        let value: ScanMode = scan_mode.into();
        attribute_id_vals.push((PlayerApplicationSettingAttributeId::ScanMode, u8::from(&value)));
    }

    attribute_id_vals
}
