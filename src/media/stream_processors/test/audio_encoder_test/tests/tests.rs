// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use audio_encoder_test_lib::pcm_audio::*;
use audio_encoder_test_lib::test_suite::*;

use encoder_test_data::*;
use fidl_fuchsia_media::*;
use fidl_fuchsia_sysmem2 as fsysmem2;
use fuchsia_async as fasync;
use std::rc::Rc;
use stream_processor_encoder_factory::EncoderFactory;
use stream_processor_test::*;

// INSTRUCTIONS FOR ADDING HASH TESTS
//
// 1. If adding a new pcm input configuration, write the saw wave to file and check it in in the
//    `test_data` directory. It should only be a few thousand PCM frames.
// 2. Set the `output_file` field to write the encoded output into
//    `/tmp/something ` so you can copy it to host with `ffx component copy`.
// 3. Create an encoded stream with the same settings using another encoder (for sbc, use sbcenc or
//    ffmpeg; for aac use faac) on the reference saw wave.
// 4. Verify the output
//      a. If the codec should produce the exact same bytes for the same settings, `diff` the two
//         files.
//      b. If the codec is permitted to produce different encoded bytes for the same settings, do a
//         similarity check:
//           b1. Decode both the reference and our encoded stream (sbcdec, faad, etc)
//           b2. Import both tracks into Audacity
//           b3. Apply Effect > Invert to one track
//           b4. Select both tracks and Tracks > Mix > Mix and Render to New Track
//           b5. On the resulting track use Effect > Amplify and observe the new peak amplitude
// 5. If all looks good, commit the hash.

#[fuchsia::test]
fn sbc_test_suite() -> Result<()> {
    with_large_stack(|| {
        let sub_bands = SbcSubBands::SubBands4;
        let block_count = SbcBlockCount::BlockCount8;

        let sbc_tests = AudioEncoderTestCase {
            input_framelength: (sub_bands.into_primitive() * block_count.into_primitive()) as usize,
            input_frames_per_second: 44100,
            settings: EncoderSettings::Sbc(SbcEncoderSettings {
                allocation: SbcAllocation::AllocLoudness,
                sub_bands,
                block_count,
                channel_mode: SbcChannelMode::Mono,
                // Recommended bit pool value for these parameters, from SBC spec.
                bit_pool: 59,
            }),
            channel_count: 1,
            output_tests: vec![AudioEncoderOutputTest::saw_wave_test(
                44100,
                OutputSize::PacketCount(94),
                vec![ExpectedDigest::new(
                    "Sbc: 44.1kHz/Loudness/Mono/bitpool 56/blocks 8/subbands 4",
                    "5c65a88bda3f132538966d87df34aa8675f85c9892b7f9f5571f76f3c7813562",
                )],
            )],
        };

        fasync::TestExecutor::new().run_singlethreaded(sbc_tests.run())
    })
}

#[fuchsia::test]
fn msbc_test_suite() -> Result<()> {
    with_large_stack(|| {
        let sub_bands = 8;
        let block_count = 15;

        let msbc_tests = AudioEncoderTestCase {
            input_framelength: (sub_bands * block_count) as usize,
            input_frames_per_second: 16000,
            settings: EncoderSettings::Msbc(MSbcEncoderSettings::default()),
            channel_count: 1,
            output_tests: vec![AudioEncoderOutputTest::saw_wave_test(
                16000,
                OutputSize::PacketCount(25),
                vec![ExpectedDigest::new(
                    "Sbc: 16kHz/Loudness/Mono/bitpool 26/blocks 15/subbands 8",
                    "bf96bd3b827a1317d8e707d3791d1a7d4b7f6a0d7f63f89831c5de1f1828b5ab",
                )],
            )],
        };

        fasync::TestExecutor::new().run_singlethreaded(msbc_tests.run())
    })
}

