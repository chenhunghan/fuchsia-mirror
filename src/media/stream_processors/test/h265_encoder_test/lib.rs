// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod h265;
mod test_suite;

use crate::test_suite::*;
use fidl_fuchsia_images2 as images2;
use fidl_fuchsia_math::{RectU, SizeU};
use fidl_fuchsia_media::*;
use fuchsia_async as fasync;
use std::rc::Rc;
use stream_processor_test::*;

// Instructions for capturing output of encoder:
// 1. Set the `output_file` field to write the encoded output into "/tmp/".
// 2. Add exec.run_singlethreaded(future::pending()) at the end of the test so it doesn't exit.
//    This is so the tmp file doesn't get cleaned up.
// 3. Use `ffx component copy` to copy the file to the host.

fn h265_stream_output_generated() -> Result<()> {
    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 240;
    let mut exec = fasync::TestExecutor::new();

    let test_case = H265EncoderTestCase {
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
            EncoderSettings::Hevc(HevcEncoderSettings {
                bit_rate: Some(1000000),
                frame_rate: Some(30),
                gop_size: Some(2),
                ..Default::default()
            })
        }),
        expected_key_frames: Some(3),
        output_file: None,
    };
    exec.run_singlethreaded(test_case.run())?;
    Ok(())
}

fn test_serial_h265_encoder_on_same_codec() -> Result<()> {
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
        EncoderSettings::Hevc(HevcEncoderSettings {
            bit_rate: Some(1000000),
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
            "video/h265",
        )?);

        let format_constraints = sysmem2::ImageFormatConstraints {
            pixel_format: Some(*input_format.pixel_format.as_ref().unwrap()),
            color_spaces: Some(vec![images2::ColorSpace::Rec601Pal]),
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

        let validators: Vec<Rc<dyn OutputValidator>> = vec![
            Rc::new(H265NalValidator {
                expected_frames: Some(6),
                expected_key_frames: Some(3),
                output_file: None,
            }),
            Rc::new(TimestampValidator { generator: stream.timestamp_generator() }),
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

#[fuchsia::test]
fn run_all_tests() -> Result<()> {
    // TODO(https://fxbug.dev/564808353): We shouldn't need to run these sequentially, but when we
    // have each one as its own test, we get failures when running on amlogic_hevc_encoder. In
    // contrast the amlogic_h264_encoder passes its h264_encoder_test with its analogous tests
    // split as separate tests.
    h265_stream_output_generated()?;
    test_serial_h265_encoder_on_same_codec()?;
    Ok(())
}
