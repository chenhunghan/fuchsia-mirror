// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_LIB_PROCESSING_COEFFICIENT_TABLE_H_
#define SRC_MEDIA_AUDIO_LIB_PROCESSING_COEFFICIENT_TABLE_H_

#include <lib/stdcompat/span.h>

#include <algorithm>
#include <cmath>
#include <memory>
#include <optional>
#include <tuple>
#include <utility>
#include <vector>

#include "src/media/audio/lib/format2/fixed.h"

// `coefficient_table.h` is included by `gen_coefficient_tables.cc`, which does not have access to
// Fuchsia headers because it is compiled as a host binary.
#ifndef BUILDING_FUCHSIA_AUDIO_HOST_TOOL
#include <lib/syslog/cpp/macros.h>  // nogncheck
#else
#include <cassert>
#define FX_CHECK(cond) assert(cond)
#endif

namespace media_audio {

// Half-amplitude (-6.02 dB) cutoff frequency for the ultrasound high-pass filter.
// With a 28-frame side_length Hann-windowed sinc filter at 96 kHz, the transition
// band spans 20 kHz to 26 kHz, achieving > 43.9 dB stopband attenuation at <= 20 kHz
// (> 50 dB at <= 19 kHz) and unity passband (+-0.06 dB) at >= 26 kHz.
inline constexpr int32_t kUltrasoundMinFrequencyHz = 23000;

// Half-amplitude (-6.02 dB) cutoff frequency ceiling for the audio low-pass filter.
// With a 28-frame side_length Hann-windowed sinc filter at 96 kHz, the transition
// band spans 20 kHz to 26 kHz, achieving > 43.9 dB stopband attenuation at >= 26 kHz
// (> 50 dB at >= 27 kHz).
inline constexpr int32_t kAudioMaxFrequencyHz = 23000;

// `CoefficientTable` is a shim around `std::vector` that maps indices into a physical addressing
// scheme that is most optimal with respect to how this table is typically accessed. More
// specifically, they are most commonly accessed with an integral stride (that is `1 << frac_bits`
// stride). We optimize for this use case by placing these values physically contiguously in memory.
//
// Coefficient tables represent one side of a symmetric convolution filter. Coefficients cover the
// entire discrete space of fractional position values, but for any calculation we reference only a
// small subset of these values (see `ReadSlice` below for an example).
class CoefficientTable {
 public:
  // `width` is the filter width of this table, in fixed point format with `frac_bits` of fractional
  // precision. The `width` will determine the number of entries in the table, which will be `width`
  // rounded up to the nearest integer in the same fixed-point format. `data` provides the raw table
  // data ordered by physical address. If `data` is empty, storage is allocated automatically.
  CoefficientTable(int64_t width, int32_t frac_bits, cpp20::span<const float> data)
      : stride_(ComputeStride(width, frac_bits)),
        frac_filter_width_(width),
        frac_bits_(frac_bits),
        frac_mask_((1 << frac_bits_) - 1),
        storage_(data.empty() ? std::make_optional<std::vector<float>>(stride_ * (1 << frac_bits))
                              : std::nullopt),
        table_(data.empty() ? cpp20::span<const float>(storage_->begin(), storage_->end()) : data) {
    FX_CHECK(frac_filter_width_ >= 0);
    FX_CHECK(static_cast<int64_t>(table_.size()) == stride_ * (1 << frac_bits));
  }

  const float& operator[](int64_t offset) const { return table_[PhysicalIndex(offset)]; }

  // Reads `num_coefficients` coefficients starting at `offset`. The result is a pointer to
  // `num_coefficients` coefficients with the following semantics:
  //
  // ```
  // auto c = new CoefficientTable(width, frac_bits);
  // auto f = c->ReadSlice(offset, size);
  // ASSERT_EQ(f[0], c[off + 0 << frac_bits]);
  // ASSERT_EQ(f[1], c[off + 1 << frac_bits]);
  //  ...
  // ASSERT_EQ(f[size], c[off + size << frac_bits]);
  // ```
  const float* ReadSlice(int64_t offset, int64_t num_coefficients) const {
    if (num_coefficients <= 0 ||
        offset + ((num_coefficients - 1) << frac_bits_) > frac_filter_width_) {
      return nullptr;
    }

    // The underlying table already stores these consecutively.
    return &table_[PhysicalIndex(offset)];
  }

  // Returns the raw table in physical (not logical) order.
  cpp20::span<const float> raw_table() const { return table_; }

