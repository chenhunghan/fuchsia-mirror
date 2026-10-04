// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_LIB_BOOT_SHIM_INCLUDE_LIB_BOOT_SHIM_BOOTLOADER_FILE_H_
#define ZIRCON_KERNEL_PHYS_LIB_BOOT_SHIM_INCLUDE_LIB_BOOT_SHIM_BOOTLOADER_FILE_H_

#include <lib/fit/result.h>
#include <lib/zbitl/item.h>
#include <lib/zbitl/items/bootloader-file.h>

#include "item-base.h"

namespace boot_shim {

// This extends the zbitl::BootloaderFile representation so it can be used as
// an item.  It can be constructed or assigned from a zbitl::BootloaderFile.
class BootloaderFileItem : public zbitl::BootloaderFile, public ItemBase {
 public:
  BootloaderFileItem(const BootloaderFileItem&) = default;

  explicit BootloaderFileItem(const zbitl::BootloaderFile& file) : zbitl::BootloaderFile{file} {}

  BootloaderFileItem& operator=(const BootloaderFileItem&) = default;

  BootloaderFileItem& operator=(const zbitl::BootloaderFile& file) {
    *static_cast<zbitl::BootloaderFile*>(this) = file;
    return *this;
  }

  constexpr size_t size_bytes() const {
    return *this ? zbitl::AlignedItemLength(static_cast<uint32_t>(payload_size())) : 0;
  }

  constexpr fit::result<DataZbi::Error> AppendItems(DataZbi& zbi) const {
    if (!*this) {
      return fit::ok();
    }
    auto result = zbi.Append(zbi_header_t{
        .type = ZBI_TYPE_BOOTLOADER_FILE,
        .length = static_cast<uint32_t>(payload_size()),
    });
    if (result.is_error()) {
      return result.take_error();
    }
    std::span<std::byte> bytes = result->payload;
    assert(bytes.size_bytes() == 1 + name.size() + contents.size_bytes());
    bytes.front() = static_cast<std::byte>(name.size());
    bytes = bytes.subspan<1>();
    bytes = bytes.subspan(name.copy(reinterpret_cast<char*>(bytes.data()), bytes.size_bytes()));
    assert(bytes.size_bytes() == contents.size_bytes());
    memcpy(bytes.data(), contents.data(), bytes.size_bytes());
    return fit::ok();
  }

 private:
  constexpr size_t payload_size() const {
    assert(!name.empty());
    return 1 + name.size() + contents.size_bytes();
  }
};

// This is a shorthand for multiple BootloaderFile items in one.  New items can
// be added with `.push_back(zbitl::BootloaderFile{name, data})`, etc.
template <size_t N>
using BootloaderFileItems = Multiple<BootloaderFileItem, N>;

}  // namespace boot_shim

#endif  // ZIRCON_KERNEL_PHYS_LIB_BOOT_SHIM_INCLUDE_LIB_BOOT_SHIM_BOOTLOADER_FILE_H_
