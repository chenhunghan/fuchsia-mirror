// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod test_suite;

use crate::test_suite::*;
use fidl_fuchsia_images2 as images2;
use fidl_fuchsia_math::{RectU, SizeU};
use fidl_fuchsia_media::*;
use fuchsia_async as fasync;
use h264_stream::*;
use std::rc::Rc;
use stream_processor_test::*;

// Instructions for capturing output of encoder:
// 1. Set the `output_file` field to write the encoded output into "/tmp/".
// 2. Add exec.run_singlethreaded(future::pending()) at the end of the test so it doesn't exit.
// 3. Use `ffx component copy` to copy the file to the host.

#[fuchsia::test]
fn h264_stream_output_generated() -> Result<()> {
    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 240;
    let mut exec = fasync::TestExecutor::new();

    let test_case = H264EncoderTestCase {
        input_format: images2::ImageFormat {
            pixel_format: Some(images2::PixelFormat::Nv12),
            color_space: Some(images2::ColorSpace::Rec601Pal),
            size: Some(SizeU { width: WIDTH, height: HEIGHT }),
            display_rect: Some(RectU { x: 0, y: 0, width: WIDTH, height: HEIGHT }),
            bytes_per_row: Some(WIDTH),
            ..Default::default()
        },
        num_frames: 6,
        settings: Rc::new(move || -> EncoderSettings {
            EncoderSettings::H264(H264EncoderSettings {
                bit_rate: Some(2000000),
                frame_rate: Some(30),
                gop_size: Some(2),
                ..Default::default()
            })
        }),
        expected_nals: Some(vec![
            H264NalKind::SPS,
            H264NalKind::PPS,
            H264NalKind::IDR,
            H264NalKind::NonIDR,
            H264NalKind::NonIDR,
            H264NalKind::SPS,
            H264NalKind::PPS,
            H264NalKind::IDR,
            H264NalKind::NonIDR,
            H264NalKind::NonIDR,
        ]),
        decode_output: true,
        normalized_sad_threshold: Some(2.0),
        output_file: None,
    };
    exec.run_singlethreaded(test_case.run())?;
    Ok(())
}

#[fuchsia::test]
fn test_serial_h264_encoder_on_same_codec() -> Result<()> {
    use fidl_fuchsia_sysmem2 as sysmem2;
    use stream_processor_encoder_factory::EncoderFactory;
    use video_frame_stream::{TimestampValidator, VideoFrameStream};

    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 240;
    let mut exec = fasync::TestExecutor::new();

    let input_format = images2::ImageFormat {
        pixel_format: Some(images2::PixelFormat::Nv12),
        color_space: Some(images2::ColorSpace::Rec601Pal),
        size: Some(SizeU { width: WIDTH, height: HEIGHT }),
        display_rect: Some(RectU { x: 0, y: 0, width: WIDTH, height: HEIGHT }),
        bytes_per_row: Some(WIDTH),
        ..Default::default()
    };

    let settings = Rc::new(move || -> EncoderSettings {
        EncoderSettings::H264(H264EncoderSettings {
            bit_rate: Some(2000000),
            frame_rate: Some(30),
            gop_size: Some(2),
            ..Default::default()
        })
    });

    let create_case = |name: &'static str, stream_lifetime_ordinal: u64| -> Result<TestCase> {
        let stream = Rc::new(VideoFrameStream::create(
            input_format.clone(),
            6, // num_frames
            settings.clone(),
            30, // frame_rate
            Some(zx::MonotonicDuration::from_seconds(1).into_nanos() as u64),
            "video/h264",
        )?);

        let format_constraints = sysmem2::ImageFormatConstraints {
            pixel_format: Some(*input_format.pixel_format.as_ref().unwrap()),
            color_spaces: Some(vec![images2::ColorSpace::Rec709]),
            required_max_size: input_format.size.clone(),
            ..image_format_constraints_default()
        };

        let stream_options = Some(StreamOptions {
            input_buffer_collection_constraints: Some(sysmem2::BufferCollectionConstraints {
                image_format_constraints: Some(vec![format_constraints]),
                ..buffer_collection_constraints_default()
            }),
            ..StreamOptions::default()
        });

        let expected_nals = vec![
            H264NalKind::SPS,
            H264NalKind::PPS,
            H264NalKind::IDR,
            H264NalKind::NonIDR,
            H264NalKind::NonIDR,
            H264NalKind::SPS,
            H264NalKind::PPS,
            H264NalKind::IDR,
            H264NalKind::NonIDR,
            H264NalKind::NonIDR,
        ];

        let validators: Vec<Rc<dyn OutputValidator>> = vec![
            Rc::new(H264NalValidator { expected_nals: Some(expected_nals), output_file: None }),
            Rc::new(TimestampValidator { generator: stream.timestamp_generator() }),
            Rc::new(H264DecoderValidator {
                num_frames: 6,
                input_stream: stream.clone(),
                normalized_sad_threshold: 2.0,
                require_sw: true,
            }),
            Rc::new(TerminatesWithValidator {
                expected_terminal_output: Output::Eos { stream_lifetime_ordinal },
            }),
        ];

        Ok(TestCase { name, stream, validators, stream_options })
    };

    let spec = TestSpec {
        cases: vec![create_case("Run 1", 1)?, create_case("Run 2", 3)?],
        relation: CaseRelation::Serial,
        stream_processor_factory: Rc::new(EncoderFactory),
    };

    exec.run_singlethreaded(spec.run())?;
    Ok(())
}
