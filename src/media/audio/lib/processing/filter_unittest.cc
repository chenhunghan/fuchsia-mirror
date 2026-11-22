// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/lib/processing/filter.h"

#include <lib/syslog/cpp/macros.h>

#include <cmath>
#include <limits>
#include <vector>

#include <gtest/gtest.h>

namespace media_audio {
namespace {

#define EXPECT_AUDIO_TEST_DOUBLE_EQ(X, Y) EXPECT_NEAR(X, Y, std::numeric_limits<double>::epsilon())

TEST(LinearFilterTest, Construction) {
  {
    auto source_rate = 48000;
    auto dest_rate = 48000;
    int32_t expected_num_frac_bits = Fixed::Format::FractionalBits;  // default
    LinearFilter filter(source_rate, dest_rate);
    auto expected_side_length = 1 << expected_num_frac_bits;

    EXPECT_EQ(filter.source_rate(), source_rate);
    EXPECT_EQ(filter.dest_rate(), dest_rate);
    EXPECT_EQ(filter.num_frac_bits(), expected_num_frac_bits);
    EXPECT_EQ(filter.side_length(), expected_side_length);
    EXPECT_AUDIO_TEST_DOUBLE_EQ(filter.rate_conversion_ratio(), 1.0);
  }

  {
    auto source_rate = 32000;
    auto dest_rate = 96000;
    int32_t expected_num_frac_bits = Fixed::Format::FractionalBits;  // default
    LinearFilter filter(source_rate, dest_rate);
    auto expected_side_length = 1 << expected_num_frac_bits;

    EXPECT_EQ(filter.source_rate(), source_rate);
    EXPECT_EQ(filter.dest_rate(), dest_rate);
    EXPECT_EQ(filter.num_frac_bits(), expected_num_frac_bits);
    EXPECT_EQ(filter.side_length(), expected_side_length);
    EXPECT_AUDIO_TEST_DOUBLE_EQ(filter.rate_conversion_ratio(), 3.0);
  }

  {
    auto source_rate = 96000;
    auto dest_rate = 48000;
    int32_t num_frac_bits = 6;
    LinearFilter filter(source_rate, dest_rate, num_frac_bits);
    auto expected_side_length = 1 << num_frac_bits;

    EXPECT_EQ(filter.source_rate(), source_rate);
    EXPECT_EQ(filter.dest_rate(), dest_rate);
    EXPECT_EQ(filter.num_frac_bits(), num_frac_bits);
    EXPECT_EQ(filter.side_length(), expected_side_length);
    EXPECT_AUDIO_TEST_DOUBLE_EQ(filter.rate_conversion_ratio(), 0.5);
  }
}

TEST(SincFilterTest, Construction) {
  {
    auto source_rate = 48000;
    auto dest_rate = 48000;
    int32_t expected_num_frac_bits = Fixed::Format::FractionalBits;  // default
    auto num_taps = SincFilter::kSideTaps;                           // default
    auto side_length = (num_taps + 1) << expected_num_frac_bits;     // default
    SincFilter filter(source_rate, dest_rate);

    EXPECT_EQ(filter.source_rate(), source_rate);
    EXPECT_EQ(filter.dest_rate(), dest_rate);
    EXPECT_EQ(filter.num_frac_bits(), expected_num_frac_bits);
    EXPECT_EQ(filter.side_length(), side_length);
    EXPECT_AUDIO_TEST_DOUBLE_EQ(filter.rate_conversion_ratio(), 1.0);
  }

  {
    auto source_rate = 32000;
    auto dest_rate = 96000;
    int32_t expected_num_frac_bits = Fixed::Format::FractionalBits;  // default
    auto num_taps = SincFilter::kSideTaps;                           // default
    auto side_length = (num_taps + 1) << expected_num_frac_bits;     // default
    SincFilter filter(source_rate, dest_rate);

    EXPECT_EQ(filter.source_rate(), source_rate);
    EXPECT_EQ(filter.dest_rate(), dest_rate);
    EXPECT_EQ(filter.side_length(), side_length);
    EXPECT_EQ(filter.num_frac_bits(), expected_num_frac_bits);
    EXPECT_AUDIO_TEST_DOUBLE_EQ(filter.rate_conversion_ratio(), 3.0);
  }

  {
    auto source_rate = 96000;
    auto dest_rate = 48000;
    auto num_taps = 9;
    int32_t num_frac_bits = Fixed::Format::FractionalBits;
    auto side_length = (num_taps + 1) << num_frac_bits;
    SincFilter filter(source_rate, dest_rate, side_length);

    EXPECT_EQ(filter.source_rate(), source_rate);
    EXPECT_EQ(filter.dest_rate(), dest_rate);
    EXPECT_EQ(filter.side_length(), side_length);
    EXPECT_EQ(filter.num_frac_bits(), num_frac_bits);
    EXPECT_AUDIO_TEST_DOUBLE_EQ(filter.rate_conversion_ratio(), 0.5);
  }

  {
    auto source_rate = 16000;
    auto dest_rate = 96000;
    int32_t num_frac_bits = 4;
    auto num_taps = 10;
    auto side_length = (num_taps + 1) << num_frac_bits;
    SincFilter filter(source_rate, dest_rate, side_length, num_frac_bits);

    EXPECT_EQ(filter.source_rate(), source_rate);
    EXPECT_EQ(filter.dest_rate(), dest_rate);
    EXPECT_EQ(filter.side_length(), side_length);
    EXPECT_EQ(filter.num_frac_bits(), num_frac_bits);
    EXPECT_AUDIO_TEST_DOUBLE_EQ(filter.rate_conversion_ratio(), 6.0);
  }
}

TEST(LinearFilterTest, FilterCoefficients) {
  auto source_rate = 48000;
  auto dest_rate = 48000;
  int32_t num_frac_bits = 6;
  LinearFilter filter(source_rate, dest_rate, num_frac_bits);

  float frac_size = static_cast<float>(1 << num_frac_bits);
  int64_t expected_side_length = static_cast<int64_t>(frac_size);
  EXPECT_EQ(filter.side_length(), expected_side_length);

  for (auto idx = 0; idx < expected_side_length; ++idx) {
    EXPECT_FLOAT_EQ(filter[idx], (frac_size - static_cast<float>(idx)) / frac_size);
  }
}

TEST(SincFilterTest, FilterCoefficients_Unity) {
  auto source_rate = 48000;
  auto dest_rate = 48000;
  auto num_taps = 10;
  int32_t num_frac_bits = 4;
  auto side_length = ((num_taps + 1) << num_frac_bits);
  SincFilter filter(source_rate, dest_rate, side_length, num_frac_bits);

  EXPECT_EQ(filter.source_rate(), source_rate);
  EXPECT_EQ(filter.dest_rate(), dest_rate);
  EXPECT_EQ(filter.side_length(), side_length);
  EXPECT_EQ(filter.num_frac_bits(), num_frac_bits);
  EXPECT_AUDIO_TEST_DOUBLE_EQ(filter.rate_conversion_ratio(), 1.0);

  EXPECT_FLOAT_EQ(filter[0], 1.0f);
  auto frac_size = 1 << num_frac_bits;
  auto frac_half = frac_size >> 1;
  auto idx = frac_size;
  for (auto tap = 1; tap <= num_taps; ++tap, idx += frac_size) {
    EXPECT_FLOAT_EQ(filter[idx], 0.0f) << "idx: " << idx;

    if (tap & 1) {
      EXPECT_GT(filter[idx - frac_half], 0.0f);
      EXPECT_LT(filter[idx + frac_half], 0.0f);
    } else {
      EXPECT_LT(filter[idx - frac_half], 0.0f);
      EXPECT_GT(filter[idx + frac_half], 0.0f);
    }
  }
}

TEST(SincFilterTest, FilterCoefficients_DownSample) {
  auto source_rate = 48000;
  auto dest_rate = 24000;
  auto num_taps = 9;
  int32_t num_frac_bits = 4;
  auto side_length = ((num_taps + 1) << num_frac_bits);
  SincFilter filter(source_rate, dest_rate, side_length, num_frac_bits);

  EXPECT_EQ(filter.source_rate(), source_rate);
  EXPECT_EQ(filter.dest_rate(), dest_rate);
  EXPECT_EQ(filter.side_length(), side_length);
  EXPECT_AUDIO_TEST_DOUBLE_EQ(filter.rate_conversion_ratio(), 0.5);
}

TEST(SincFilterTest, FilterCoefficients_UpSample) {
  auto source_rate = 24000;
  auto dest_rate = 48000;
  auto num_taps = 8;
  int32_t num_frac_bits = 3;
  auto side_length = ((num_taps + 1) << num_frac_bits);
  SincFilter filter(source_rate, dest_rate, side_length, num_frac_bits);

  EXPECT_EQ(filter.source_rate(), source_rate);
  EXPECT_EQ(filter.dest_rate(), dest_rate);
  EXPECT_EQ(filter.side_length(), side_length);
  EXPECT_AUDIO_TEST_DOUBLE_EQ(filter.rate_conversion_ratio(), 2.0);
}

TEST(LinearFilterTest, ComputeSample) {
  auto source_rate = 48000;
  auto dest_rate = 48000;
  int32_t num_frac_bits = 4;
  LinearFilter filter(source_rate, dest_rate, num_frac_bits);

  auto frac_size = 1 << num_frac_bits;
  auto frac_half = frac_size >> 1;
  auto frac_quarter = frac_half >> 1;
  float data[] = {0.0f, 1.0f, 2.0f, 3.0f, 4.0f, 5.0f};

  EXPECT_FLOAT_EQ(filter.ComputeSample(0, &data[1]), data[1]);
  EXPECT_FLOAT_EQ(filter.ComputeSample(frac_half, &data[2]), (data[2] + data[3]) / 2.0f);
  EXPECT_FLOAT_EQ(filter.ComputeSample(frac_half + frac_quarter, &data[3]),
                  (data[3] + 3.0f * data[4]) / 4.0f);
}

void ValidateSincComputeSample(int32_t source_rate, int32_t dest_rate, int64_t side_length,
                               int32_t num_frac_bits) {
  SincFilter filter(source_rate, dest_rate, side_length, num_frac_bits);

  // If values outside indices [1,33] are used in `ComputeSample`, data compares will fail.
  float data[] = {
      999999.0f,                                             //
      0.1f,       0.2f, 0.3f, 0.4f, 0.5f, 0.6f, 0.7f, 0.8f,  //
      0.9f,       1.0f, 1.1f, 1.2f, 1.3f, 1.4f, 1.5f, 1.6f,  //
      1.7f,                                                  //
      1.8f,       1.9f, 2.f,  2.1f, 2.2f, 2.3f, 2.4f, 2.5f,  //
      2.6f,       2.7f, 2.8f, 2.9f, 3.0f, 3.1f, 3.2f, 3.3f,  //
      -999999.0f,
  };
  auto frac_size = 1 << num_frac_bits;
  auto frac_half = frac_size >> 1;
  auto frac_quarter = frac_half >> 1;
  auto frac_three_quarters = frac_size - frac_quarter;

  // These values should be precisely equal
  SCOPED_TRACE("Compute(17.0) == [17]");
  EXPECT_FLOAT_EQ(filter.ComputeSample(0, &data[17]), data[17]);

  // These values are only calculated to a specific quality tolerance (related to `side_length` and
  // `num_frac_bits`), so the only SAFE things to do here are rough comparisons.
  SCOPED_TRACE("[17] < Compute(17.25)");
  EXPECT_LT(data[17], filter.ComputeSample(frac_half, &data[17]));

  SCOPED_TRACE("Compare 16.5 < 16.75");
  EXPECT_LT(filter.ComputeSample(frac_half, &data[16]),
            filter.ComputeSample(frac_three_quarters, &data[16]));

  SCOPED_TRACE("Compare 17.25 < 17.5");
  EXPECT_LT(filter.ComputeSample(frac_quarter, &data[17]),
            filter.ComputeSample(frac_half, &data[17]));
}

TEST(SincFilterTest, ComputeSample) {
  // Unity rate ratio
  auto source_rate = 48000;
  auto dest_rate = 48000;
  auto num_taps = 2;
  int32_t num_frac_bits = 2;
  auto side_length = (num_taps + 1) << num_frac_bits;
  ValidateSincComputeSample(source_rate, dest_rate, side_length, num_frac_bits);

  // Up-sampling rate ratio
  source_rate = 24000;
  dest_rate = 48000;
  num_taps = 3;
  num_frac_bits = 2;
  side_length = (num_taps + 1) << num_frac_bits;
  ValidateSincComputeSample(source_rate, dest_rate, side_length, num_frac_bits);

  // Down-sampling rate ratio
  source_rate = 48000;
  dest_rate = 24000;
  num_taps = 3;
  num_frac_bits = 3;
  side_length = ((num_taps + 1) << num_frac_bits) * 2;
  ValidateSincComputeSample(source_rate, dest_rate, side_length, num_frac_bits);

  source_rate = 148500;  // rates don't change results calculated in ValidateSincComputeSample
  dest_rate = 36000;
  num_taps = 3;
  num_frac_bits = 3;

  // Width is chosen to be non-integral and to reference a final data element that is right at the
  // edge of the data populated in ValidateSincComputeSample (downsampling of about 4.125:1).
  side_length = 132;
  ValidateSincComputeSample(source_rate, dest_rate, side_length, num_frac_bits);
}

double MeasureFilterMagnitude(SincFilter& filter, int64_t frac_offset, double freq_hz,
                              double sample_rate = 96000.0) {
  constexpr int32_t kCenter = 50;
  constexpr int32_t kSize = 101;
  float cos_buf[kSize];
  float sin_buf[kSize];
  double omega = 2.0 * M_PI * freq_hz / sample_rate;
  for (int32_t i = 0; i < kSize; ++i) {
    int32_t n = i - kCenter;
    cos_buf[i] = static_cast<float>(std::cos(omega * static_cast<double>(n)));
    sin_buf[i] = static_cast<float>(std::sin(omega * static_cast<double>(n)));
  }
  double y_cos = filter.ComputeSample(frac_offset, &cos_buf[kCenter]);
  double y_sin = filter.ComputeSample(frac_offset, &sin_buf[kCenter]);
  return std::sqrt(y_cos * y_cos + y_sin * y_sin);
}

// Verifies HighPass SincFilter length, >43.9 dB stopband attenuation, and 26-31 kHz passband.
TEST(SincFilterTest, HighPassFrequencyResponse) {
  EXPECT_EQ(SincFilter::Length(96000, 96000, /*high_pass=*/true),
            Fixed::FromRaw(SincFilter::kFracHighPassSideLength));

  SincFilter filter(96000, 96000, SincFilter::kFracHighPassSideLength,
                    Fixed::Format::FractionalBits, /*high_pass=*/true);
  EXPECT_TRUE(filter.high_pass());
  EXPECT_EQ(filter.side_length(), SincFilter::kFracHighPassSideLength);

  const std::vector<int64_t> test_alphas = {0, 1, 2048, 4096, 8191};
  for (auto alpha : test_alphas) {
    SCOPED_TRACE(testing::Message() << "alpha = " << alpha);

    // DC (0 Hz)
    EXPECT_LT(MeasureFilterMagnitude(filter, alpha, 0.0), 2e-5);

    // Deep & mid audible stopband
    EXPECT_LT(MeasureFilterMagnitude(filter, alpha, 1000.0), 2e-5);
    EXPECT_LT(MeasureFilterMagnitude(filter, alpha, 10000.0), 5e-5);
    EXPECT_LT(MeasureFilterMagnitude(filter, alpha, 18000.0), 0.002);

    // Upper audible stopband
    EXPECT_LT(MeasureFilterMagnitude(filter, alpha, 19500.0), 0.0065);
    EXPECT_LT(MeasureFilterMagnitude(filter, alpha, 20000.0), 0.0035);

    // Cutoff (23 kHz)
    auto mag_23k = MeasureFilterMagnitude(filter, alpha, 23000.0);
    EXPECT_GE(mag_23k, 0.499);
    EXPECT_LE(mag_23k, 0.501);

    // Ultrasonic passband
    for (double freq : {26000.0, 31000.0, 36000.0, 40000.0}) {
      auto mag = MeasureFilterMagnitude(filter, alpha, freq);
      EXPECT_GE(mag, 0.992) << "freq = " << freq;
      EXPECT_LE(mag, 0.008 + 1.0) << "freq = " << freq;
    }
  }
}

// Verifies LowPass SincFilter length, audible passband ripple, and >43.9 dB ultrasonic stopband.
TEST(SincFilterTest, LowPassFrequencyResponse) {
  EXPECT_EQ(SincFilter::Length(96000, 96000, /*high_pass=*/false, /*low_pass=*/true),
            Fixed::FromRaw(SincFilter::kFracHighPassSideLength));
  EXPECT_EQ(SincFilter::Length(96000, 48000, /*high_pass=*/false, /*low_pass=*/true),
            Fixed::FromRaw(SincFilter::kFracHighPassSideLength));
  EXPECT_EQ(SincFilter::Length(96000, 16000, /*high_pass=*/false, /*low_pass=*/true),
            SincFilter::Length(96000, 16000));

  SincFilter filter(96000, 96000, SincFilter::kFracHighPassSideLength,
                    Fixed::Format::FractionalBits, /*high_pass=*/false, /*low_pass=*/true);
  EXPECT_FALSE(filter.high_pass());
  EXPECT_TRUE(filter.low_pass());
  EXPECT_EQ(filter.side_length(), SincFilter::kFracHighPassSideLength);

  const std::vector<int64_t> test_alphas = {0, 1, 2048, 4096, 8191};
  for (auto alpha : test_alphas) {
    SCOPED_TRACE(testing::Message() << "alpha = " << alpha);

    // Audible passband (DC through 20 kHz)
    for (double freq : {0.0, 1000.0, 10000.0, 18000.0, 19500.0, 20000.0}) {
      auto mag = MeasureFilterMagnitude(filter, alpha, freq);
      EXPECT_GE(mag, 0.992) << "freq = " << freq;
      EXPECT_LE(mag, 1.008) << "freq = " << freq;
    }

    // Cutoff (23 kHz)
    auto mag_23k = MeasureFilterMagnitude(filter, alpha, 23000.0);
    EXPECT_GE(mag_23k, 0.499);
    EXPECT_LE(mag_23k, 0.501);

    // Ultrasonic stopband (>= 26 kHz)
    EXPECT_LT(MeasureFilterMagnitude(filter, alpha, 26000.0), 0.0035);
    EXPECT_LT(MeasureFilterMagnitude(filter, alpha, 26500.0), 0.0065);
    EXPECT_LT(MeasureFilterMagnitude(filter, alpha, 28000.0), 0.002);
    EXPECT_LT(MeasureFilterMagnitude(filter, alpha, 36000.0), 5e-5);
    EXPECT_LT(MeasureFilterMagnitude(filter, alpha, 40000.0), 1e-4);
    EXPECT_LT(MeasureFilterMagnitude(filter, alpha, 45000.0), 5e-5);
  }
}

}  // namespace
}  // namespace media_audio