#[fuchsia::test]
fn aac_test_suite() -> Result<()> {
    with_large_stack(|| {
        let aac_raw_tests = AudioEncoderTestCase {
            input_framelength: 1024,
            input_frames_per_second: 44100,
            settings: EncoderSettings::Aac(AacEncoderSettings {
                transport: AacTransport::Raw(AacTransportRaw {}),
                channel_mode: AacChannelMode::Mono,
                bit_rate: AacBitRate::Variable(AacVariableBitRate::V5),
                aot: AacAudioObjectType::Mpeg2AacLc,
            }),
            channel_count: 1,
            output_tests: vec![AudioEncoderOutputTest::saw_wave_test(
                44100,
                OutputSize::PacketCount(5),
                vec![
                    ExpectedDigest::new(
                        // newer encoder
                        "Aac: 44.1kHz/Mono/V5/Mpeg2 LC/Raw",
                        "555f107d1c5be5e754ea9a4475478b6374fa7311192583f1e48d7b181c2fe5a3",
                    ),
                    ExpectedDigest::new(
                        // older encoder
                        "Aac: 44.1kHz/Mono/V5/Mpeg2 LC/Raw Arm",
                        "11fe39d40b09c3158172adf86ecb715d98f5e0ca9d5b541629ac80922f79fc1c",
                    ),
                    ExpectedDigest::new(
                        // older encoder
                        "Aac: 44.1kHz/Mono/V5/Mpeg2 LC/Raw x64",
                        "5be551b15b856508a186daa008e06b5ea2d7c2b18ae7977c5037ddee92d4ef9b",
                    ),
                ],
            )],
        };

        fasync::TestExecutor::new().run_singlethreaded(aac_raw_tests.run())?;

        // Test the MPEG4 AAC_LC variant. This affects encoder behavior but in this test case the
        // resulting bit streams are identical.
        let aac_raw_tests = AudioEncoderTestCase {
            input_framelength: 1024,
            input_frames_per_second: 44100,
            settings: EncoderSettings::Aac(AacEncoderSettings {
                transport: AacTransport::Raw(AacTransportRaw {}),
                channel_mode: AacChannelMode::Mono,
                bit_rate: AacBitRate::Variable(AacVariableBitRate::V5),
                aot: AacAudioObjectType::Mpeg4AacLc,
            }),
            channel_count: 1,
            output_tests: vec![AudioEncoderOutputTest::saw_wave_test(
                44100,
                OutputSize::PacketCount(5),
                vec![
                    ExpectedDigest::new(
                        // newer encoder
                        "Aac: 44.1kHz/Mono/V5/Mpeg2 LC/Raw",
                        "555f107d1c5be5e754ea9a4475478b6374fa7311192583f1e48d7b181c2fe5a3",
                    ),
                    ExpectedDigest::new(
                        // older encoder
                        "Aac: 44.1kHz/Mono/V5/Mpeg2 LC/Raw Arm",
                        "11fe39d40b09c3158172adf86ecb715d98f5e0ca9d5b541629ac80922f79fc1c",
                    ),
                    ExpectedDigest::new(
                        // older encoder
                        "Aac: 44.1kHz/Mono/V5/Mpeg2 LC/Raw x64",
                        "5be551b15b856508a186daa008e06b5ea2d7c2b18ae7977c5037ddee92d4ef9b",
                    ),
                ],
            )],
        };

        fasync::TestExecutor::new().run_singlethreaded(aac_raw_tests.run())
    })
}