  // Returns the physical index corresponding to the given logical index.
  size_t PhysicalIndex(int64_t offset) const {
    auto integer = offset >> frac_bits_;
    auto fraction = offset & frac_mask_;
    return fraction * stride_ + integer;
  }

 private:
  friend class CoefficientTableBuilder;
  friend class CoefficientTableTest;

  static int64_t ComputeStride(int64_t filter_width, int32_t frac_bits) {
    return (filter_width + ((1 << frac_bits) - 1)) / (1 << frac_bits);
  }

  const int64_t stride_;
  const int64_t frac_filter_width_;
  const int32_t frac_bits_;
  const int64_t frac_mask_;

  // The table_ can reference the storage vector storage_ or an externally-allocated array,
  // such as an array allocated in .rodata.
  std::optional<std::vector<float>> storage_;
  cpp20::span<const float> table_;
};

// `CoefficientTableBuilder` constructs a single `CoefficientTable`.
// Once constructed, the `CoefficientTable` is immutable.
class CoefficientTableBuilder {
 public:
  CoefficientTableBuilder(int64_t width, int32_t frac_bits)
      : table_(std::make_unique<CoefficientTable>(width, frac_bits, cpp20::span<const float>{})) {}

  float& operator[](int64_t offset) { return (*table_->storage_)[table_->PhysicalIndex(offset)]; }

  auto physical_index_begin() { return table_->storage_->begin(); }
  auto physical_index_end() { return table_->storage_->end(); }
  size_t size() const { return table_->storage_->size(); }

  std::unique_ptr<CoefficientTable> Build() { return std::move(table_); }

 private:
  std::unique_ptr<CoefficientTable> table_;
};

// Linear interpolation, implemented using the convolution filter.
// Length on both sides is one frame, modulo the stretching effects of downsampling.
//
// Example: for `frac_size` 1000, `filter_length` would be 999, entailing coefficient values for
// locations from that exact position, up to positions as much as 999 away. This means:
// -Fractional source pos 1.999 requires frames between 1.000 and 2.998, thus source frames 1 and 2
// -Fractional source pos 2.001 requires frames between 1.002 and 3.000, thus source frames 2 and 3
// -Fractional source pos 2.000 requires frames between 1.001 and 2.999, thus source frame 2 only
//  (Restated: source pos N.000 requires frame N only; no need to interpolate with neighbors.)
class LinearFilterCoefficientTable {
 public:
  struct Inputs {
    int64_t side_length;
    int32_t num_frac_bits;

    bool operator<(const Inputs& rhs) const {
      return std::tie(side_length, num_frac_bits) < std::tie(rhs.side_length, rhs.num_frac_bits);
    }
  };

  // Creates linear-interpolation filter with frame-rate conversion.
  static std::unique_ptr<CoefficientTable> Create(Inputs);
};

// "Fractional-delay" sinc-based resampler with integrated low-pass or unity-rate high-pass filter.
class SincFilterCoefficientTable {
 public:
  static constexpr int32_t kSideTaps = 13;
  static constexpr int64_t kFracSideLength = (kSideTaps + 1) << Fixed::Format::FractionalBits;
  static constexpr int32_t kHighPassSideTaps = 27;
  static constexpr int64_t kFracHighPassSideLength = (kHighPassSideTaps + 1)
                                                     << Fixed::Format::FractionalBits;

  // 27.5:1 allows 192 KHz to be downsampled to 6980 Hz with all taps engaged (i.e. at full
  // quality). It also allows 192:1 downsampling filters to have at least 2 tap lengths of quality.
  static constexpr double kMaxDownsampleRatioForFullSideTaps = 27.5;
  static constexpr int64_t kMaxFracSideLength = static_cast<int64_t>(
      kMaxDownsampleRatioForFullSideTaps * static_cast<double>(kFracSideLength));
  static_assert(kMaxFracSideLength > kFracSideLength,
                "kMaxFracSideLength cannot be less than kFracSideLength");

  struct Inputs {
    int64_t side_length;
    int32_t num_frac_bits;
    double rate_conversion_ratio;
    bool high_pass = false;

    bool operator<(const Inputs& rhs) const {
      return std::tie(side_length, num_frac_bits, rate_conversion_ratio, high_pass) <
             std::tie(rhs.side_length, rhs.num_frac_bits, rhs.rate_conversion_ratio, rhs.high_pass);
    }
  };

