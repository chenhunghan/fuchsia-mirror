// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, format_err};
use fidl::endpoints::{Proxy, ServerEnd};
use fidl_fuchsia_hardware_audio::*;

use futures::future::{self, Either};
use futures::{Future, FutureExt, TryFutureExt};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct DigitalAudioInterface {
    /// The path used to connect to the device.
    path: PathBuf,
    /// The proxy to the device, if connected.
    proxy: Option<DaiProxy>,
}

impl DigitalAudioInterface {
    /// A new interface that will connect to the device at `path`.
    /// The interface is unconnected when created.
    pub fn new(path: &Path) -> Self {
        Self { path: path.to_path_buf(), proxy: None }
    }

    /// Build a DAI from a proxy.  Path will be empty.
    pub(crate) fn from_proxy(proxy: DaiProxy) -> Self {
        Self { path: PathBuf::new(), proxy: Some(proxy) }
    }

    /// Connect to the DigitalAudioInterface.
    pub fn connect(&mut self) -> Result<(), Error> {
        if let Some(proxy) = &self.proxy {
            if !proxy.is_closed() {
                return Ok(());
            }
        }
        let (dai_connect_proxy, dai_connect_server) =
            fidl::endpoints::create_proxy::<DaiConnectorMarker>();
        let path = self.path.to_str().ok_or_else(|| format_err!("invalid DAI path"))?;
        fdio::service_connect(path, dai_connect_server.into_channel())?;

        let (ours, theirs) = fidl::endpoints::create_proxy::<DaiMarker>();
        dai_connect_proxy.connect(theirs)?;

        self.proxy = Some(ours);
        Ok(())
    }

    fn get_proxy(&self) -> Result<&DaiProxy, Error> {
        self.proxy.as_ref().ok_or_else(|| format_err!("Proxy not connected"))
    }

    /// Get the properties of the DAI.
    /// Will attempt to connect to the DAI if not connected.
    pub fn properties(&self) -> impl Future<Output = Result<DaiProperties, Error>> {
        match self.get_proxy() {
            Err(e) => Either::Left(future::ready(Err(e))),
            Ok(proxy) => Either::Right(proxy.clone().get_properties().err_into()),
        }
    }

    pub fn dai_formats(
        &self,
    ) -> impl Future<Output = Result<Vec<DaiSupportedFormats>, Error>> + use<> {
        let proxy = match self.get_proxy() {
            Err(e) => return Either::Left(future::ready(Err(e))),
            Ok(proxy) => proxy,
        };
        Either::Right(proxy.clone().get_dai_formats().map(|o| match o {
            Err(e) => Err(e.into()),
            Ok(Err(e)) => Err(zx::Status::err_from_raw(e).into()),
            Ok(Ok(o)) => Ok(o),
        }))
    }

    pub fn ring_buffer_formats(
        &self,
    ) -> impl Future<Output = Result<Vec<SupportedFormats>, Error>> {
        let proxy = match self.get_proxy() {
            Err(e) => return Either::Left(future::ready(Err(e))),
            Ok(proxy) => proxy,
        };
        Either::Right(proxy.clone().get_ring_buffer_formats().map(|o| match o {
            Err(e) => Err(e.into()),
            Ok(Err(e)) => Err(zx::Status::err_from_raw(e).into()),
            Ok(Ok(o)) => Ok(o),
        }))
    }

    pub fn create_ring_buffer(
        &self,
        dai_format: DaiFormat,
        buffer_format: Format,
        ring_buffer_client: ServerEnd<RingBufferMarker>,
    ) -> Result<(), Error> {
        let proxy = self.get_proxy()?;
        proxy
            .create_ring_buffer(&dai_format, &buffer_format, ring_buffer_client)
            .map_err(Into::into)
    }
}

