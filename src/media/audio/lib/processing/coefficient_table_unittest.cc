// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/lib/processing/coefficient_table.h"

#include <gtest/gtest.h>

#include "src/media/audio/lib/format2/fixed.h"

namespace media_audio {
namespace {

TEST(CoefficientTableTest, AllIndicesAccessible) {
  Fixed width(10);
  CoefficientTableBuilder builder(width.raw_value(), Fixed::Format::FractionalBits);
  for (int64_t i = 0; i < width.raw_value(); ++i) {
    builder[i] = static_cast<float>(i);
  }
  for (int64_t i = 0; i < width.raw_value(); ++i) {
    ASSERT_FLOAT_EQ(builder[i], static_cast<float>(i)) << "index " << i;
  }

  auto table = builder.Build();
  for (int64_t i = 0; i < width.raw_value(); ++i) {
    ASSERT_FLOAT_EQ((*table)[i], static_cast<float>(i)) << "index " << i;
  }
}

TEST(CoefficientTableTest, IntegralStrideHasPhysicallyContiguousIndicies) {
  Fixed width(10);
  CoefficientTable table(width.raw_value(), Fixed::Format::FractionalBits,
                         cpp20::span<const float>{});

  for (int64_t fraction = 0; fraction < kFracOneFrame; ++fraction) {
    // Each fractional value will have a block in the vector. Now check that every valid integral
    // value is contiguous for this fractional value.
    auto block_index = fraction * width.Ceiling();
    for (int64_t integer = 0; integer < width.Ceiling(); ++integer) {
      auto fixed_value = (integer << Fixed::Format::FractionalBits) + fraction;
      ASSERT_EQ(block_index + integer, static_cast<int64_t>(table.PhysicalIndex(fixed_value)))
          << "block_index = " << block_index << ", fixed_value = " << fixed_value;
    }
  }
}

TEST(CoefficientTableTest, ReadSlice) {
  Fixed width(10);
  CoefficientTableBuilder builder(width.raw_value(), Fixed::Format::FractionalBits);

  for (size_t k = 0; k < builder.size(); k++) {
    builder[k] = static_cast<float>(k);
  }

  auto table = builder.Build();
  for (int64_t fraction = 0; fraction < kFracOneFrame; ++fraction) {
    auto slice = table->ReadSlice(fraction, width.Ceiling());
    ASSERT_NE(slice, nullptr);

    for (int64_t i = 0; i < width.Ceiling(); ++i) {
      ASSERT_EQ(slice[i], (*table)[fraction + (i << Fixed::Format::FractionalBits)]);
    }
  }
}

// Verifies high-pass table spectral inversion against low-pass/all-pass tables and wing symmetry.
TEST(SincFilterCoefficientTableTest, HighPassSymmetryAndSpectralInversion) {
  auto hp_inputs = SincFilterCoefficientTable::MakeHighPassInputs(96000, kUltrasoundMinFrequencyHz);
  EXPECT_TRUE(hp_inputs.high_pass);
  EXPECT_EQ(hp_inputs.side_length, SincFilterCoefficientTable::kFracHighPassSideLength);

  auto hp_table = SincFilterCoefficientTable::Create(hp_inputs);
  ASSERT_NE(hp_table, nullptr);
  EXPECT_EQ(hp_table->raw_table().size(), 28u * (1u << Fixed::Format::FractionalBits));
  EXPECT_NE(hp_table->ReadSlice(0, 28), nullptr);

  auto lp_inputs = SincFilterCoefficientTable::Inputs{
      .side_length = hp_inputs.side_length,
      .num_frac_bits = hp_inputs.num_frac_bits,
      .rate_conversion_ratio = hp_inputs.rate_conversion_ratio,
      .high_pass = false,
  };
  auto lp_table = SincFilterCoefficientTable::Create(lp_inputs);
  ASSERT_NE(lp_table, nullptr);

  auto ap_inputs = SincFilterCoefficientTable::Inputs{
      .side_length = hp_inputs.side_length,
      .num_frac_bits = hp_inputs.num_frac_bits,
      .rate_conversion_ratio = 1.0,
      .high_pass = false,
  };
  auto ap_table = SincFilterCoefficientTable::Create(ap_inputs);
  ASSERT_NE(ap_table, nullptr);

  // Spectral inversion must hold across all raw table indices: hp == ap - lp.
  for (int64_t idx = 0; idx < hp_inputs.side_length; ++idx) {
    EXPECT_FLOAT_EQ((*hp_table)[idx], (*ap_table)[idx] - (*lp_table)[idx]);
  }

  // At integer index 0: all_pass has 1.0f at index 0, so HP[0] == 1.0f - LP[0].
  EXPECT_NEAR((*hp_table)[0], 1.0f - (*lp_table)[0], 1e-6f);

  // At integer indices k > 0: all_pass is 0.0f, so HP[k << 13] == -LP[k << 13].
  for (int64_t k = 1; k < 28; ++k) {
    int64_t idx = k << Fixed::Format::FractionalBits;
    EXPECT_NEAR((*hp_table)[idx], -(*lp_table)[idx], 1e-6f) << "tap k=" << k;
  }

  // Verify fractional slices are well-formed and match table lookups and spectral inversion.
  for (int64_t frac = 1; frac < kFracOneFrame; ++frac) {
    auto slice = hp_table->ReadSlice(frac, 28);
    ASSERT_NE(slice, nullptr);
    for (int64_t k = 0; k < 28; ++k) {
      int64_t idx = frac + (k << Fixed::Format::FractionalBits);
      EXPECT_FLOAT_EQ(slice[k], (*hp_table)[idx]);
      EXPECT_FLOAT_EQ(slice[k], (*ap_table)[idx] - (*lp_table)[idx]);
    }
  }
}

// Verifies low-pass inputs and spectral complementarity with the high-pass table.
TEST(SincFilterCoefficientTableTest, LowPassComplementsHighPass) {
  auto lp_96k = SincFilterCoefficientTable::MakeLowPassInputs(96000, 96000, kAudioMaxFrequencyHz);
  auto lp_48k = SincFilterCoefficientTable::MakeLowPassInputs(96000, 48000, kAudioMaxFrequencyHz);
  EXPECT_FALSE(lp_96k.high_pass);
  EXPECT_EQ(lp_96k.side_length, SincFilterCoefficientTable::kFracHighPassSideLength);
  EXPECT_DOUBLE_EQ(lp_96k.rate_conversion_ratio, 46000.0 / 96000.0);

  // 96k->96k and 96k->48k low-pass filters share the exact same Inputs and coefficient table.
  EXPECT_EQ(lp_96k.side_length, lp_48k.side_length);
  EXPECT_EQ(lp_96k.num_frac_bits, lp_48k.num_frac_bits);
  EXPECT_DOUBLE_EQ(lp_96k.rate_conversion_ratio, lp_48k.rate_conversion_ratio);
  EXPECT_EQ(lp_96k.high_pass, lp_48k.high_pass);

  // When dest_rate <= 2 * kAudioMaxFrequencyHz (e.g. 16 kHz), MakeLowPassInputs matches MakeInputs.
  auto lp_16k = SincFilterCoefficientTable::MakeLowPassInputs(96000, 16000, kAudioMaxFrequencyHz);
  auto std_16k = SincFilterCoefficientTable::MakeInputs(96000, 16000);
  EXPECT_EQ(lp_16k.side_length, std_16k.side_length);
  EXPECT_DOUBLE_EQ(lp_16k.rate_conversion_ratio, std_16k.rate_conversion_ratio);

  auto lp_table = SincFilterCoefficientTable::Create(lp_96k);
  auto hp_table = SincFilterCoefficientTable::Create(
      SincFilterCoefficientTable::MakeHighPassInputs(96000, kUltrasoundMinFrequencyHz));
  ASSERT_NE(lp_table, nullptr);
  ASSERT_NE(hp_table, nullptr);

  // At integer index 0: LP[0] + HP[0] == 1.0f.
  EXPECT_NEAR((*lp_table)[0] + (*hp_table)[0], 1.0f, 1e-6f);

  // At integer indices k > 0: LP[k << 13] + HP[k << 13] == 0.0f.
  for (int64_t k = 1; k < 28; ++k) {
    int64_t idx = k << Fixed::Format::FractionalBits;
    EXPECT_NEAR((*lp_table)[idx] + (*hp_table)[idx], 0.0f, 1e-6f) << "tap k=" << k;
  }
}

}  // namespace
}  // namespace media_audio
