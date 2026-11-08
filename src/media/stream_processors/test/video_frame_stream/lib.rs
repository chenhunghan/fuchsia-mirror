// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::format_err;
use async_trait::async_trait;
use fidl_fuchsia_images2 as images2;
use fidl_fuchsia_media::*;
use std::rc::Rc;
use stream_processor_test::*;

#[derive(Debug)]
pub struct VideoFrame {
    data: Vec<u8>,
}

/// Container for raw video frame
impl VideoFrame {
    /// Generates a frame with specified `format`.
    /// `step` is for diagonally shifting the checkerboard pattern.
    pub fn create(format: &images2::ImageFormat, step: usize) -> Self {
        // For 4:2:0 YUV, the UV data is 1/2 the size of the Y data,
        // so the size of the frame is 3/2 the size of the Y plane.
        let width = *format.bytes_per_row.as_ref().unwrap() as usize;
        let height = format.size.as_ref().unwrap().height as usize;
        let frame_size = width * height * 3usize / 2usize;
        let mut data = vec![128; frame_size];

        // generate checkerboard
        const NUM_BLOCKS: usize = 8usize;
        let block_size = width / NUM_BLOCKS;
        let mut y_on = true;
        let mut x_on = true;
        for y in 0..height {
            if y % block_size == 0 {
                y_on = !y_on;
                x_on = y_on;
            }
            for x in 0..width {
                if x % block_size == 0 {
                    x_on = !x_on;
                }
                let luma = if x_on { 255 } else { 0 };
                let y_s = (y + step) % height;
                let x_s = (x + step) % width;
                data[y_s * width + x_s] = luma;
            }
        }

        Self { data }
    }
}

/// Generates timestamps according to a timebase and rate of playback of uncompressed video.
///
/// Since the rate is constant, this can also be used to extrapolate timestamps.
pub struct TimestampGenerator {
    pub timebase: u64,
    pub frames_per_second: usize,
}

impl TimestampGenerator {
    pub fn timestamp_at(&self, input_index: usize) -> u64 {
        let fps = self.frames_per_second as u64;
        (input_index as u64) * self.timebase / fps
    }
}

/// Validates that timestamps match the expected output from the specified generator
pub struct TimestampValidator {
    pub generator: Option<TimestampGenerator>,
}