#[fuchsia::test]
fn aac_adts_test_suite() -> Result<()> {
    with_large_stack(|| {
        let aac_adts_tests = AudioEncoderTestCase {
            input_framelength: 1024,
            input_frames_per_second: 44100,
            settings: EncoderSettings::Aac(AacEncoderSettings {
                transport: AacTransport::Adts(AacTransportAdts {}),
                channel_mode: AacChannelMode::Mono,
                bit_rate: AacBitRate::Variable(AacVariableBitRate::V5),
                aot: AacAudioObjectType::Mpeg2AacLc,
            }),
            channel_count: 1,
            output_tests: vec![AudioEncoderOutputTest::saw_wave_test(
                44100,
                OutputSize::PacketCount(5),
                vec![
                    ExpectedDigest::new(
                        // newer encoder
                        "Aac: 44.1kHz/Mono/V5/Mpeg2 LC/Adts",
                        "105cf69e9e063d47fd61f143e107828f6b8285b67b9bb248f9a21c55ac734924",
                    ),
                    ExpectedDigest::new(
                        // older encoder
                        "Aac: 44.1kHz/Mono/V5/Mpeg2 LC/Adts Arm",
                        "c9d1ebb5844b9d90c09b0a26db14ddcf4189e77087efc064061f1c88df51e296",
                    ),
                    ExpectedDigest::new(
                        // older encoder
                        "Aac: 44.1kHz/Mono/V5/Mpeg2 LC/Adts x64",
                        "e88afc9130dc3cf429719f4e66fa7c60a17161c5ac30b37c527ab98e83f30750",
                    ),
                ],
            )],
        };

        fasync::TestExecutor::new().run_singlethreaded(aac_adts_tests.run())
    })
}

#[fuchsia::test]
fn aac_latm_test_suite() -> Result<()> {
    with_large_stack(|| {
        let aac_latm_with_mux_config_test = AudioEncoderTestCase {
            input_framelength: 1024,
            input_frames_per_second: 44100,
            settings: EncoderSettings::Aac(AacEncoderSettings {
                transport: AacTransport::Latm(AacTransportLatm { mux_config_present: true }),
                channel_mode: AacChannelMode::Mono,
                bit_rate: AacBitRate::Variable(AacVariableBitRate::V5),
                aot: AacAudioObjectType::Mpeg2AacLc,
            }),
            channel_count: 1,
            output_tests: vec![AudioEncoderOutputTest::saw_wave_test(
                44100,
                OutputSize::PacketCount(5),
                vec![
                    ExpectedDigest::new(
                        // newer encoder
                        "Aac: 44.1kHz/Mono/V5/Mpeg2 LC/Latm/MuxConfig",
                        "64e1ad758406de6058f50ced29294eb403705134972c003d1a2dbc7eb14946cc",
                    ),
                    ExpectedDigest::new(
                        // older encoder
                        "Aac: 44.1kHz/Mono/V5/Mpeg2 LC/Latm/MuxConfig Arm",
                        "85ce565087981c36e47c873be7df2d57d3c0e8273e6641477e1b6d20c41c29b4",
                    ),
                    ExpectedDigest::new(
                        // older encoder
                        "Aac: 44.1kHz/Mono/V5/Mpeg2 LC/Latm/MuxConfig x64",
                        "6f2eadfe6dd88b189a38b00b9711160fea4b2d8a6acc24ea9008708d2a355735",
                    ),
                ],
            )],
        };

        fasync::TestExecutor::new().run_singlethreaded(aac_latm_with_mux_config_test.run())?;

        let aac_latm_without_mux_config_test = AudioEncoderTestCase {
            input_framelength: 1024,
            input_frames_per_second: 44100,
            settings: EncoderSettings::Aac(AacEncoderSettings {
                transport: AacTransport::Latm(AacTransportLatm { mux_config_present: false }),
                channel_mode: AacChannelMode::Mono,
                bit_rate: AacBitRate::Variable(AacVariableBitRate::V5),
                aot: AacAudioObjectType::Mpeg2AacLc,
            }),
            channel_count: 1,
            output_tests: vec![AudioEncoderOutputTest::saw_wave_test(
                44100,
                OutputSize::PacketCount(5),
                vec![
                    ExpectedDigest::new(
                        // newer encoder
                        "Aac: 44.1kHz/Mono/V5/Mpeg2 LC/Latm/NoMuxConfig",
                        "0f98c415fe2ec51387c0c268168962b81eaf237afb29df12fc0bfafbde40c7d5",
                    ),
                    ExpectedDigest::new(
                        // older encoder
                        "Aac: 44.1kHz/Mono/V5/Mpeg2 LC/Latm/NoMuxConfig Arm",
                        "09f7e4a6c55873f21772a8ef6d28d96eab287a93290d6d3cd612a11bc2abe6e3",
                    ),
                    ExpectedDigest::new(
                        // older encoder
                        "Aac: 44.1kHz/Mono/V5/Mpeg2 LC/Latm/NoMuxConfig x64",
                        "a139f287f77c06e3f0a318a8712ea2cabf93c94b7b7106825747f3dd752fc7c0",
                    ),
                ],
            )],
        };

        fasync::TestExecutor::new().run_singlethreaded(aac_latm_without_mux_config_test.run())
    })
}

