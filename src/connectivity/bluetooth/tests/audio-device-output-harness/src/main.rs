// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Context;
use fidl_fuchsia_media::{AudioChannelId2, AudioDeviceEnumeratorMarker, AudioPcmMode2, PcmFormat2};
use fuchsia_audio_device::stream_config::SoftStreamConfig;

use futures::StreamExt;
use log::info;

#[fuchsia::main(logging_tags = ["audio-device-output-harness"])]
async fn main() -> Result<(), anyhow::Error> {
    const LOCAL_MONOTONIC_CLOCK_DOMAIN: u32 = 0;
    let pcm_format = PcmFormat2 {
        pcm_mode: Some(AudioPcmMode2::Linear),
        bits_per_sample: Some(16),
        frames_per_second: Some(48000),
        channel_map: Some(vec![AudioChannelId2::Fl, AudioChannelId2::Fr]),
        ..Default::default()
    };

    let id = [2; 16];
    let (client, mut frame_stream) = SoftStreamConfig::create_output(
        &id,
        "Fuchsia",
        "AudioOutHarness",
        LOCAL_MONOTONIC_CLOCK_DOMAIN,
        pcm_format,
        zx::MonotonicDuration::from_millis(10),
        zx::MonotonicDuration::from_millis(0),
    )?;

    // Spawn a task to read all the frames from the audio.
    // This should be started before adding it to the AudioDeviceEnumerator as it will not respond
    // to the enumerator test queries until it is active.
    let audio_read_task = fuchsia_async::Task::spawn(async move {
        info!("Starting frame stream read loop");
        while let Some(Ok(_frame)) = frame_stream.next().await {}
        info!("Frame stream loop completed");
    });

    let svc = fuchsia_component::client::connect_to_protocol::<AudioDeviceEnumeratorMarker>()
        .context("Failed to connect to AudioDeviceEnumerator")?;
    svc.add_device_by_channel("AudioOutHarness", false, client)?;

    audio_read_task.await;

    Ok(())
}
