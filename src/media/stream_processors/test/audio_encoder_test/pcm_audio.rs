// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use byteorder::{ByteOrder, NativeEndian};
use fidl_fuchsia_media::*;
use itertools::Itertools;
use stream_processor_test::*;

const PCM_MIME_TYPE: &str = "audio/pcm";

fn sample_bytes_for_format(pcm_format: &PcmFormat2) -> usize {
    let bits_per_sample = pcm_format.bits_per_sample.expect("bits_per_sample required") as usize;
    pcm_format
        .bytes_per_sample
        .map(|bytes| bytes as usize)
        .unwrap_or_else(|| bits_per_sample.div_ceil(8))
}

fn append_s16_sample_in_format(
    buffer: &mut Vec<u8>,
    pcm_format: &PcmFormat2,
    sample_index: usize,
    sample: i16,
) {
    assert_eq!(pcm_format.pcm_mode, Some(AudioPcmMode2::Linear));
    let bits_per_sample = pcm_format.bits_per_sample.expect("bits_per_sample required");
    let sample_bytes = sample_bytes_for_format(pcm_format);
    match (bits_per_sample, sample_bytes) {
        (16, 2) => {
            let mut bytes = [0u8; std::mem::size_of::<i16>()];
            NativeEndian::write_i16(&mut bytes, sample);
            buffer.extend_from_slice(&bytes);
        }
        (24, 3) => {
            let [lo, hi] = sample.to_le_bytes();
            buffer.extend_from_slice(&[0x00, lo, hi]);
        }
        (24, 4) => {
            // `PcmFormat2` places valid 24-bit samples in the high-order (left-justified) bits
            // `31..8`, and requires receivers to ignore low-order padding bits `7..0`. Populate
            // non-zero low-byte padding so tests verify receivers ignore bits `7..0`.
            let low_byte_padding =
                ((sample_index as u32).wrapping_mul(73).wrapping_add(0xA5)) & 0xFF;
            let word = ((sample as u16 as u32) << 16) | low_byte_padding;
            buffer.extend_from_slice(&word.to_ne_bytes());
        }
        _ => panic!(
            "Unsupported test PCM format: bits_per_sample={}, bytes_per_sample={}",
            bits_per_sample, sample_bytes
        ),
    }
}

#[derive(Clone, Debug)]
pub struct PcmAudio {
    pcm_format: PcmFormat2,
    buffer: Vec<u8>,
}

impl PcmAudio {
    pub fn create_saw_wave(pcm_format: PcmFormat2, frame_count: usize) -> Self {
        const FREQUENCY: f32 = 20.0;
        const AMPLITUDE: f32 = 0.2;

        assert_eq!(pcm_format.pcm_mode, Some(AudioPcmMode2::Linear));

        let channel_count = pcm_format.channel_map.as_ref().unwrap().len();
        let frames_per_second = pcm_format.frames_per_second.unwrap();
        let pcm_frame_size = sample_bytes_for_format(&pcm_format) * channel_count;
        let samples_per_frame = channel_count;
        let sample_count = frame_count * samples_per_frame;

        let mut buffer = Vec::with_capacity(frame_count * pcm_frame_size);

        for i in 0..sample_count {
            let frame = (i / samples_per_frame) as f32;
            let value = ((frame * FREQUENCY / (frames_per_second as f32)) % 1.0) * AMPLITUDE;
            let sample = (value * i16::MAX as f32) as i16;
            append_s16_sample_in_format(&mut buffer, &pcm_format, i, sample);
        }

        Self { pcm_format, buffer }
    }

    pub fn create_from_data(pcm_format: PcmFormat2, raw_data: &[i16]) -> Self {
        let mut buffer = Vec::with_capacity(raw_data.len() * sample_bytes_for_format(&pcm_format));
        for (i, &pcm_sample) in raw_data.iter().enumerate() {
            append_s16_sample_in_format(&mut buffer, &pcm_format, i, pcm_sample);
        }
        Self { pcm_format, buffer }
    }

    pub fn create_from_raw_bytes(pcm_format: PcmFormat2, buffer: Vec<u8>) -> Self {
        Self { pcm_format, buffer }
    }