#[fuchsia::test]
fn cvsd_simple_test_suite() -> Result<()> {
    with_large_stack(|| {
        let cvsd_tests = AudioEncoderTestCase {
            input_framelength: 8,
            input_frames_per_second: 64000,
            settings: EncoderSettings::Cvsd(CvsdEncoderSettings::default()),
            channel_count: 1,
            output_tests: vec![AudioEncoderOutputTest {
                output_file: None,
                input_audio: PcmAudio::create_from_data(
                    PcmFormat {
                        pcm_mode: AudioPcmMode::Linear,
                        bits_per_sample: 16,
                        frames_per_second: 64000,
                        channel_map: vec![AudioChannelId::Cf],
                    },
                    vec![1, 2, 3, 4, 5, 6, 7, 8].as_slice(),
                ),
                // 16-1 compression means 8 samples of 2-byte PCM input will be converted to 1 byte of output.
                expected_output_size: OutputSize::RawBytesCount(1),
                expected_digests: Some(vec![ExpectedDigest::new_from_raw(
                    "Simple test case",
                    vec![0b01010101],
                )]),
            }],
        };
        fasync::TestExecutor::new().run_singlethreaded(cvsd_tests.run())
    })
}

#[test]
fn lc3_simple_test_suite() -> Result<()> {
    const BITS_PER_SAMPLE: u32 = 16;
    const FRAMES_PER_SECOND: u32 = 32000;
    const FRAME_DURATION: Lc3FrameDuration = Lc3FrameDuration::D7P5Ms;
    const FRAME_SIZE: u32 = 240;
    const NBYTES: u16 = 58;
    with_large_stack(|| {
        let lc3_tests = AudioEncoderTestCase {
            input_framelength: (FRAME_SIZE).try_into().unwrap(),
            // We use 32kHz for timestamp testing instead of 44.1kHz that's used
            // for other codecs.
            // For LC3, When the sampling frequency of the input signal is 44.1 kHz, the
            // same frame length is used as for 48 kHz, resulting in the slightly longer
            // actual frame duration of 10.884 ms for the 10 ms frame interval and of 8.16 ms
            // for the 7.5 ms frame interval, which makes it hard to test for timestamp
            // validations.
            // See LC3 specification v1.0 section 2.1 for more details.
            input_frames_per_second: 32000,
            settings: EncoderSettings::Lc3(Lc3EncoderSettings {
                nbytes: Some(NBYTES),
                frame_duration: Some(FRAME_DURATION),
                ..Lc3EncoderSettings::default()
            }),
            channel_count: 1,
            output_tests: vec![AudioEncoderOutputTest {
                output_file: None,
                input_audio: PcmAudio::create_from_data(
                    PcmFormat {
                        pcm_mode: AudioPcmMode::Linear,
                        bits_per_sample: BITS_PER_SAMPLE,
                        frames_per_second: FRAMES_PER_SECOND,
                        channel_map: vec![AudioChannelId::Lf],
                    },
                    &INPUT_TEST_S16LE32000MONO,
                ),
                // Each frame of input is to be converted to 58 bytes of output.
                // There are 48 frames worth of input data in total.
                expected_output_size: OutputSize::RawBytesCount((NBYTES * 48) as usize),
                expected_digests: None,
            }],
        };
        fasync::TestExecutor::new().run_singlethreaded(lc3_tests.run())
    })
}