  static Fixed Length(int32_t source_frame_rate, int32_t dest_frame_rate, bool high_pass = false,
                      bool low_pass = false) {
    FX_CHECK(!(high_pass && low_pass));
    if (high_pass) {
      FX_CHECK(source_frame_rate == dest_frame_rate);
      return Fixed::FromRaw(kFracHighPassSideLength);
    }
    if (low_pass) {
      FX_CHECK(source_frame_rate > 2 * kAudioMaxFrequencyHz);
      if (dest_frame_rate > 2 * kAudioMaxFrequencyHz) {
        return std::max(Fixed::FromRaw(kFracHighPassSideLength), Length(source_frame_rate, 48000));
      }
    }
    int64_t filter_length = kFracSideLength;
    if (source_frame_rate > dest_frame_rate) {
      filter_length =
          static_cast<int64_t>(std::ceil(static_cast<double>(filter_length * source_frame_rate) /
                                         static_cast<double>(dest_frame_rate)));

      // For down-sampling ratios beyond `kMaxDownsampleRatioForFullSideTaps` the effective number
      // of side taps decreases proportionally -- rate-conversion quality gracefully degrades.
      filter_length = std::min(filter_length, kMaxFracSideLength);
    }

    return Fixed::FromRaw(filter_length);
  }

  static Inputs MakeInputs(int32_t source_rate, int32_t dest_rate) {
    return Inputs{
        .side_length = Length(source_rate, dest_rate).raw_value(),
        .num_frac_bits = kPtsFractionalBits,
        .rate_conversion_ratio = static_cast<double>(dest_rate) / static_cast<double>(source_rate),
        .high_pass = false,
    };
  }

  static Inputs MakeHighPassInputs(int32_t source_rate,
                                   int32_t cutoff_hz = kUltrasoundMinFrequencyHz,
                                   int64_t side_length = kFracHighPassSideLength,
                                   int32_t num_frac_bits = kPtsFractionalBits) {
    FX_CHECK(cutoff_hz > 0 && 2 * static_cast<int64_t>(cutoff_hz) < source_rate);
    return Inputs{
        .side_length = side_length,
        .num_frac_bits = num_frac_bits,
        .rate_conversion_ratio =
            2.0 * static_cast<double>(cutoff_hz) / static_cast<double>(source_rate),
        .high_pass = true,
    };
  }

  // Creates inputs for a low-pass windowed-sinc filter. For dest rates above 2 * cutoff_hz
  // (e.g. 96 kHz -> 96 kHz or 96 kHz -> 48 kHz), this clamps bandwidth to cutoff_hz (23 kHz).
  // For dest rates <= 2 * cutoff_hz (e.g. 16 kHz), the dest Nyquist rate governs downsampling.
  static Inputs MakeLowPassInputs(int32_t source_rate, int32_t dest_rate,
                                  int32_t cutoff_hz = kAudioMaxFrequencyHz,
                                  std::optional<int64_t> side_length = std::nullopt,
                                  int32_t num_frac_bits = kPtsFractionalBits) {
    FX_CHECK(cutoff_hz > 0 && 2 * static_cast<int64_t>(cutoff_hz) < source_rate);
    const double effective_dest_rate =
        std::min(static_cast<double>(dest_rate), 2.0 * static_cast<double>(cutoff_hz));
    return Inputs{
        .side_length = side_length.value_or(Length(source_rate, dest_rate, /*high_pass=*/false,
                                                   /*low_pass=*/true)
                                                .raw_value()),
        .num_frac_bits = num_frac_bits,
        .rate_conversion_ratio = effective_dest_rate / static_cast<double>(source_rate),
        .high_pass = false,
    };
  }

  // Creates a low-pass or high-pass windowed-sinc FIR filter.
  static std::unique_ptr<CoefficientTable> Create(Inputs);
};

// This global struct describes a set of prebuilt coefficient tables.
struct PrebuiltSincFilterCoefficientTable {
  int32_t source_rate;
  int32_t dest_rate;
  cpp20::span<const float> table;
  bool high_pass = false;
  bool low_pass = false;
};

// The list of prebuilt coefficient tables.
// This uses `std::array` so it can directly reference data in `.rodata` without reallocating.
extern const cpp20::span<const PrebuiltSincFilterCoefficientTable>
    kPrebuiltSincFilterCoefficientTables;

}  // namespace media_audio

#endif  // SRC_MEDIA_AUDIO_LIB_PROCESSING_COEFFICIENT_TABLE_H_
