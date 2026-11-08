// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_CORESIGHT_INCLUDE_DEV_CORESIGHT_ROM_TABLE_H_
#define ZIRCON_KERNEL_DEV_CORESIGHT_INCLUDE_DEV_CORESIGHT_ROM_TABLE_H_

#include <lib/fit/result.h>
#include <zircon/assert.h>

#include <cstddef>
#include <cstdint>
#include <string_view>
#include <type_traits>

#include <dev/coresight/component.h>
#include <hwreg/mmio.h>

extern "C" {
using RustCoresightWalkCallback = void (*)(void*, uint32_t);

bool rust_coresight_rom_table_walk(const uint32_t* base, uint32_t max_offset, void* cb_ctx,
                                   RustCoresightWalkCallback callback,
                                   const char** out_error_reason_ptr, size_t* out_error_reason_len,
                                   uint32_t* out_error_offset);
}

namespace coresight {

// [CS] D5
// A ROM table is a basic CoreSight component that provides pointers to other
// components (including other ROM tables) in its lower registers via offsets
// from its base address. It is an organizational structure that can be used to
// find all CoreSight components - possibly as well as legacy or
// vendor-specific ones - on an SoC. Thought of as a tree, the leaves are the
// system's CoreSight components and the root is typically referred to as the
// "base ROM table" (or, more plainly, "the ROM table").
class RomTable {
 public:
  // Represents an error occurred while walking the table.
  struct WalkError {
    std::string_view reason;
    uint32_t offset{};
  };

  // Walks the underlying tree of components with no dynamic allocation,
  // calling `callback` on the offset from the table's base address (implicitly
  // encoded in `io`) of each component found. The (`io`, `max_offset`)
  // together implicitly give the aperture to walk.
  //
  // The walk will visit and access the first page of memory of each found
  // component. Unfortunately, however, there is no canonical means to
  // determine how large a region of memory this entails. The determination of
  // the maximum visited offset - or at least something deemed large enough -
  // is left to the caller. The offset must at least be
  // `kMininumComponentSize`, which is the size of the base table proper.
  template <typename ComponentCallback>
  static fit::result<WalkError> Walk(hwreg::RegisterMmio io, uint32_t max_offset,
                                     ComponentCallback&& callback) {
    static_assert(std::is_invocable_v<ComponentCallback, uint32_t>);
    ZX_ASSERT(max_offset >= kMinimumComponentSize);
    auto cb = [](void* ctx, uint32_t offset) {
      auto* cb_ptr = static_cast<std::remove_reference_t<ComponentCallback>*>(ctx);
      (*cb_ptr)(offset);
    };
    const char* err_ptr = nullptr;
    size_t err_len = 0;
    uint32_t err_offset = 0;
    if (!rust_coresight_rom_table_walk(reinterpret_cast<const uint32_t*>(io.base()), max_offset,
                                       &callback, cb, &err_ptr, &err_len, &err_offset)) {
      return fit::error(WalkError{std::string_view(err_ptr, err_len), err_offset});
    }
    return fit::ok();
  }
};

}  // namespace coresight

#endif  // ZIRCON_KERNEL_DEV_CORESIGHT_INCLUDE_DEV_CORESIGHT_ROM_TABLE_H_