fn run_aac_stream_switching_test(close_on_stop: bool) -> Result<()> {
    with_large_stack(move || {
        let settings = EncoderSettings::Aac(AacEncoderSettings {
            transport: AacTransport::Raw(AacTransportRaw {}),
            channel_mode: AacChannelMode::Mono,
            bit_rate: AacBitRate::Variable(AacVariableBitRate::V5),
            aot: AacAudioObjectType::Mpeg2AacLc,
        });

        let pcm_format = PcmFormat {
            pcm_mode: AudioPcmMode::Linear,
            bits_per_sample: 16,
            frames_per_second: 44100,
            channel_map: vec![AudioChannelId::Cf],
        };

        // Create input PCM saw wave.
        let pcm_audio1 = PcmAudio::create_saw_wave(pcm_format.clone(), TEST_PCM_FRAME_COUNT);
        let pcm_audio2 = PcmAudio::create_saw_wave(pcm_format.clone(), TEST_PCM_FRAME_COUNT);

        let stream1 = Rc::new(PcmAudioStream {
            pcm_audio: pcm_audio1,
            encoder_settings: settings.clone(),
            frames_per_packet: (0..).map(move |_| 1024),
            timebase: None,
        });

        let stream2 = Rc::new(PcmAudioStream {
            pcm_audio: pcm_audio2,
            encoder_settings: settings.clone(),
            frames_per_packet: (0..).map(move |_| 1024),
            timebase: None,
        });

        let mut executor = fasync::TestExecutor::new();
        executor.run_singlethreaded(async {
            let stream_processor =
                EncoderFactory.connect_to_stream_processor(stream1.as_ref(), 1).await?;
            let mut stream_runner = StreamRunner::new(stream_processor);

            // Run stream 1, but stop after receiving 2 output packets.
            let output1 = stream_runner
                .run_stream(
                    stream1.clone(),
                    StreamOptions {
                        queue_format_details: false,
                        stop_after_n_output: Some(2),
                        close_on_stop,
                        ..Default::default()
                    },
                )
                .await?;

            // Verify we got 2 output packets for stream 1.
            let packets1: Vec<_> = output1
                .iter()
                .filter_map(|o| match o {
                    Output::Packet(p) => Some(p),
                    _ => None,
                })
                .collect();
            assert_eq!(packets1.len(), 2);

            // Run stream 2 to completion.
            let output2 = stream_runner
                .run_stream(
                    stream2.clone(),
                    StreamOptions {
                        queue_format_details: false,
                        close_on_stop,
                        ..Default::default()
                    },
                )
                .await?;

            // Verify stream 2 output has 5 packets and correct digest.
            let packets2: Vec<_> = output2
                .iter()
                .filter_map(|o| match o {
                    Output::Packet(p) => Some(p),
                    _ => None,
                })
                .collect();
            assert_eq!(packets2.len(), 5);

            let validator = BytesValidator {
                output_file: None,
                expected_digests: vec![
                    ExpectedDigest::new(
                        // newer encoder
                        "Aac: 44.1kHz/Mono/V5/Mpeg2 LC/Raw",
                        "555f107d1c5be5e754ea9a4475478b6374fa7311192583f1e48d7b181c2fe5a3",
                    ),
                    ExpectedDigest::new(
                        // older encoder
                        "Aac: 44.1kHz/Mono/V5/Mpeg2 LC/Raw Arm",
                        "11fe39d40b09c3158172adf86ecb715d98f5e0ca9d5b541629ac80922f79fc1c",
                    ),
                    ExpectedDigest::new(
                        // older encoder
                        "Aac: 44.1kHz/Mono/V5/Mpeg2 LC/Raw x64",
                        "5be551b15b856508a186daa008e06b5ea2d7c2b18ae7977c5037ddee92d4ef9b",
                    ),
                ],
            };
            validator.validate(&output2).await?;

            Ok(())
        })
    })
}

