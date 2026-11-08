// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Hashes video frames.

use async_trait::async_trait;
use fidl_fuchsia_images2 as images2;
use hex::encode;
use mundane::hash::{Digest, Hasher, Sha256};
use std::convert::*;
use std::fmt;
use stream_processor_test::{ExpectedDigest, FatalError, Output, OutputPacket, OutputValidator};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    DataTooSmallToBeFrame { expected_size: usize, actual_size: usize },
    FormatDetailsNotUncompressedVideo,
    MissingImageFormatField(&'static str),
    InconsistentFormatDimensions,
    FormatHasNonZeroDisplayOrigin,
    FrameSizeOverflow,
    UnsupportedPixelFormat(images2::PixelFormat),
    FormatHasNoPlane { plane_requested: usize, planes_in_format: usize },
}

impl fmt::Display for Error {
    fn fmt(&self, w: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self, w)
    }
}

struct Frame<'a> {
    data: &'a [u8],
    format: &'a images2::ImageFormat,
}

fn display_dimensions(format: &images2::ImageFormat) -> Result<(u32, u32), Error> {
    let size = format.size.as_ref().ok_or(Error::MissingImageFormatField("size"))?;
    if let Some(rect) = &format.display_rect {
        if rect.x != 0 || rect.y != 0 {
            return Err(Error::FormatHasNonZeroDisplayOrigin);
        }
        Ok((rect.width, rect.height))
    } else if let Some(valid) = &format.valid_size {
        Ok((valid.width, valid.height))
    } else {
        Ok((size.width, size.height))
    }
}

trait Subsample420 {
    const CHROMA_SUBSAMPLE_RATIO: usize = 2;

    fn frame_size(format: &images2::ImageFormat) -> Result<usize, Error> {
        // For 4:2:0 YUV, the UV data is 1/2 the size of the Y data,
        // so the size of the frame is 3/2 the size of the Y plane.
        let bytes_per_row =
            *format.bytes_per_row.as_ref().ok_or(Error::MissingImageFormatField("bytes_per_row"))?
                as usize;
        let size = format.size.as_ref().ok_or(Error::MissingImageFormatField("size"))?;
        let coded_width = size.width as usize;
        let coded_height = size.height as usize;
        let (display_width, display_height) = display_dimensions(format)?;
        let display_width = display_width as usize;
        let display_height = display_height as usize;
        if bytes_per_row < Self::CHROMA_SUBSAMPLE_RATIO
            || coded_width == 0
            || coded_height == 0
            || coded_width > bytes_per_row
            || display_width == 0
            || display_height == 0
            || display_width > coded_width
            || display_height > coded_height
        {
            return Err(Error::InconsistentFormatDimensions);
        }
        bytes_per_row
            .checked_mul(coded_height)
            .and_then(|y| y.checked_mul(3))
            .map(|n| n / 2)
            .ok_or(Error::FrameSizeOverflow)
    }
}

struct Yv12Frame<'a> {
    frame: Frame<'a>,
}

impl<'a> Subsample420 for Yv12Frame<'a> {}

impl<'a> Yv12Frame<'a> {
    /// Updates `hasher` with the display data in canonical planar YV12 order (Y, V, U).
    fn hash_yv12(&self, hasher: &mut Sha256) {
        let luminance_plane_stride = *self.frame.format.bytes_per_row.as_ref().unwrap() as usize;
        let (display_width, display_height) =
            display_dimensions(self.frame.format).expect("validated by TryFrom");
        let luminance_plane_display_width = display_width as usize;
        let luminance_plane_display_height = display_height as usize;
        for row in
            self.frame.data.chunks(luminance_plane_stride).take(luminance_plane_display_height)
        {
            hasher.update(&row[..luminance_plane_display_width]);
        }

        let coded_height = self.frame.format.size.as_ref().unwrap().height as usize;
        let luminance_plane_coded_size = luminance_plane_stride * coded_height;
        let chroma_plane_display_height =
            luminance_plane_display_height / Self::CHROMA_SUBSAMPLE_RATIO;
        let chroma_plane_display_width =
            luminance_plane_display_width / Self::CHROMA_SUBSAMPLE_RATIO;

        // V rows followed by U rows.
        let chroma_rows = self.frame.data[luminance_plane_coded_size..]
            .chunks(luminance_plane_stride / Self::CHROMA_SUBSAMPLE_RATIO);

        for row in chroma_rows.clone().take(chroma_plane_display_height) {
            hasher.update(&row[..chroma_plane_display_width]);
        }

        let chroma_plane_coded_height = coded_height / Self::CHROMA_SUBSAMPLE_RATIO;
        for row in chroma_rows.skip(chroma_plane_coded_height).take(chroma_plane_display_height) {
            hasher.update(&row[..chroma_plane_display_width]);
        }
    }
}

