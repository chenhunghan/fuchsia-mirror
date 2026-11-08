// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_INPUT_LIB_HID_INPUT_REPORT_TEST_TEST_H_
#define SRC_UI_INPUT_LIB_HID_INPUT_REPORT_TEST_TEST_H_

#include <lib/fidl/cpp/wire/arena.h>

#include <cstring>
#include <memory>
#include <vector>

#include <zxtest/zxtest.h>

namespace hid_input_report {

// This is the static size of the buffer to allocate test
// descriptors. This was chosen heuristically, a single descriptor shouldn't
// really need to be larger than this size.
static constexpr size_t kFidlDescriptorBufferSize = 4096 * 2;
using TestDescriptorAllocator = fidl::Arena<kFidlDescriptorBufferSize>;

// This is the static size of the buffer to allocate test
// reports. This was chosen heuristically, a single report shouldn't
// really need to be larger than this size.
static constexpr size_t kFidlReportBufferSize = 4096 * 2;
using TestReportAllocator = fidl::Arena<kFidlReportBufferSize>;

// Custom FIDL arena that places trailing canary bytes immediately after the
// exact `item_size * count` byte range of each allocation to catch OOB writes.
class CanaryArena final : public fidl::AnyArena {
 public:
  static constexpr size_t kCanarySize = 32;
  static constexpr uint8_t kCanaryByte = 0xAA;

  CanaryArena() = default;
  CanaryArena(const CanaryArena&) = delete;
  CanaryArena& operator=(const CanaryArena&) = delete;

  ~CanaryArena() override {
    for (auto it = allocations_.rbegin(); it != allocations_.rend(); ++it) {
      if (it->destructor != nullptr) {
        it->destructor(it->data, it->count);
      }
    }
  }

  void VerifyNoOutOfBoundsWrites() const {
    uint8_t expected[kCanarySize];
    std::memset(expected, kCanaryByte, sizeof(expected));
    for (const Allocation& alloc : allocations_) {
      EXPECT_BYTES_EQ(expected, alloc.data + alloc.exact_bytes, kCanarySize);
    }
  }

 protected:
  uint8_t* Allocate(size_t item_size, size_t count,
                    void (*destructor_function)(uint8_t* data, size_t count)) override {
    const size_t exact_bytes = item_size * count;
    const size_t num_words = (exact_bytes + kCanarySize + sizeof(uint64_t) - 1) / sizeof(uint64_t);
    auto storage = std::make_unique<uint64_t[]>(num_words);
    auto* data = reinterpret_cast<uint8_t*>(storage.get());
    std::memset(data, kCanaryByte, num_words * sizeof(uint64_t));
    allocations_.push_back({std::move(storage), data, exact_bytes, count, destructor_function});
    return data;
  }

 private:
  struct Allocation {
    std::unique_ptr<uint64_t[]> storage;
    uint8_t* data;
    size_t exact_bytes;
    size_t count;
    void (*destructor)(uint8_t* data, size_t count);
  };
  std::vector<Allocation> allocations_;
};

}  // namespace hid_input_report

#endif  // SRC_UI_INPUT_LIB_HID_INPUT_REPORT_TEST_TEST_H_