    pub fn pcm_format(&self) -> &PcmFormat2 {
        &self.pcm_format
    }

    pub fn frame_size(&self) -> usize {
        self.pcm_format.channel_map.as_ref().unwrap().len()
            * sample_bytes_for_format(&self.pcm_format)
    }
}

/// Generates timestamps according to a timebase and rate of playback of uncompressed audio.
///
/// Since the rate is constant, this can also be used to extrapolate timestamps.
pub struct TimestampGenerator {
    bytes_per_second: usize,
    timebase: u64,
}

impl TimestampGenerator {
    pub fn timestamp_at(&self, input_index: usize) -> u64 {
        let bps = self.bytes_per_second as u64;
        (input_index as u64) * self.timebase / bps
    }
}

pub struct PcmAudioStream<I> {
    pub pcm_audio: PcmAudio,
    pub encoder_settings: EncoderSettings,
    pub frames_per_packet: I,
    pub timebase: Option<u64>,
}

impl<I> PcmAudioStream<I>
where
    I: Iterator<Item = usize> + Clone,
{
    pub fn bytes_per_second(&self) -> usize {
        self.pcm_audio.pcm_format.frames_per_second.unwrap() as usize * self.pcm_audio.frame_size()
    }

    pub fn timestamp_generator(&self) -> Option<TimestampGenerator> {
        self.timebase.map(|timebase| TimestampGenerator {
            bytes_per_second: self.bytes_per_second(),
            timebase,
        })
    }
}