#[fuchsia::test]
fn aac_stream_switching_with_close_test() -> Result<()> {
    run_aac_stream_switching_test(true)
}

#[fuchsia::test]
fn aac_stream_switching_without_close_test() -> Result<()> {
    run_aac_stream_switching_test(false)
}

fn run_lc3_stream_switching_test(close_on_stop: bool) -> Result<()> {
    const BITS_PER_SAMPLE: u32 = 16;
    const FRAMES_PER_SECOND: u32 = 32000;
    const FRAME_DURATION: Lc3FrameDuration = Lc3FrameDuration::D7P5Ms;
    const FRAME_SIZE: usize = 240;
    const NBYTES: u16 = 58;

    with_large_stack(move || {
        let settings = EncoderSettings::Lc3(Lc3EncoderSettings {
            nbytes: Some(NBYTES),
            frame_duration: Some(FRAME_DURATION),
            ..Lc3EncoderSettings::default()
        });

        let pcm_format = PcmFormat {
            pcm_mode: AudioPcmMode::Linear,
            bits_per_sample: BITS_PER_SAMPLE,
            frames_per_second: FRAMES_PER_SECOND,
            channel_map: vec![AudioChannelId::Lf],
        };

        let pcm_audio1 = PcmAudio::create_from_data(pcm_format.clone(), &INPUT_TEST_S16LE32000MONO);
        let pcm_audio2 = PcmAudio::create_from_data(pcm_format.clone(), &INPUT_TEST_S16LE32000MONO);

        let stream1 = Rc::new(PcmAudioStream {
            pcm_audio: pcm_audio1,
            encoder_settings: settings.clone(),
            frames_per_packet: (0..).map(move |_| FRAME_SIZE),
            timebase: None,
        });

        let stream2 = Rc::new(PcmAudioStream {
            pcm_audio: pcm_audio2,
            encoder_settings: settings.clone(),
            frames_per_packet: (0..).map(move |_| FRAME_SIZE),
            timebase: None,
        });

        let mut executor = fasync::TestExecutor::new();
        executor.run_singlethreaded(async {
            let stream_processor =
                EncoderFactory.connect_to_stream_processor(stream1.as_ref(), 1).await?;
            let mut stream_runner = StreamRunner::new(stream_processor);

            // Run stream 1, but stop after receiving 5 output packets.
            let output1 = stream_runner
                .run_stream(
                    stream1.clone(),
                    StreamOptions {
                        queue_format_details: false,
                        stop_after_n_output: Some(5),
                        close_on_stop,
                        ..Default::default()
                    },
                )
                .await?;

            // Verify we got 5 output packets for stream 1.
            let packets1: Vec<_> = output1
                .iter()
                .filter_map(|o| match o {
                    Output::Packet(p) => Some(p),
                    _ => None,
                })
                .collect();
            assert_eq!(packets1.len(), 5);

            // Run stream 2 to completion.
            let output2 = stream_runner
                .run_stream(
                    stream2.clone(),
                    StreamOptions {
                        queue_format_details: false,
                        close_on_stop,
                        ..Default::default()
                    },
                )
                .await?;

            // Verify stream 2 output has 48 packets.
            let packets2: Vec<_> = output2
                .iter()
                .filter_map(|o| match o {
                    Output::Packet(p) => Some(p),
                    _ => None,
                })
                .collect();
            assert_eq!(packets2.len(), 48);

            let validator =
                OutputDataSizeValidator { expected_output_data_size: (NBYTES * 48) as usize };
            validator.validate(&output2).await?;

            Ok(())
        })
    })
}

#[fuchsia::test]
fn lc3_stream_switching_with_close_test() -> Result<()> {
    run_lc3_stream_switching_test(true)
}

#[fuchsia::test]
fn lc3_stream_switching_without_close_test() -> Result<()> {
    run_lc3_stream_switching_test(false)
}

