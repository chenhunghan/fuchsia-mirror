// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/*!

Provides an asynchronous wrapper around the StreamProcessor API to encode/decode audio packets.

The main object to use is the StreamProcessor with either the `create_encoder` or `create_decoder`
methods which are passed the input audio parameters and the encoder settings to provide to the
StreamProcessor for the encoder case. The `input_pcm_format` can be found as
`fidl_fuchsia_media::PcmFormat2`. The `encoder_settings` is found as
`fidl_fuchsia_media::EncoderSettings`.

After creating the encoder/decoder, audio input data can be provided either by using the
StreamProcessor as an `AsyncWrite` provider.  The resulting audio is provided via the output stream
which can be acquired via the `StreamProcessor::take_output_stream` call.

# Example

```rust

let pcm_format = PcmFormat2 {
    pcm_mode: Some(AudioPcmMode2::Linear),
    bits_per_sample: Some(16),
    frames_per_second: Some(44100),
    channel_map: Some(vec![AudioChannelId2::Fc]),
    ..Default::default()
};

// Some sample SBC encoder settings.
let sbc_encoder_settings = EncoderSettings::Sbc(SbcEncoderSettings {
    sub_bands,
    block_count,
    allocation: SbcAllocation::AllocLoudness,
    channel_mode: SbcChannelMode::Mono,
    bit_pool: 59,
});


// Read a WAV file into memory.
let mut raw_audio = Vec::new();
File::open("sample.wav")?.read_to_end(&mut raw_audio)?;

let mut encoder = StreamProcessor::create_encoder(pcm_format, sbc_encoder_settings)?;

// 44 bytes offset skips the RIFF header in the wav file.
// Delivering 16 audio frames at once that are 2 bytes each
for audio_frames in raw_audio[44..].chunks(2 * 16) {
    encoder.write(&audio_frames).await?;
}

encoder.close()?;

let mut encoded_stream = encoder.take_output_stream()?;

let mut output_file = File::create("sample.sbc")?;

while let Some(data) = encoded_stream.try_next().await? {
    output_file.write(&data)?;
}

```

The output stream will begin producing data as soon as the StreamProcessor starts providing it -
the writing of the uncompressed data and the reading of the output stream can happen on separate
tasks.

*/

/// Interface to CodecFactory
pub mod stream_processor;
pub use stream_processor::{StreamProcessor, StreamProcessorOutputStream};

mod buffer_collection_constraints;
pub mod sysmem_allocator;
