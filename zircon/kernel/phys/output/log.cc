// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <phys/address-space/address-space.h>
#include <phys/output/log.h>

#include <ktl/enforce.h>

Log* gLog;

int Log::Write(ktl::string_view str) {
  // Use the remaining space in the existing buffer, or make more space.
  auto make_space = [this](size_t needed) -> ktl::span<char> {
    if (ktl::span space = buffer_chars_left(); space.size() >= needed) {
      return space;
    }

    // Expand (or initially allocate) the buffer if it's too small.
    // The buffer is always allocated in whole pages.
    const size_t expand_size = (needed + AddressSpace::kPageSize - 1) & -AddressSpace::kPageSize;
    fbl::AllocChecker ac;
    if (buffer_) {
      buffer_.Resize(ac, buffer_.size_bytes() + expand_size);
    } else {
      buffer_ = Allocation::New(ac, memalloc::Type::kPhysLog, expand_size, AddressSpace::kPageSize);
    }
    if (!ac.check()) {
      ZX_PANIC("failed to increase phys log from %#zx to %#zx bytes", buffer_.size_bytes(),
               buffer_.size_bytes() + expand_size);
    }
    return buffer_chars_left();
  };

  ktl::span<char> space = make_space(str.size());
  size_t copied = str.copy(space.data(), space.size());
  ZX_DEBUG_ASSERT(copied == str.size());
  size_ += copied;
  return static_cast<int>(copied);
}