fn run_sbc_stream_switching_test(close_on_stop: bool) -> Result<()> {
    with_large_stack(move || {
        let sub_bands = SbcSubBands::SubBands4;
        let block_count = SbcBlockCount::BlockCount8;
        let settings = EncoderSettings::Sbc(SbcEncoderSettings {
            allocation: SbcAllocation::AllocLoudness,
            sub_bands,
            block_count,
            channel_mode: SbcChannelMode::Mono,
            bit_pool: 59,
        });

        let pcm_format = PcmFormat {
            pcm_mode: AudioPcmMode::Linear,
            bits_per_sample: 16,
            frames_per_second: 44100,
            channel_map: vec![AudioChannelId::Cf],
        };

        // Create input PCM saw wave.
        let pcm_audio1 = PcmAudio::create_saw_wave(pcm_format.clone(), TEST_PCM_FRAME_COUNT);
        let pcm_audio2 = PcmAudio::create_saw_wave(pcm_format.clone(), TEST_PCM_FRAME_COUNT);

        let input_framelength =
            (sub_bands.into_primitive() * block_count.into_primitive()) as usize;

        let stream1 = Rc::new(PcmAudioStream {
            pcm_audio: pcm_audio1,
            encoder_settings: settings.clone(),
            frames_per_packet: (0..).map(move |_| input_framelength),
            timebase: None,
        });

        let stream2 = Rc::new(PcmAudioStream {
            pcm_audio: pcm_audio2,
            encoder_settings: settings.clone(),
            frames_per_packet: (0..).map(move |_| input_framelength),
            timebase: None,
        });

        let mut executor = fasync::TestExecutor::new();
        executor.run_singlethreaded(async {
            let stream_processor =
                EncoderFactory.connect_to_stream_processor(stream1.as_ref(), 1).await?;
            let mut stream_runner = StreamRunner::new(stream_processor);

            // Run stream 1, but stop after receiving 2 output packets.
            let output1 = stream_runner
                .run_stream(
                    stream1.clone(),
                    StreamOptions {
                        queue_format_details: false,
                        stop_after_n_output: Some(2),
                        close_on_stop,
                        ..Default::default()
                    },
                )
                .await?;

            // Verify we got 2 output packets for stream 1.
            let packets1: Vec<_> = output1
                .iter()
                .filter_map(|o| match o {
                    Output::Packet(p) => Some(p),
                    _ => None,
                })
                .collect();
            assert_eq!(packets1.len(), 2);

            // Run stream 2 to completion.
            let output2 = stream_runner
                .run_stream(
                    stream2.clone(),
                    StreamOptions {
                        queue_format_details: false,
                        close_on_stop,
                        ..Default::default()
                    },
                )
                .await?;

            // Verify stream 2 output has 94 packets.
            let packets2: Vec<_> = output2
                .iter()
                .filter_map(|o| match o {
                    Output::Packet(p) => Some(p),
                    _ => None,
                })
                .collect();
            assert_eq!(packets2.len(), 94);

            let validator = BytesValidator {
                output_file: None,
                expected_digests: vec![ExpectedDigest::new(
                    "Sbc: 44.1kHz/Loudness/Mono/bitpool 56/blocks 8/subbands 4",
                    "5c65a88bda3f132538966d87df34aa8675f85c9892b7f9f5571f76f3c7813562",
                )],
            };
            validator.validate(&output2).await?;

            Ok(())
        })
    })
}

#[fuchsia::test]
fn sbc_stream_switching_with_close_test() -> Result<()> {
    run_sbc_stream_switching_test(true)
}

#[fuchsia::test]
fn sbc_stream_switching_without_close_test() -> Result<()> {
    run_sbc_stream_switching_test(false)
}