pub(crate) fn ensure_pcm_format_is_supported(
    ring_buffer_formats: &[SupportedFormats],
    pcm_format: &PcmFormat,
) -> Result<(), Error> {
    for format in ring_buffer_formats {
        if let SupportedFormats {
            pcm_supported_formats:
                Some(PcmSupportedFormats {
                    channel_sets: Some(channel_sets),
                    sample_formats: Some(sample_formats),
                    bytes_per_sample: Some(bytes_per_sample),
                    valid_bits_per_sample: Some(valid_bits_per_sample),
                    frame_rates: Some(frame_rates),
                    ..
                }),
            ..
        } = format
        {
            if channel_sets[0].attributes.as_ref().unwrap().len()
                == pcm_format.number_of_channels as usize
                && sample_formats.contains(&pcm_format.sample_format)
                && bytes_per_sample.contains(&pcm_format.bytes_per_sample)
                && valid_bits_per_sample.contains(&pcm_format.valid_bits_per_sample)
                && frame_rates.contains(&pcm_format.frame_rate)
            {
                return Ok(());
            }
        }
    }
    Err(format_err!("DAI does not support PCM format: {:?}", pcm_format))
}

fn dai_sample_format_match(
    dai_sample_formats: &[DaiSampleFormat],
    pcm_sample_format: SampleFormat,
) -> Option<DaiSampleFormat> {
    let matching = match pcm_sample_format {
        SampleFormat::PcmSigned => DaiSampleFormat::PcmSigned,
        SampleFormat::PcmUnsigned => DaiSampleFormat::PcmUnsigned,
        SampleFormat::PcmFloat => DaiSampleFormat::PcmFloat,
    };
    dai_sample_formats.iter().find(|format| **format == matching).cloned()
}

/// Returns the first frame format in `dai_frame_formats` that can carry PCM audio with
/// `number_of_channels` channels.
fn dai_frame_format_match(
    dai_frame_formats: &[DaiFrameFormat],
    number_of_channels: u8,
) -> Option<DaiFrameFormat> {
    dai_frame_formats
        .iter()
        .find(|format| match format {
            // NONE is for frameless formats like PDM, and can't carry PCM audio.
            DaiFrameFormat::FrameFormatStandard(DaiFrameFormatStandard::None) => false,
            // These standard formats require exactly 2 channels.
            DaiFrameFormat::FrameFormatStandard(
                DaiFrameFormatStandard::I2S
                | DaiFrameFormatStandard::StereoLeft
                | DaiFrameFormatStandard::StereoRight,
            ) => number_of_channels == 2,
            DaiFrameFormat::FrameFormatStandard(
                DaiFrameFormatStandard::Tdm1
                | DaiFrameFormatStandard::Tdm2
                | DaiFrameFormatStandard::Tdm3,
            ) => true,
            DaiFrameFormat::FrameFormatCustom(_) => true,
        })
        .copied()
}

/// Returns the bits per slot to use for a PCM format, preferring a slot that matches the PCM
/// container size (`pcm_bytes_per_sample` * 8) and otherwise using the smallest supported slot
/// that fits `pcm_valid_bits_per_sample`.
fn dai_bits_per_slot_match(
    dai_bits_per_slot: &[u8],
    pcm_bytes_per_sample: u8,
    pcm_valid_bits_per_sample: u8,
) -> Option<u8> {
    if let Some(pcm_slot_bits) = pcm_bytes_per_sample.checked_mul(8) {
        if pcm_slot_bits >= pcm_valid_bits_per_sample && dai_bits_per_slot.contains(&pcm_slot_bits)
        {
            return Some(pcm_slot_bits);
        }
    }
    dai_bits_per_slot.iter().find(|&&slot| slot >= pcm_valid_bits_per_sample).copied()
}

/// Returns a bitmask with the lowest `number_of_channels` bits set, or None if
/// `number_of_channels` is not between 1 and 64 inclusive.
fn channels_to_use_bitmask(number_of_channels: u8) -> Option<u64> {
    match number_of_channels {
        0 => None,
        64 => Some(u64::MAX),
        n if n < 64 => Some((1u64 << n) - 1),
        _ => None,
    }
}