impl<'a> TryFrom<Frame<'a>> for Yv12Frame<'a> {
    type Error = Error;
    fn try_from(frame: Frame<'a>) -> Result<Self, Self::Error> {
        let expected_size = Self::frame_size(frame.format)?;
        if frame.data.len() < expected_size {
            return Err(Error::DataTooSmallToBeFrame {
                actual_size: frame.data.len(),
                expected_size,
            });
        }

        Ok(Yv12Frame { frame })
    }
}

struct Nv12Frame<'a> {
    frame: Frame<'a>,
}

impl<'a> Subsample420 for Nv12Frame<'a> {}

impl<'a> Nv12Frame<'a> {
    /// Updates `hasher` with the display data in canonical planar YV12 order (Y, V, U).
    fn hash_yv12(&self, hasher: &mut Sha256) {
        let stride = *self.frame.format.bytes_per_row.as_ref().unwrap() as usize;
        let (display_width, display_height) =
            display_dimensions(self.frame.format).expect("validated by TryFrom");
        let row_length = display_width as usize;
        let luminance_row_count = display_height as usize;
        let chroma_row_count = luminance_row_count / Self::CHROMA_SUBSAMPLE_RATIO;
        let coded_height = self.frame.format.size.as_ref().unwrap().height as usize;

        for row in self.frame.data.chunks(stride).take(luminance_row_count) {
            hasher.update(&row[..row_length]);
        }

        let chroma_rows =
            self.frame.data[stride * coded_height..].chunks(stride).take(chroma_row_count);
        let mut chroma_scratch = Vec::with_capacity(row_length.div_ceil(2));

        // V plane first, then U plane, matching Yv12Frame canonical digest order.
        for row in chroma_rows.clone() {
            chroma_scratch.clear();
            chroma_scratch.extend(row[..row_length].iter().skip(1).step_by(2).copied());
            hasher.update(&chroma_scratch);
        }
        for row in chroma_rows {
            chroma_scratch.clear();
            chroma_scratch.extend(row[..row_length].iter().step_by(2).copied());
            hasher.update(&chroma_scratch);
        }
    }
}

impl<'a> TryFrom<Frame<'a>> for Nv12Frame<'a> {
    type Error = Error;
    fn try_from(frame: Frame<'a>) -> Result<Self, Self::Error> {
        let expected_size = Self::frame_size(frame.format)?;
        if frame.data.len() < expected_size {
            return Err(Error::DataTooSmallToBeFrame {
                actual_size: frame.data.len(),
                expected_size,
            });
        }

        Ok(Nv12Frame { frame })
    }
}

/// Updates `hasher` with `src`'s display data in canonical planar YV12 order (Y, V, U) without
/// finishing it. On `Err`, `hasher` is unchanged.
fn hash_packet_display_data(src: &OutputPacket, hasher: &mut Sha256) -> Result<(), Error> {
    let format = src
        .format
        .format_details
        .image_format
        .as_ref()
        .ok_or(Error::FormatDetailsNotUncompressedVideo)?;

    let pixel_format =
        *format.pixel_format.as_ref().ok_or(Error::MissingImageFormatField("pixel_format"))?;
    match pixel_format {
        images2::PixelFormat::Yv12 => {
            Yv12Frame::try_from(Frame { data: src.data.as_slice(), format })?.hash_yv12(hasher);
        }
        images2::PixelFormat::Nv12 => {
            Nv12Frame::try_from(Frame { data: src.data.as_slice(), format })?.hash_yv12(hasher);
        }
        _ => return Err(Error::UnsupportedPixelFormat(pixel_format)),
    }
    Ok(())
}

pub struct VideoFrameHasher {
    pub expected_digest: ExpectedDigest,
}