fn run_cvsd_stream_switching_test(close_on_stop: bool) -> Result<()> {
    with_large_stack(move || {
        let settings = EncoderSettings::Cvsd(CvsdEncoderSettings::default());

        let pcm_format = PcmFormat {
            pcm_mode: AudioPcmMode::Linear,
            bits_per_sample: 16,
            frames_per_second: 64000,
            channel_map: vec![AudioChannelId::Cf],
        };

        // Create input PCM saw wave.
        let pcm_audio1 = PcmAudio::create_saw_wave(pcm_format.clone(), TEST_PCM_FRAME_COUNT);
        let pcm_audio2 = PcmAudio::create_saw_wave(pcm_format.clone(), TEST_PCM_FRAME_COUNT);

        let input_framelength = 8; // 8 frames = 16 bytes = 1 CVSD byte

        let stream1 = Rc::new(PcmAudioStream {
            pcm_audio: pcm_audio1,
            encoder_settings: settings.clone(),
            frames_per_packet: (0..).map(move |_| input_framelength),
            timebase: None,
        });

        let stream2 = Rc::new(PcmAudioStream {
            pcm_audio: pcm_audio2,
            encoder_settings: settings.clone(),
            frames_per_packet: (0..).map(move |_| input_framelength),
            timebase: None,
        });

        // We want to force the output buffer size to be 60 bytes.
        // This will result in 60-byte packets.
        let output_constraints = Some(fsysmem2::BufferCollectionConstraints {
            buffer_memory_constraints: Some(fsysmem2::BufferMemoryConstraints {
                min_size_bytes: Some(60),
                max_size_bytes: Some(60),
                ..buffer_memory_constraints_default()
            }),
            ..buffer_collection_constraints_default()
        });

        let mut executor = fasync::TestExecutor::new();
        executor.run_singlethreaded(async {
            let stream_processor =
                EncoderFactory.connect_to_stream_processor(stream1.as_ref(), 1).await?;
            let mut stream_runner = StreamRunner::new(stream_processor);

            // Run stream 1, but stop after receiving 2 output packets.
            let output1 = stream_runner
                .run_stream(
                    stream1.clone(),
                    StreamOptions {
                        queue_format_details: false,
                        stop_after_n_output: Some(2),
                        close_on_stop,
                        output_buffer_collection_constraints: output_constraints.clone(),
                        ..Default::default()
                    },
                )
                .await?;

            // Verify we got 2 output packets for stream 1.
            let packets1: Vec<_> = output1
                .iter()
                .filter_map(|o| match o {
                    Output::Packet(p) => Some(p),
                    _ => None,
                })
                .collect();
            assert_eq!(packets1.len(), 2);
            for p in &packets1 {
                assert_eq!(p.data.len(), 60);
            }

            // Run stream 2 to completion.
            let output2 = stream_runner
                .run_stream(
                    stream2.clone(),
                    StreamOptions {
                        queue_format_details: false,
                        close_on_stop,
                        output_buffer_collection_constraints: output_constraints,
                        ..Default::default()
                    },
                )
                .await?;

            // Verify stream 2 output.
            // 3000 frames total.
            // 8 frames = 16 bytes = 1 output byte.
            // 3000 / 8 = 375 output bytes.
            // With 60-byte packets, 375 / 60 = 6 packets of 60 bytes, and 1 packet of 15 bytes.
            // Total 7 packets.
            let packets2: Vec<_> = output2
                .iter()
                .filter_map(|o| match o {
                    Output::Packet(p) => Some(p),
                    _ => None,
                })
                .collect();
            assert_eq!(packets2.len(), 7);

            // Check sizes of packets.
            for i in 0..6 {
                assert_eq!(packets2[i].data.len(), 60);
            }
            assert_eq!(packets2[6].data.len(), 15);

            let validator = OutputDataSizeValidator { expected_output_data_size: 375 };
            validator.validate(&output2).await?;

            Ok(())
        })
    })
}

#[fuchsia::test]
fn cvsd_stream_switching_with_close_test() -> Result<()> {
    run_cvsd_stream_switching_test(true)
}

#[fuchsia::test]
fn cvsd_stream_switching_without_close_test() -> Result<()> {
    run_cvsd_stream_switching_test(false)
}
