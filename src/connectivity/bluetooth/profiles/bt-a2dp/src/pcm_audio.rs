// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, format_err};
use byteorder::{ByteOrder, NativeEndian};
use fidl_fuchsia_media::PcmFormat2;

const PCM_SAMPLE_SIZE: usize = 2;

#[derive(Clone, Debug)]
pub struct PcmAudio {
    // Retained for `Debug` output.
    #[allow(dead_code)]
    pub pcm_format: PcmFormat2,
    // Retained for `Debug` output.
    #[allow(dead_code)]
    pub frequency: f32,
    // Retained for `Debug` output.
    #[allow(dead_code)]
    pub amplitude: f32,
    pub buffer: Vec<u8>,
}

impl PcmAudio {
    pub fn create_saw_wave(
        frequency: f32,
        amplitude: f32,
        pcm_format: PcmFormat2,
        frame_count: usize,
    ) -> Result<Self, Error> {
        let PcmFormat2 {
            frames_per_second: Some(frames_per_second),
            channel_map: Some(ref channel_map),
            ..
        } = pcm_format
        else {
            return Err(format_err!("PcmFormat2 missing frames_per_second or channel_map"));
        };
        if frames_per_second == 0 || channel_map.is_empty() {
            return Err(format_err!(
                "PcmFormat2 frames_per_second and channel_map must be non-empty"
            ));
        }
        let samples_per_frame = channel_map.len();
        let sample_count = frame_count * samples_per_frame;
        let frames_per_second = frames_per_second as f32;

        let mut buffer = Vec::with_capacity(sample_count * PCM_SAMPLE_SIZE);

        let amplitude = amplitude.min(1.0).max(0.0);

        for i in 0..sample_count {
            let frame = (i / samples_per_frame) as f32;
            let value = ((frame * frequency / frames_per_second) % 1.0) * amplitude;
            let sample = (value * i16::MAX as f32) as i16;

            let mut sample_bytes = [0; std::mem::size_of::<i16>()];
            NativeEndian::write_i16(&mut sample_bytes, sample);
            buffer.extend_from_slice(&sample_bytes);
        }

        Ok(Self { pcm_format, frequency, amplitude, buffer })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use fidl_fuchsia_media::{AudioChannelId2, AudioPcmMode2};
    use sha2::{Digest as _, Sha256};

    #[test]
    fn saw_wave_matches_hash() {
        /// This was obtained by writing the buffer out to file and inspecting the wave on each channel.
        const GOLDEN_DIGEST: &str =
            "2bf4f233a179f0cb572b72570a28c07a334e406baa7fb4fc65f641b82d0ae64a";

        let pcm_audio = PcmAudio::create_saw_wave(
            20.0,
            0.2,
            PcmFormat2 {
                pcm_mode: Some(AudioPcmMode2::Linear),
                bits_per_sample: Some(16),
                frames_per_second: Some(44100),
                channel_map: Some(vec![AudioChannelId2::Fl, AudioChannelId2::Fr]),
                ..Default::default()
            },
            /*frame_count=*/ 50000,
        )
        .expect("saw wave");

        let actual_digest = hex::encode(Sha256::digest(&pcm_audio.buffer));
        assert_eq!(&actual_digest, GOLDEN_DIGEST);
    }
}