impl<I> ElementaryStream for PcmAudioStream<I>
where
    I: Iterator<Item = usize> + Clone,
{
    fn format_details(&self, format_details_version_ordinal: u64) -> FormatDetails {
        FormatDetails {
            pcm_format: Some(self.pcm_audio.pcm_format.clone()),
            encoder_settings: Some(self.encoder_settings.clone()),
            format_details_version_ordinal: Some(format_details_version_ordinal),
            mime_type: Some(String::from(PCM_MIME_TYPE)),
            oob_bytes: None,
            pass_through_parameters: None,
            timebase: self.timebase,
            ..Default::default()
        }
    }

    fn is_access_units(&self) -> bool {
        false
    }

    fn stream<'a>(&'a self) -> Box<dyn Iterator<Item = ElementaryStreamChunk> + 'a> {
        let data = self.pcm_audio.buffer.as_slice();
        let frame_size = self.pcm_audio.frame_size();
        let mut offset = 0;
        let mut frames_per_packet = self.frames_per_packet.clone();

        let chunks = (0..)
            .map(move |_| {
                let number_of_frames_for_this_packet = frames_per_packet.next()?;
                let payload_size = number_of_frames_for_this_packet * frame_size;
                let payload_size = data
                    .len()
                    .checked_sub(offset)
                    .map(|remaining_bytes| std::cmp::min(remaining_bytes, payload_size))
                    .filter(|payload_size| *payload_size > 0)?;

                let range = offset..(offset + payload_size);
                let result = data.get(range).map(|range| (offset, range))?;
                offset += payload_size;

                Some(result)
            })
            .while_some();
        Box::new(chunks.map(move |(input_index, data)| {
            ElementaryStreamChunk {
                start_access_unit: false,
                known_end_access_unit: false,
                data: data.to_vec(),
                significance: Significance::Audio(AudioSignificance::PcmFrames),
                timestamp: self
                    .timestamp_generator()
                    .as_ref()
                    .map(|timestamp_generator| timestamp_generator.timestamp_at(input_index)),
            }
        }))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    // Settings are arbitrary; we just need to construct an instance.
    const DUMMY_ENCODER_SETTINGS: EncoderSettings = EncoderSettings::Sbc(SbcEncoderSettings {
        sub_bands: SbcSubBands::SubBands8,
        allocation: SbcAllocation::AllocLoudness,
        block_count: SbcBlockCount::BlockCount16,
        channel_mode: SbcChannelMode::JointStereo,
        bit_pool: 59,
    });

    #[fuchsia::test]
    fn elementary_chunk_data() {
        let pcm_format = PcmFormat2 {
            pcm_mode: Some(AudioPcmMode2::Linear),
            bits_per_sample: Some(16),
            frames_per_second: Some(44100),
            channel_map: Some(vec![AudioChannelId2::Fl, AudioChannelId2::Fr]),
            ..Default::default()
        };
        let pcm_audio = PcmAudio::create_saw_wave(pcm_format, /*frame_count=*/ 100);

        let stream = PcmAudioStream {
            pcm_audio: pcm_audio.clone(),
            encoder_settings: DUMMY_ENCODER_SETTINGS,
            frames_per_packet: (0..).map(|_| 40),
            timebase: None,
        };

        let actual: Vec<u8> = stream.stream().flat_map(|chunk| chunk.data.clone()).collect();
        assert_eq!(pcm_audio.buffer, actual);
    }

    #[fuchsia::test]
    fn saw_wave_matches_hash() {
        use hex;
        use mundane::hash::*;

        /// This was obtained by writing the buffer out to file and inspecting the wave on each channel.
        const GOLDEN_DIGEST: &str =
            "2bf4f233a179f0cb572b72570a28c07a334e406baa7fb4fc65f641b82d0ae64a";

        let pcm_audio = PcmAudio::create_saw_wave(
            PcmFormat2 {
                pcm_mode: Some(AudioPcmMode2::Linear),
                bits_per_sample: Some(16),
                frames_per_second: Some(44100),
                channel_map: Some(vec![AudioChannelId2::Fl, AudioChannelId2::Fr]),
                ..Default::default()
            },
            /*frame_count=*/ 50000,
        );

        let actual_digest = hex::encode(Sha256::hash(&pcm_audio.buffer).bytes());
        assert_eq!(&actual_digest, GOLDEN_DIGEST);
    }

    #[fuchsia::test]
    fn stream_timestamps() {
        let pcm_format = PcmFormat2 {
            pcm_mode: Some(AudioPcmMode2::Linear),
            bits_per_sample: Some(16),
            frames_per_second: Some(50),
            channel_map: Some(vec![AudioChannelId2::Fl, AudioChannelId2::Fr]),
            ..Default::default()
        };
        let pcm_audio = PcmAudio::create_saw_wave(pcm_format, /*frame_count=*/ 100);

        let stream = PcmAudioStream {
            pcm_audio: pcm_audio.clone(),
            encoder_settings: DUMMY_ENCODER_SETTINGS,
            frames_per_packet: (0..).map(|_| 50),
            timebase: Some(zx::MonotonicDuration::from_seconds(1).into_nanos() as u64),
        };

        let mut chunks = stream.stream();

        assert_eq!(&chunks.next().and_then(|chunk| chunk.timestamp), &Some(0));
        assert_eq!(
            chunks.next().and_then(|chunk| chunk.timestamp),
            Some(zx::MonotonicDuration::from_seconds(1).into_nanos() as u64)
        );
    }

    #[fuchsia::test]
    fn stream_timestamps_24bit_packed_and_padded() {
        for (bytes_per_sample, expected_frame_size) in [(None, 6usize), (Some(3), 6), (Some(4), 8)]
        {
            let pcm_format = PcmFormat2 {
                pcm_mode: Some(AudioPcmMode2::Linear),
                bits_per_sample: Some(24),
                bytes_per_sample,
                frames_per_second: Some(50),
                channel_map: Some(vec![AudioChannelId2::Fl, AudioChannelId2::Fr]),
                ..Default::default()
            };
            let pcm_audio = PcmAudio::create_saw_wave(pcm_format, /*frame_count=*/ 100);
            assert_eq!(pcm_audio.frame_size(), expected_frame_size);

            let stream = PcmAudioStream {
                pcm_audio,
                encoder_settings: DUMMY_ENCODER_SETTINGS,
                frames_per_packet: (0..).map(|_| 50),
                timebase: Some(zx::MonotonicDuration::from_seconds(1).into_nanos() as u64),
            };
            assert_eq!(stream.bytes_per_second(), 50 * expected_frame_size);

            let mut chunks = stream.stream();
            assert_eq!(chunks.next().and_then(|chunk| chunk.timestamp), Some(0));
            assert_eq!(
                chunks.next().and_then(|chunk| chunk.timestamp),
                Some(zx::MonotonicDuration::from_seconds(1).into_nanos() as u64)
            );
        }
    }
}
