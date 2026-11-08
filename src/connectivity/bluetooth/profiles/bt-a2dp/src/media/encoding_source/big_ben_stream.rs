// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bt_a2dp::media_task::MediaTaskError;
use fidl_fuchsia_media::PcmFormat2;
use fuchsia_async as fasync;
use fuchsia_audio_device::AudioStreamItem;
use fuchsia_bluetooth::types::PeerId;
use fuchsia_inspect as inspect;
use fuchsia_inspect_derive::Inspect;
use futures::FutureExt;
use futures::stream::FusedStream;
use futures::task::{Context, Poll};
use std::pin::Pin;
use zx;

use super::{AudioSourceStream, AudioSourceStreamBuilder};
use crate::PcmAudio;

pub struct SawWaveStream {
    format: PcmFormat2,
    frames_per_second: usize,
    frequency_hops: Vec<f32>,
    next_frame_timer: Pin<Box<fasync::Timer>>,
    /// the last time we delivered frames.
    last_frame_time: Option<zx::MonotonicInstant>,
    terminated: bool,
    inspect_node: inspect::Node,
}

impl Inspect for &mut SawWaveStream {
    fn iattach(
        self,
        parent: &inspect::Node,
        name: impl AsRef<str>,
    ) -> Result<(), fuchsia_inspect_derive::AttachError> {
        self.inspect_node = parent.create_child(name.as_ref());
        self.inspect_node.record_string("format", format!("{:?}", self.format));
        self.inspect_node.record_string("hops", format!("{:?}", self.frequency_hops));
        Ok(())
    }
}

impl futures::Stream for SawWaveStream {
    type Item = fuchsia_audio_device::Result<AudioStreamItem>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminated {
            return Poll::Ready(None);
        }
        let now = zx::MonotonicInstant::get();
        if self.last_frame_time.is_none() {
            self.last_frame_time = Some(now - zx::Duration::from_seconds(1));
        }
        let last_time = self.last_frame_time.as_ref().unwrap().clone();
        let repeats = (now - last_time).into_seconds();
        if repeats == 0 {
            self.next_frame_timer
                .as_mut()
                .reset((last_time + zx::Duration::from_seconds(1)).into());
            let poll = self.next_frame_timer.poll_unpin(cx);
            assert!(poll == Poll::Pending);
            return Poll::Pending;
        }
        let next_freq = self.frequency_hops.remove(0);
        let audio = match PcmAudio::create_saw_wave(
            next_freq,
            0.2,
            self.format.clone(),
            self.frames_per_second,
        ) {
            Ok(audio) => audio,
            Err(_e) => {
                self.terminated = true;
                return Poll::Ready(Some(Err(fuchsia_audio_device::Error::InvalidArgs)));
            }
        };
        self.frequency_hops.push(next_freq);
        self.last_frame_time = Some(last_time + zx::Duration::from_seconds(1));
        Poll::Ready(Some(Ok(audio.buffer.into())))
    }
}

impl FusedStream for SawWaveStream {
    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

impl SawWaveStream {
    pub fn new_big_ben(format: PcmFormat2) -> Result<Self, MediaTaskError> {
        let (_frame_bytes, frames_per_second) = fuchsia_audio_device::validate_pcm_format2(&format)
            .map_err(|e| MediaTaskError::Other(e.to_string()))?;
        if format.bits_per_sample != Some(16) || format.bytes_per_sample.unwrap_or(2) != 2 {
            return Err(MediaTaskError::NotSupported);
        }
        Ok(Self {
            format,
            frames_per_second: frames_per_second as usize,
            // G# - 415.30 F# - 369.99 E - 329.63 B - 246.94
            // Clock Chimes: (silence) E, G#, F#, B, E, F#, G#, E, G#, E, F#, B, B, F#, G# E
            frequency_hops: vec![
                0.0, 329.63, 415.30, 369.99, 246.94, 329.63, 369.99, 415.30, 329.63, 415.30,
                329.63, 369.99, 246.94, 246.94, 369.99, 415.30, 329.63,
            ],
            next_frame_timer: Box::pin(fasync::Timer::new(fasync::MonotonicInstant::INFINITE_PAST)),
            last_frame_time: None,
            terminated: false,
            inspect_node: Default::default(),
        })
    }
}

impl AudioSourceStream for SawWaveStream {}

#[derive(Default)]
pub(crate) struct BigBenStream {}

impl AudioSourceStreamBuilder for BigBenStream {
    fn build(
        &self,
        _peer_id: &PeerId,
        pcm_format: PcmFormat2,
        _external_delay: std::time::Duration,
        inspect_parent: &mut inspect::Node,
    ) -> Result<Box<dyn AudioSourceStream>, MediaTaskError> {
        let mut stream = SawWaveStream::new_big_ben(pcm_format)?;
        let _ = stream.iattach(inspect_parent, "audio_source");
        Ok(Box::new(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_media::{AudioChannelId2, AudioPcmMode2};

    #[fuchsia::test]
    fn big_ben_stream_validates_pcm_format() {
        let _exec = fasync::TestExecutor::new();
        let builder = BigBenStream::default();
        let peer_id = PeerId(1);
        let inspector = inspect::Inspector::default();
        let mut node = inspector.root().create_child("test");
        let valid_format = PcmFormat2 {
            pcm_mode: Some(AudioPcmMode2::Linear),
            bits_per_sample: Some(16),
            frames_per_second: Some(48000),
            channel_map: Some(vec![AudioChannelId2::Fl, AudioChannelId2::Fr]),
            ..Default::default()
        };

        assert!(
            builder
                .build(&peer_id, valid_format.clone(), std::time::Duration::ZERO, &mut node)
                .is_ok()
        );

        let invalid_formats = [
            PcmFormat2 { pcm_mode: None, ..valid_format.clone() },
            PcmFormat2 { bits_per_sample: Some(24), ..valid_format.clone() },
            PcmFormat2 { bytes_per_sample: Some(4), ..valid_format.clone() },
            PcmFormat2 { frames_per_second: None, ..valid_format.clone() },
            PcmFormat2 { frames_per_second: Some(0), ..valid_format.clone() },
            PcmFormat2 { channel_map: None, ..valid_format.clone() },
            PcmFormat2 { channel_map: Some(vec![]), ..valid_format },
        ];
        for format in invalid_formats {
            assert!(builder.build(&peer_id, format, std::time::Duration::ZERO, &mut node).is_err());
        }
    }
}