pub(crate) fn dai_format_supporting_pcm_format(
    supported_formats: &[DaiSupportedFormats],
    pcm_format: &PcmFormat,
) -> Result<DaiFormat, Error> {
    let Some(channels_to_use_bitmask) = channels_to_use_bitmask(pcm_format.number_of_channels)
    else {
        return Err(format_err!(
            "Unsupported number of channels {} in {pcm_format:?}",
            pcm_format.number_of_channels
        ));
    };
    for dai_supported in supported_formats.iter() {
        let Some(sample_format) =
            dai_sample_format_match(&dai_supported.sample_formats, pcm_format.sample_format)
        else {
            continue;
        };
        let Some(frame_format) =
            dai_frame_format_match(&dai_supported.frame_formats, pcm_format.number_of_channels)
        else {
            continue;
        };
        let Some(bits_per_slot) = dai_bits_per_slot_match(
            &dai_supported.bits_per_slot,
            pcm_format.bytes_per_sample,
            pcm_format.valid_bits_per_sample,
        ) else {
            continue;
        };
        if dai_supported.number_of_channels.contains(&(pcm_format.number_of_channels as u32))
            && dai_supported.frame_rates.contains(&pcm_format.frame_rate)
            && dai_supported.bits_per_sample.contains(&pcm_format.valid_bits_per_sample)
        {
            return Ok(DaiFormat {
                number_of_channels: pcm_format.number_of_channels as u32,
                channels_to_use_bitmask,
                sample_format,
                frame_rate: pcm_format.frame_rate,
                frame_format,
                bits_per_slot,
                bits_per_sample: pcm_format.valid_bits_per_sample,
            });
        }
    }
    Err(format_err!(
        "DAI does not support format: {pcm_format:?} not derivable in {supported_formats:?}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_utils::PollExt;
    use fuchsia_async as fasync;
    use futures::StreamExt;
    use futures::task::Poll;
    use std::pin::pin;

    fn connected_dai() -> (DigitalAudioInterface, DaiRequestStream) {
        let (proxy, requests) = fidl::endpoints::create_proxy_and_stream::<DaiMarker>();
        let dai = DigitalAudioInterface::from_proxy(proxy);
        (dai, requests)
    }

    #[fuchsia::test]
    fn get_properties() {
        let mut exec = fasync::TestExecutor::new();
        // Unconnected DAI
        let dai = DigitalAudioInterface { path: PathBuf::new(), proxy: None };

        let _ = exec
            .run_singlethreaded(&mut dai.properties())
            .expect_err("properties of an unconnected DAI should be error");

        let (dai, mut requests) = connected_dai();

        let properties_fut = dai.properties();
        let mut properties_fut = pin!(properties_fut);

        exec.run_until_stalled(&mut properties_fut).expect_pending("should be pending");

        match exec.run_until_stalled(&mut requests.next()) {
            Poll::Ready(Some(Ok(DaiRequest::GetProperties { responder }))) => responder
                .send(&DaiProperties {
                    is_input: Some(true),
                    manufacturer: Some(String::from("Fuchsia")),
                    product_name: Some(String::from("Spinny Audio")),
                    ..Default::default()
                })
                .expect("send response okay"),
            x => panic!("Expected a ready GetProperties request, got {:?}", x),
        };

        let result = exec.run_until_stalled(&mut properties_fut).expect("response from properties");
        let properties = result.expect("ok response");
        assert_eq!(Some(true), properties.is_input);
    }

    #[fuchsia::test]
    fn dai_formats() {
        let mut exec = fasync::TestExecutor::new();
        let (dai, mut requests) = connected_dai();

        let supported_formats_fut = dai.dai_formats();
        let mut supported_formats_fut = pin!(supported_formats_fut);

        // Doesn't need to continue to be held to complete this
        drop(dai);

        exec.run_until_stalled(&mut supported_formats_fut).expect_pending("should be pending");

        match exec.run_until_stalled(&mut requests.next()) {
            Poll::Ready(Some(Ok(DaiRequest::GetDaiFormats { responder }))) => responder
                .send(Ok(&[
                    DaiSupportedFormats {
                        number_of_channels: vec![1],
                        sample_formats: vec![
                            DaiSampleFormat::PcmSigned,
                            DaiSampleFormat::PcmUnsigned,
                        ],
                        frame_formats: vec![DaiFrameFormat::FrameFormatStandard(
                            DaiFrameFormatStandard::Tdm1,
                        )],
                        frame_rates: vec![44100],
                        bits_per_slot: vec![16],
                        bits_per_sample: vec![16],
                    },
                    DaiSupportedFormats {
                        number_of_channels: vec![2],
                        sample_formats: vec![DaiSampleFormat::PcmSigned],
                        frame_formats: vec![DaiFrameFormat::FrameFormatStandard(
                            DaiFrameFormatStandard::I2S,
                        )],
                        frame_rates: vec![8000],
                        bits_per_slot: vec![32],
                        bits_per_sample: vec![32],
                    },
                ]))
                .expect("send response okay"),
            x => panic!("expected a ready GetDaiFormats, got {:?}", x),
        };

        let result = exec.run_until_stalled(&mut supported_formats_fut).expect("response");
        let formats = result.expect("ok response");
        assert_eq!(2, formats.len());
    }

    #[fuchsia::test]
    fn dai_format_supporting_pcm_format_translates_correctly() {
        let supported_formats = vec![
            DaiSupportedFormats {
                number_of_channels: vec![1],
                sample_formats: vec![DaiSampleFormat::PcmUnsigned],
                frame_formats: vec![DaiFrameFormat::FrameFormatStandard(
                    DaiFrameFormatStandard::Tdm1,
                )],
                frame_rates: vec![44100],
                bits_per_slot: vec![16],
                bits_per_sample: vec![16],
            },
            DaiSupportedFormats {
                number_of_channels: vec![1, 2],
                sample_formats: vec![DaiSampleFormat::PcmSigned],
                frame_formats: vec![DaiFrameFormat::FrameFormatStandard(
                    DaiFrameFormatStandard::I2S,
                )],
                frame_rates: vec![48000],
                bits_per_slot: vec![16, 32],
                bits_per_sample: vec![16, 24],
            },
        ];

        let mut pcm_format = PcmFormat {
            number_of_channels: 1,
            sample_format: SampleFormat::PcmUnsigned,
            bytes_per_sample: 2,
            valid_bits_per_sample: 16,
            frame_rate: 48000,
        };

        // Can't find one that is compatible initially.
        let derived = dai_format_supporting_pcm_format(&supported_formats, &pcm_format);
        assert!(derived.is_err());

        // If we switch the frame rate, then the sample format will match and we get a format
        // and we choose the frame format.
        pcm_format.frame_rate = 44100;
        let derived = dai_format_supporting_pcm_format(&supported_formats, &pcm_format);
        assert_eq!(
            DaiFormat {
                number_of_channels: 1,
                channels_to_use_bitmask: 0b1,
                sample_format: DaiSampleFormat::PcmUnsigned,
                frame_format: DaiFrameFormat::FrameFormatStandard(DaiFrameFormatStandard::Tdm1),
                frame_rate: 44100,
                bits_per_slot: 16,
                bits_per_sample: 16,
            },
            derived.expect("should find a format")
        );

        // I2S requires 2 channels, so 1 channel at 48kHz fails even though 1 channel is in
        // number_of_channels.
        pcm_format.frame_rate = 48000;
        pcm_format.sample_format = SampleFormat::PcmSigned;
        let derived = dai_format_supporting_pcm_format(&supported_formats, &pcm_format);
        assert!(derived.is_err());

        // With 2 channels, I2S is selected.
        pcm_format.number_of_channels = 2;
        let derived = dai_format_supporting_pcm_format(&supported_formats, &pcm_format);
        assert_eq!(
            DaiFormat {
                number_of_channels: 2,
                channels_to_use_bitmask: 0b11,
                sample_format: DaiSampleFormat::PcmSigned,
                frame_format: DaiFrameFormat::FrameFormatStandard(DaiFrameFormatStandard::I2S),
                frame_rate: 48000,
                bits_per_slot: 16,
                bits_per_sample: 16,
            },
            derived.expect("should find a format")
        );

        // 24-bit audio in a 32-bit container sets bits_per_slot to 32 and bits_per_sample to 24.
        pcm_format.bytes_per_sample = 4;
        pcm_format.valid_bits_per_sample = 24;
        let derived = dai_format_supporting_pcm_format(&supported_formats, &pcm_format);
        assert_eq!(
            DaiFormat {
                number_of_channels: 2,
                channels_to_use_bitmask: 0b11,
                sample_format: DaiSampleFormat::PcmSigned,
                frame_format: DaiFrameFormat::FrameFormatStandard(DaiFrameFormatStandard::I2S),
                frame_rate: 48000,
                bits_per_slot: 32,
                bits_per_sample: 24,
            },
            derived.expect("should find a format")
        );
    }

    #[fuchsia::test]
    fn dai_format_supporting_pcm_format_skips_frameless_format() {
        let supported_formats = vec![DaiSupportedFormats {
            number_of_channels: vec![1],
            sample_formats: vec![DaiSampleFormat::PcmSigned],
            frame_formats: vec![
                DaiFrameFormat::FrameFormatStandard(DaiFrameFormatStandard::None),
                DaiFrameFormat::FrameFormatStandard(DaiFrameFormatStandard::Tdm1),
            ],
            frame_rates: vec![16000],
            bits_per_slot: vec![16],
            bits_per_sample: vec![16],
        }];
        let pcm_format = PcmFormat {
            number_of_channels: 1,
            sample_format: SampleFormat::PcmSigned,
            bytes_per_sample: 2,
            valid_bits_per_sample: 16,
            frame_rate: 16000,
        };

        let derived = dai_format_supporting_pcm_format(&supported_formats, &pcm_format)
            .expect("should find a format");
        assert_eq!(
            DaiFrameFormat::FrameFormatStandard(DaiFrameFormatStandard::Tdm1),
            derived.frame_format
        );

        // With only the frameless format, there is no usable format.
        let supported_formats = vec![DaiSupportedFormats {
            frame_formats: vec![DaiFrameFormat::FrameFormatStandard(DaiFrameFormatStandard::None)],
            ..supported_formats[0].clone()
        }];
        let derived = dai_format_supporting_pcm_format(&supported_formats, &pcm_format);
        assert!(derived.is_err());
    }

    #[fuchsia::test]
    fn dai_format_supporting_pcm_format_rejects_bad_channel_counts() {
        let supported_formats = vec![DaiSupportedFormats {
            number_of_channels: vec![0, 1, 64, 65],
            sample_formats: vec![DaiSampleFormat::PcmSigned],
            frame_formats: vec![DaiFrameFormat::FrameFormatStandard(DaiFrameFormatStandard::Tdm1)],
            frame_rates: vec![16000],
            bits_per_slot: vec![16],
            bits_per_sample: vec![16],
        }];
        let mut pcm_format = PcmFormat {
            number_of_channels: 0,
            sample_format: SampleFormat::PcmSigned,
            bytes_per_sample: 2,
            valid_bits_per_sample: 16,
            frame_rate: 16000,
        };

        let derived = dai_format_supporting_pcm_format(&supported_formats, &pcm_format);
        assert!(derived.is_err());

        pcm_format.number_of_channels = 65;
        let derived = dai_format_supporting_pcm_format(&supported_formats, &pcm_format);
        assert!(derived.is_err());

        // 64 channels uses every bit in the mask.
        pcm_format.number_of_channels = 64;
        let derived = dai_format_supporting_pcm_format(&supported_formats, &pcm_format)
            .expect("should find a format");
        assert_eq!(u64::MAX, derived.channels_to_use_bitmask);
    }

    #[fuchsia::test]
    fn dai_format_supporting_pcm_format_large_bytes_per_sample() {
        let supported_formats = vec![DaiSupportedFormats {
            number_of_channels: vec![1],
            sample_formats: vec![DaiSampleFormat::PcmSigned],
            frame_formats: vec![DaiFrameFormat::FrameFormatStandard(DaiFrameFormatStandard::Tdm1)],
            frame_rates: vec![16000],
            bits_per_slot: vec![16],
            bits_per_sample: vec![16],
        }];
        // 32 bytes per sample is 256 bits, which doesn't fit in a u8.  This should fall back to
        // the smallest slot that fits the valid bits instead of overflowing.
        let pcm_format = PcmFormat {
            number_of_channels: 1,
            sample_format: SampleFormat::PcmSigned,
            bytes_per_sample: 32,
            valid_bits_per_sample: 16,
            frame_rate: 16000,
        };

        let derived = dai_format_supporting_pcm_format(&supported_formats, &pcm_format)
            .expect("should find a format");
        assert_eq!(16, derived.bits_per_slot);
    }
}