#[async_trait(?Send)]
impl OutputValidator for VideoFrameHasher {
    async fn validate(&self, output: &[Output]) -> Result<(), anyhow::Error> {
        let mut hasher = Sha256::default();
        let mut frame_ordinal = 0;

        if let Some(per_frame_bytes) = &self.expected_digest.per_frame_bytes {
            let packet_count = output
                .iter()
                .filter(|item| {
                    if let Output::Packet(_packet) = item {
                        return true;
                    }
                    false
                })
                .count();
            if per_frame_bytes.len() != packet_count {
                return Err(FatalError(format!(
                    "per_frame_bytes.len() != packet_count - {} vs {}",
                    per_frame_bytes.len(),
                    packet_count
                ))
                .into());
            }
        }

        output
            .iter()
            .map(|output| {
                if let Output::Packet(packet) = output {
                    hash_packet_display_data(packet, &mut hasher)?;
                    let tmp_hasher = hasher.clone();
                    let frame_digest = tmp_hasher.finish().bytes();
                    if let Some(per_frame_bytes) = &self.expected_digest.per_frame_bytes {
                        if per_frame_bytes[frame_ordinal] != frame_digest {
                            return Err(FatalError(format!(
                                "frame_ordinal {} digest mismatch - expected {}; got {}",
                                frame_ordinal,
                                encode(per_frame_bytes[frame_ordinal]),
                                encode(frame_digest)
                            ))
                            .into());
                        }
                    }

                    frame_ordinal += 1;
                }
                Ok(())
            })
            .collect::<Result<(), anyhow::Error>>()?;

        let digest = hasher.finish().bytes();
        if self.expected_digest.bytes != digest {
            return Err(FatalError(format!(
                "Expected {}; got {}",
                self.expected_digest,
                encode(digest)
            ))
            .into());
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use fidl_fuchsia_images2::{self as fimages2, *};
    use fidl_fuchsia_math::{RectU, SizeU};
    use fidl_fuchsia_media::FormatDetails;
    use fuchsia_stream_processors::{ValidPacket, ValidPacketHeader};
    use rand::prelude::*;
    use std::rc::Rc;
    use stream_processor_test::ValidStreamOutputFormat;

    #[derive(Debug, Copy, Clone)]
    struct TestSpec {
        pixel_format: PixelFormat,
        coded_width: usize, // coded_height() is in the impl
        display_width: usize,
        display_height: usize,
        bytes_per_row: usize,
    }

    impl TestSpec {
        fn coded_height(&self) -> usize {
            // This just makes sure that coded_height() is > height.
            self.display_height * 2
        }
    }

    fn make_output_packet(
        pixel_format: PixelFormat,
        coded_width: usize,
        coded_height: usize,
        display_width: usize,
        display_height: usize,
        bytes_per_row: usize,
        data: Vec<u8>,
    ) -> OutputPacket {
        let mut format_details = FormatDetails::default();
        format_details.image_format = Some(ImageFormat {
            pixel_format: Some(pixel_format),
            size: Some(SizeU {
                width: coded_width.try_into().unwrap(),
                height: coded_height.try_into().unwrap(),
            }),
            bytes_per_row: Some(bytes_per_row.try_into().unwrap()),
            display_rect: Some(RectU {
                x: 0,
                y: 0,
                width: display_width.try_into().unwrap(),
                height: display_height.try_into().unwrap(),
            }),
            color_space: Some(fimages2::ColorSpace::Rec709),
            ..Default::default()
        });
        OutputPacket {
            data,
            format: Rc::new(ValidStreamOutputFormat { stream_lifetime_ordinal: 0, format_details }),
            packet: ValidPacket {
                header: ValidPacketHeader { buffer_lifetime_ordinal: 0, packet_index: 0 },
                buffer_index: 0,
                stream_lifetime_ordinal: 0,
                start_offset: 0,
                valid_length_bytes: 0,
                timestamp_ish: None,
                start_access_unit: false,
                known_end_access_unit: false,
            },
        }
    }

    impl Into<OutputPacket> for TestSpec {
        fn into(self) -> OutputPacket {
            let coded_height = self.coded_height();
            make_output_packet(
                self.pixel_format,
                self.coded_width,
                coded_height,
                self.display_width,
                self.display_height,
                self.bytes_per_row,
                vec![0; self.bytes_per_row * coded_height * 3 / 2],
            )
        }
    }

    fn specs(pixel_format: PixelFormat) -> impl Iterator<Item = TestSpec> {
        vec![
            TestSpec {
                pixel_format,
                coded_width: 16,
                display_width: 10,
                display_height: 16,
                bytes_per_row: 20,
            },
            TestSpec {
                pixel_format,
                coded_width: 16,
                display_width: 14,
                display_height: 8,
                bytes_per_row: 16,
            },
            TestSpec {
                pixel_format,
                coded_width: 32,
                display_width: 12,
                display_height: 4,
                bytes_per_row: 54,
            },
            TestSpec {
                pixel_format,
                coded_width: 16,
                display_width: 2,
                display_height: 100,
                bytes_per_row: 1200,
            },
        ]
        .into_iter()
    }

    #[fuchsia::test]
    fn packets_of_different_formats_hash_same_with_matching_data() -> Result<(), Error> {
        let mut rng = StdRng::seed_from_u64(45);

        for (nv12_spec, yv12_spec) in specs(PixelFormat::Nv12).zip(specs(PixelFormat::Yv12)) {
            // Initialize two packets with random data. Then set every display byte to
            // `global_index % 256`, where `global_index` is a count of every display byte so far
            // in the frame.

            let mut nv12_packet: OutputPacket = nv12_spec.into();
            rng.fill(nv12_packet.data.as_mut_slice());

            let mut nv12_global_index: usize = 0;
            let mut assign_index = |b: &mut u8| {
                *b = (nv12_global_index % 256) as u8;
                nv12_global_index += 1;
            };

            // NV12 Luminance plane.
            for row in
                nv12_packet.data.chunks_mut(nv12_spec.bytes_per_row).take(nv12_spec.display_height)
            {
                row.iter_mut().take(nv12_spec.display_width).for_each(&mut assign_index);
            }

            // NV12 Chroma V plane.
            for row in nv12_packet
                .data
                .chunks_mut(nv12_spec.bytes_per_row)
                .skip(nv12_spec.coded_height() as usize)
                .take(nv12_spec.display_height / Nv12Frame::CHROMA_SUBSAMPLE_RATIO)
            {
                row.iter_mut()
                    .take(nv12_spec.display_width)
                    .skip(1)
                    .step_by(2)
                    .for_each(&mut assign_index)
            }

            // NV12 Chroma U plane.
            for row in nv12_packet
                .data
                .chunks_mut(nv12_spec.bytes_per_row)
                .skip(nv12_spec.coded_height() as usize)
                .take(nv12_spec.display_height / Nv12Frame::CHROMA_SUBSAMPLE_RATIO)
            {
                row.iter_mut().take(nv12_spec.display_width).step_by(2).for_each(&mut assign_index)
            }

            let mut yv12_packet: OutputPacket = yv12_spec.into();
            rng.fill(yv12_packet.data.as_mut_slice());

            let mut yv12_global_index: usize = 0;
            let mut assign_index = |b: &mut u8| {
                *b = (yv12_global_index % 256) as u8;
                yv12_global_index += 1;
            };

            // YV12 Luminance plane.
            for row in
                yv12_packet.data.chunks_mut(yv12_spec.bytes_per_row).take(yv12_spec.display_height)
            {
                row.iter_mut().take(yv12_spec.display_width).for_each(&mut assign_index);
            }

            // YV12 Chroma V plane.
            let luminance_plane_size = yv12_spec.bytes_per_row * yv12_spec.coded_height();
            for row in yv12_packet.data[luminance_plane_size..]
                .chunks_mut(yv12_spec.bytes_per_row / Yv12Frame::CHROMA_SUBSAMPLE_RATIO)
                .take(yv12_spec.display_height / Yv12Frame::CHROMA_SUBSAMPLE_RATIO)
            {
                row.iter_mut()
                    .take(yv12_spec.display_width / Yv12Frame::CHROMA_SUBSAMPLE_RATIO)
                    .for_each(&mut assign_index)
            }

            // YV12 Chroma U plane.
            let chrominance_plane_size =
                luminance_plane_size / Yv12Frame::CHROMA_SUBSAMPLE_RATIO.pow(2);
            for row in yv12_packet.data[(luminance_plane_size + chrominance_plane_size)..]
                .chunks_mut(yv12_spec.bytes_per_row / Yv12Frame::CHROMA_SUBSAMPLE_RATIO)
                .take(yv12_spec.display_height / Yv12Frame::CHROMA_SUBSAMPLE_RATIO)
            {
                row.iter_mut()
                    .take(yv12_spec.display_width / Yv12Frame::CHROMA_SUBSAMPLE_RATIO)
                    .for_each(&mut assign_index)
            }

            let mut yv12_hasher = Sha256::default();
            let mut nv12_hasher = Sha256::default();
            hash_packet_display_data(&yv12_packet, &mut yv12_hasher)?;
            hash_packet_display_data(&nv12_packet, &mut nv12_hasher)?;

            assert_eq!(yv12_hasher.finish(), nv12_hasher.finish());
        }

        Ok(())
    }

    #[fuchsia::test]
    fn nv12_and_yv12_hash_expected_cropped_canonical_yv12_bytes() -> Result<(), Error> {
        const DISPLAY_WIDTH: usize = 4;
        const DISPLAY_HEIGHT: usize = 4;
        const CODED_WIDTH: usize = 6;
        const CODED_HEIGHT: usize = 6;
        const BYTES_PER_ROW: usize = 8;

        #[rustfmt::skip]
        let nv12_data: Vec<u8> = vec![
            // Luminance rows 0..4 (4 visible bytes, 2 coded-width pad bytes, 2 stride pad bytes).
            0x01, 0x02, 0x03, 0x04, 0xa0, 0xa1, 0xa2, 0xa3,
            0x05, 0x06, 0x07, 0x08, 0xa4, 0xa5, 0xa6, 0xa7,
            0x09, 0x0a, 0x0b, 0x0c, 0xa8, 0xa9, 0xaa, 0xab,
            0x0d, 0x0e, 0x0f, 0x10, 0xac, 0xad, 0xae, 0xaf,
            // Coded-height padding luminance rows 4..6 (cropped by display_height = 4).
            0xb0, 0xb1, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7,
            0xb8, 0xb9, 0xba, 0xbb, 0xbc, 0xbd, 0xbe, 0xbf,
            // NV12 interleaved [U0, V0, U1, V1, pad..] chroma rows 0..2, plus coded-height pad row.
            0x31, 0x21, 0x32, 0x22, 0xc0, 0xc1, 0xc2, 0xc3,
            0x33, 0x23, 0x34, 0x24, 0xc4, 0xc5, 0xc6, 0xc7,
            0xc8, 0xc9, 0xca, 0xcb, 0xcc, 0xcd, 0xce, 0xcf,
        ];

        #[rustfmt::skip]
        let yv12_data: Vec<u8> = vec![
            // Luminance rows 0..4 (4 visible bytes, 2 coded-width pad bytes, 2 stride pad bytes).
            0x01, 0x02, 0x03, 0x04, 0xd0, 0xd1, 0xd2, 0xd3,
            0x05, 0x06, 0x07, 0x08, 0xd4, 0xd5, 0xd6, 0xd7,
            0x09, 0x0a, 0x0b, 0x0c, 0xd8, 0xd9, 0xda, 0xdb,
            0x0d, 0x0e, 0x0f, 0x10, 0xdc, 0xdd, 0xde, 0xdf,
            // Coded-height padding luminance rows 4..6 (cropped by display_height = 4).
            0xe0, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7,
            0xe8, 0xe9, 0xea, 0xeb, 0xec, 0xed, 0xee, 0xef,
            // YV12 planar V rows 0..2 (2 visible bytes, 2 pad bytes) plus coded-height pad row.
            0x21, 0x22, 0xf0, 0xf1,
            0x23, 0x24, 0xf2, 0xf3,
            0xf4, 0xf5, 0xf6, 0xf7,
            // YV12 planar U rows 0..2 (2 visible bytes, 2 pad bytes) plus coded-height pad row.
            0x31, 0x32, 0xf8, 0xf9,
            0x33, 0x34, 0xfa, 0xfb,
            0xfc, 0xfd, 0xfe, 0xff,
        ];

        #[rustfmt::skip]
        let expected_canonical_yv12: [u8; 24] = [
            // Y (4x4)
            0x01, 0x02, 0x03, 0x04,
            0x05, 0x06, 0x07, 0x08,
            0x09, 0x0a, 0x0b, 0x0c,
            0x0d, 0x0e, 0x0f, 0x10,
            // V (2x2)
            0x21, 0x22,
            0x23, 0x24,
            // U (2x2)
            0x31, 0x32,
            0x33, 0x34,
        ];

        let nv12_packet = make_output_packet(
            PixelFormat::Nv12,
            CODED_WIDTH,
            CODED_HEIGHT,
            DISPLAY_WIDTH,
            DISPLAY_HEIGHT,
            BYTES_PER_ROW,
            nv12_data,
        );
        let yv12_packet = make_output_packet(
            PixelFormat::Yv12,
            CODED_WIDTH,
            CODED_HEIGHT,
            DISPLAY_WIDTH,
            DISPLAY_HEIGHT,
            BYTES_PER_ROW,
            yv12_data,
        );

        let expected_digest = Sha256::hash(&expected_canonical_yv12);

        let mut nv12_hasher = Sha256::default();
        hash_packet_display_data(&nv12_packet, &mut nv12_hasher)?;
        assert_eq!(nv12_hasher.finish(), expected_digest);

        let mut yv12_hasher = Sha256::default();
        hash_packet_display_data(&yv12_packet, &mut yv12_hasher)?;
        assert_eq!(yv12_hasher.finish(), expected_digest);

        Ok(())
    }

    #[fuchsia::test]
    fn sanity_test_that_different_packets_hash_differently() -> Result<(), Error> {
        let mut rng = StdRng::seed_from_u64(45);

        for (nv12_spec, yv12_spec) in specs(PixelFormat::Nv12).zip(specs(PixelFormat::Yv12)) {
            let mut nv12_packet: OutputPacket = nv12_spec.into();
            rng.fill(nv12_packet.data.as_mut_slice());

            let mut yv12_packet: OutputPacket = yv12_spec.into();
            yv12_packet.data.iter_mut().enumerate().for_each(|(i, b)| *b = nv12_packet.data[i]);

            // Change a random display byte.
            let x = rng.random_range(0..nv12_spec.display_width);
            let y = {
                let y_range = [
                    // Luminance plane rows.
                    (0..nv12_spec.display_height),
                    // Chrominance plane rows.
                    {
                        let start = nv12_spec.coded_height();
                        let end = start + (nv12_spec.display_height / 2);
                        start..end
                    },
                ]
                .choose(&mut rng)
                .expect("Sampling from nonempty slice")
                .clone();
                rng.random_range(y_range)
            };
            let idx = y * nv12_spec.bytes_per_row + x;
            nv12_packet.data[idx] = nv12_packet.data[idx].overflowing_add(1).0;

            let mut yv12_hasher = Sha256::default();
            let mut nv12_hasher = Sha256::default();
            hash_packet_display_data(&yv12_packet, &mut yv12_hasher)?;
            hash_packet_display_data(&nv12_packet, &mut nv12_hasher)?;

            assert_ne!(yv12_hasher.finish(), nv12_hasher.finish());
        }

        Ok(())
    }

    #[fuchsia::test]
    fn missing_size_returns_err_without_panic() {
        let spec = specs(PixelFormat::Nv12).next().unwrap();
        let mut packet: OutputPacket = spec.into();
        {
            let image_format = Rc::get_mut(&mut packet.format)
                .unwrap()
                .format_details
                .image_format
                .as_mut()
                .unwrap();
            image_format.size = None;
            assert_eq!(
                display_dimensions(image_format),
                Err(Error::MissingImageFormatField("size"))
            );
        }
        let mut err_hasher = Sha256::default();
        assert_eq!(
            hash_packet_display_data(&packet, &mut err_hasher),
            Err(Error::MissingImageFormatField("size"))
        );
        assert_eq!(err_hasher.finish(), Sha256::default().finish());
    }

    #[fuchsia::test]
    fn display_rect_none_falls_back_to_valid_size_then_coded_size() {
        let mut rng = StdRng::seed_from_u64(46);
        let spec = specs(PixelFormat::Nv12).next().unwrap();
        let mut packet: OutputPacket = spec.into();
        rng.fill(packet.data.as_mut_slice());

        // Clearing `display_rect` (with `valid_size` unset) falls back to the coded-size display
        // area.
        {
            let image_format = Rc::get_mut(&mut packet.format)
                .unwrap()
                .format_details
                .image_format
                .as_mut()
                .unwrap();
            image_format.display_rect = None;
            image_format.valid_size = None;
        }
        let mut coded_fallback_hasher = Sha256::default();
        hash_packet_display_data(&packet, &mut coded_fallback_hasher)
            .expect("fallback to coded size should succeed");

        let mut twin_coded_rect_packet: OutputPacket = spec.into();
        twin_coded_rect_packet.data.copy_from_slice(&packet.data);
        {
            let image_format = Rc::get_mut(&mut twin_coded_rect_packet.format)
                .unwrap()
                .format_details
                .image_format
                .as_mut()
                .unwrap();
            image_format.display_rect = Some(RectU {
                x: 0,
                y: 0,
                width: spec.coded_width.try_into().unwrap(),
                height: spec.coded_height().try_into().unwrap(),
            });
        }
        let mut twin_coded_rect_hasher = Sha256::default();
        hash_packet_display_data(&twin_coded_rect_packet, &mut twin_coded_rect_hasher)
            .expect("coded display_rect packet should succeed");
        let coded_fallback_digest = coded_fallback_hasher.finish();
        assert_eq!(coded_fallback_digest, twin_coded_rect_hasher.finish());

        // `valid_size`-only (with `display_rect == None`) yields the exact same digest as a twin
        // packet carrying the equivalent `display_rect` (and differs from the larger coded-size
        // fallback digest).
        let mut valid_size_packet: OutputPacket = spec.into();
        valid_size_packet.data.copy_from_slice(&packet.data);
        {
            let image_format = Rc::get_mut(&mut valid_size_packet.format)
                .unwrap()
                .format_details
                .image_format
                .as_mut()
                .unwrap();
            image_format.display_rect = None;
            image_format.valid_size = Some(SizeU {
                width: spec.display_width.try_into().unwrap(),
                height: spec.display_height.try_into().unwrap(),
            });
        }
        let mut twin_display_rect_packet: OutputPacket = spec.into();
        twin_display_rect_packet.data.copy_from_slice(&packet.data);

        let mut valid_size_hasher = Sha256::default();
        hash_packet_display_data(&valid_size_packet, &mut valid_size_hasher)
            .expect("valid_size fallback should succeed");
        let mut display_rect_hasher = Sha256::default();
        hash_packet_display_data(&twin_display_rect_packet, &mut display_rect_hasher)
            .expect("display_rect packet should succeed");
        let valid_size_digest = valid_size_hasher.finish();
        assert_eq!(valid_size_digest, display_rect_hasher.finish());
        assert_ne!(valid_size_digest, coded_fallback_digest);
    }

    #[fuchsia::test]
    fn format_rejection_cases_return_expected_errors() {
        type Mutator = fn(&mut OutputPacket);
        let empty_digest = Sha256::default().finish();

        for pixel_format in [PixelFormat::Nv12, PixelFormat::Yv12] {
            let spec = specs(pixel_format).next().unwrap();
            let expected_size = spec.bytes_per_row * spec.coded_height() * 3 / 2;
            let cases: &[(&str, Mutator, Error)] = &[
                (
                    "missing image_format",
                    |p| {
                        Rc::get_mut(&mut p.format).unwrap().format_details.image_format = None;
                    },
                    Error::FormatDetailsNotUncompressedVideo,
                ),
                (
                    "bytes_per_row < 2",
                    |p| {
                        let fmt = Rc::get_mut(&mut p.format)
                            .unwrap()
                            .format_details
                            .image_format
                            .as_mut()
                            .unwrap();
                        fmt.bytes_per_row = Some(1);
                        fmt.size = Some(SizeU { width: 1, height: 2 });
                        fmt.display_rect = Some(RectU { x: 0, y: 0, width: 1, height: 2 });
                    },
                    Error::InconsistentFormatDimensions,
                ),
                (
                    "zero coded width",
                    |p| {
                        let fmt = Rc::get_mut(&mut p.format)
                            .unwrap()
                            .format_details
                            .image_format
                            .as_mut()
                            .unwrap();
                        fmt.size.as_mut().unwrap().width = 0;
                    },
                    Error::InconsistentFormatDimensions,
                ),
                (
                    "zero coded height",
                    |p| {
                        let fmt = Rc::get_mut(&mut p.format)
                            .unwrap()
                            .format_details
                            .image_format
                            .as_mut()
                            .unwrap();
                        fmt.size.as_mut().unwrap().height = 0;
                    },
                    Error::InconsistentFormatDimensions,
                ),
                (
                    "coded_width > bytes_per_row",
                    |p| {
                        let fmt = Rc::get_mut(&mut p.format)
                            .unwrap()
                            .format_details
                            .image_format
                            .as_mut()
                            .unwrap();
                        fmt.size.as_mut().unwrap().width = fmt.bytes_per_row.unwrap() + 2;
                    },
                    Error::InconsistentFormatDimensions,
                ),
                (
                    "zero display dimension",
                    |p| {
                        let fmt = Rc::get_mut(&mut p.format)
                            .unwrap()
                            .format_details
                            .image_format
                            .as_mut()
                            .unwrap();
                        fmt.display_rect.as_mut().unwrap().width = 0;
                    },
                    Error::InconsistentFormatDimensions,
                ),
                (
                    "display larger than coded",
                    |p| {
                        let fmt = Rc::get_mut(&mut p.format)
                            .unwrap()
                            .format_details
                            .image_format
                            .as_mut()
                            .unwrap();
                        let coded_w = fmt.size.as_ref().unwrap().width;
                        fmt.display_rect.as_mut().unwrap().width = coded_w + 2;
                    },
                    Error::InconsistentFormatDimensions,
                ),
                (
                    "missing bytes_per_row",
                    |p| {
                        let fmt = Rc::get_mut(&mut p.format)
                            .unwrap()
                            .format_details
                            .image_format
                            .as_mut()
                            .unwrap();
                        fmt.bytes_per_row = None;
                    },
                    Error::MissingImageFormatField("bytes_per_row"),
                ),
                (
                    "missing pixel_format",
                    |p| {
                        let fmt = Rc::get_mut(&mut p.format)
                            .unwrap()
                            .format_details
                            .image_format
                            .as_mut()
                            .unwrap();
                        fmt.pixel_format = None;
                    },
                    Error::MissingImageFormatField("pixel_format"),
                ),
                (
                    "unsupported pixel_format",
                    |p| {
                        let fmt = Rc::get_mut(&mut p.format)
                            .unwrap()
                            .format_details
                            .image_format
                            .as_mut()
                            .unwrap();
                        fmt.pixel_format = Some(PixelFormat::R8G8B8A8);
                    },
                    Error::UnsupportedPixelFormat(PixelFormat::R8G8B8A8),
                ),
                (
                    "truncated packet data",
                    |p| {
                        p.data.pop();
                    },
                    Error::DataTooSmallToBeFrame { expected_size, actual_size: expected_size - 1 },
                ),
            ];

            for (name, mutate, expected_err) in cases {
                let mut packet: OutputPacket = spec.into();
                mutate(&mut packet);
                let mut hasher = Sha256::default();
                assert_eq!(
                    hash_packet_display_data(&packet, &mut hasher).err().as_ref(),
                    Some(expected_err),
                    "{pixel_format:?} {name}"
                );
                assert_eq!(
                    hasher.finish(),
                    empty_digest,
                    "{pixel_format:?} {name}: hasher must be unchanged on Err"
                );
            }
        }
    }

    #[fuchsia::test]
    fn non_zero_display_origin_returns_err() {
        let spec = specs(PixelFormat::Nv12).next().unwrap();
        let empty_digest = Sha256::default().finish();
        for (x, y) in [(2, 0), (0, 2)] {
            let mut packet: OutputPacket = spec.into();
            let rect = Rc::get_mut(&mut packet.format)
                .unwrap()
                .format_details
                .image_format
                .as_mut()
                .unwrap()
                .display_rect
                .as_mut()
                .unwrap();
            rect.x = x;
            rect.y = y;
            let mut hasher = Sha256::default();
            assert_eq!(
                hash_packet_display_data(&packet, &mut hasher),
                Err(Error::FormatHasNonZeroDisplayOrigin)
            );
            assert_eq!(hasher.finish(), empty_digest);
        }
    }

    #[fuchsia::test]
    fn frame_size_overflow_returns_err() {
        let spec = specs(PixelFormat::Yv12).next().unwrap();
        let mut packet: OutputPacket = spec.into();
        let mut hasher = Sha256::default();
        packet.data.clear();
        let image_format =
            Rc::get_mut(&mut packet.format).unwrap().format_details.image_format.as_mut().unwrap();
        image_format.size = Some(SizeU { width: u32::MAX, height: u32::MAX });
        image_format.bytes_per_row = Some(u32::MAX);
        image_format.display_rect = None;
        image_format.valid_size = None;
        // With `u32` inputs on a 64-bit `usize`, `bytes_per_row.checked_mul(coded_height)` fits in
        // `usize` and only the subsequent `.checked_mul(3)` overflows. Check both `Yv12Frame` and
        // `Nv12Frame` in case either overrides `Subsample420::frame_size` in the future.
        assert_eq!(Yv12Frame::frame_size(image_format), Err(Error::FrameSizeOverflow));
        assert_eq!(Nv12Frame::frame_size(image_format), Err(Error::FrameSizeOverflow));
        assert_eq!(hash_packet_display_data(&packet, &mut hasher), Err(Error::FrameSizeOverflow));
        assert_eq!(hasher.finish(), Sha256::default().finish());
    }
}