#[async_trait(?Send)]
impl OutputValidator for TimestampValidator {
    async fn validate(&self, output: &[Output]) -> Result<()> {
        let packets = output_packets(output);
        for (pos, packet) in packets.enumerate() {
            if packet.packet.timestamp_ish.is_none() {
                return Err(format_err!("Missing timestamp"));
            }

            if let Some(generator) = &self.generator {
                let current_ts = packet.packet.timestamp_ish.unwrap();
                let expected_ts = generator.timestamp_at(pos);

                if current_ts != expected_ts {
                    return Err(format_err!(
                        "Unexpected timestamp {} (expected {})",
                        current_ts,
                        expected_ts
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Implements an `ElementaryStream` of raw video frames with the specified `format`, intended to be
/// fed to an encoder StreamProcessor.
pub struct VideoFrameStream {
    pub num_frames: usize,
    pub format: images2::ImageFormat,
    pub encoder_settings: Rc<dyn Fn() -> EncoderSettings>,
    pub frames_per_second: usize,
    pub timebase: Option<u64>,
    pub mime_type: String,
}

impl VideoFrameStream {
    pub fn create(
        format: images2::ImageFormat,
        num_frames: usize,
        encoder_settings: Rc<dyn Fn() -> EncoderSettings>,
        frames_per_second: usize,
        timebase: Option<u64>,
        mime_type: &str,
    ) -> Result<Self> {
        Ok(Self {
            num_frames,
            format,
            encoder_settings,
            frames_per_second,
            timebase,
            mime_type: mime_type.to_string(),
        })
    }

    pub fn timestamp_generator(&self) -> Option<TimestampGenerator> {
        self.timebase.map(|timebase| TimestampGenerator {
            frames_per_second: self.frames_per_second,
            timebase,
        })
    }
}

impl ElementaryStream for VideoFrameStream {
    fn format_details(&self, format_details_version_ordinal: u64) -> FormatDetails {
        FormatDetails {
            image_format: Some(self.format.clone()),
            encoder_settings: Some((self.encoder_settings)()),
            format_details_version_ordinal: Some(format_details_version_ordinal),
            mime_type: Some(self.mime_type.to_string()),
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
        let timestamp_generator = self.timestamp_generator();
        Box::new((0..self.num_frames).map(move |input_index| {
            let frame = VideoFrame::create(&self.format, input_index);
            ElementaryStreamChunk {
                start_access_unit: false,
                known_end_access_unit: false,
                data: frame.data,
                significance: Significance::Video(VideoSignificance::Picture),
                timestamp: timestamp_generator
                    .as_ref()
                    .map(|timestamp_generator| timestamp_generator.timestamp_at(input_index)),
            }
        }))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use fidl_fuchsia_math::{RectU, SizeU};

    #[derive(Debug, Copy, Clone)]
    struct TestSpec {
        pixel_format: images2::PixelFormat,
        coded_width: usize,
        coded_height: usize,
        display_width: usize,
        display_height: usize,
        bytes_per_row: usize,
    }

    impl From<TestSpec> for images2::ImageFormat {
        fn from(spec: TestSpec) -> Self {
            images2::ImageFormat {
                pixel_format: Some(spec.pixel_format),
                size: Some(SizeU {
                    width: spec.coded_width.try_into().unwrap(),
                    height: spec.coded_height.try_into().unwrap(),
                }),
                bytes_per_row: Some(spec.bytes_per_row as u32),
                display_rect: Some(RectU {
                    x: 0,
                    y: 0,
                    width: spec.display_width as u32,
                    height: spec.display_height as u32,
                }),
                color_space: Some(images2::ColorSpace::Rec709),
                ..Default::default()
            }
        }
    }

    #[fuchsia::test]
    fn stream_timestamps() {
        let test_spec = TestSpec {
            pixel_format: images2::PixelFormat::Nv12,
            coded_width: 16,
            coded_height: 16,
            display_height: 12,
            display_width: 16,
            bytes_per_row: 16,
        };

        let format: images2::ImageFormat = test_spec.into();
        let stream = VideoFrameStream::create(
            format,
            /*num_frames=*/ 2,
            Rc::new(move || -> EncoderSettings {
                EncoderSettings::H264(H264EncoderSettings {
                    bit_rate: Some(200000),
                    frame_rate: Some(60),
                    ..Default::default()
                })
            }),
            /*frames_per_second=*/ 60,
            /*timebase=*/ Some(zx::MonotonicDuration::from_seconds(1).into_nanos() as u64),
            /*mime_type=*/ "video/h264",
        )
        .expect("stream");
        let mut chunks = stream.stream();

        assert_eq!(chunks.next().and_then(|chunk| chunk.timestamp), Some(0));
        assert_eq!(
            chunks.next().and_then(|chunk| chunk.timestamp),
            Some(zx::MonotonicDuration::from_seconds(1).into_nanos() as u64 / 60)
        );
    }

    #[fuchsia::test]
    fn pattern_check() {
        let test_spec = TestSpec {
            pixel_format: images2::PixelFormat::Nv12,
            coded_width: 8,
            coded_height: 8,
            display_height: 8,
            display_width: 8,
            bytes_per_row: 8,
        };

        let format: images2::ImageFormat = test_spec.into();
        let frame = VideoFrame::create(&format, 0);
        assert_eq!(
            frame.data,
            vec![
                255, 0, 255, 0, 255, 0, 255, 0, 0, 255, 0, 255, 0, 255, 0, 255, 255, 0, 255, 0,
                255, 0, 255, 0, 0, 255, 0, 255, 0, 255, 0, 255, 255, 0, 255, 0, 255, 0, 255, 0, 0,
                255, 0, 255, 0, 255, 0, 255, 255, 0, 255, 0, 255, 0, 255, 0, 0, 255, 0, 255, 0,
                255, 0, 255, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128,
                128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128,
                128, 128
            ]
        );
    }
}
