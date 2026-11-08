// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_IOPORT_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_IOPORT_H_

#include <zircon/compiler.h>

#include <arch/x86/ioport_ffi.h>
#include <kernel/ffi.h>
#include <kernel/spinlock.h>

class IoBitmap {
 public:
  IoBitmap() { rust_io_bitmap_init(reinterpret_cast<ffi::Uninitialized<IoBitmap>*>(this)); }
  ~IoBitmap() { rust_io_bitmap_destroy(this); }
  IoBitmap(const IoBitmap&) = delete;
  IoBitmap& operator=(const IoBitmap&) = delete;

 private:
  using LockType = DECLARE_SPINLOCK(IoBitmap);
  static constexpr size_t kLockSize = (sizeof(LockType) + 7) & ~size_t{7};
  static constexpr size_t kStorageSize = kLockSize + sizeof(void*);

  [[maybe_unused]] OpaqueStorage<kStorageSize, 8> storage_{};
};

static_assert(sizeof(IoBitmap) ==
              ((sizeof(DECLARE_SPINLOCK(IoBitmap)) + 7) & ~size_t{7}) + sizeof(void*));
static_assert(alignof(IoBitmap) == 8);

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_IOPORT_H_
