// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_PAGE_H_
#define ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_PAGE_H_

#include <lib/zx/result.h>
#include <zircon/types.h>

#include <new>

#include <fbl/macros.h>
#include <hypervisor/page_ffi.h>
#include <kernel/ffi.h>

namespace hypervisor {

// This is a facade over the Rust `Page` in hypervisor/page.rs, which is stored in place in
// `storage_`.
class Page {
 public:
  Page() { rust_hypervisor_page_init(this); }
  DISALLOW_COPY_ASSIGN_AND_MOVE(Page);

  ~Page() { rust_hypervisor_page_free(this); }

  zx::result<> Alloc(uint8_t fill) {
    return zx::make_result(rust_hypervisor_page_alloc(this, fill));
  }

  void* VirtualAddress() const { return rust_hypervisor_page_virtual_address(this); }

  template <typename T>
  T* VirtualAddress() const {
    return static_cast<T*>(VirtualAddress());
  }

  zx_paddr_t PhysicalAddress() const { return rust_hypervisor_page_physical_address(this); }

  bool IsAllocated() const { return rust_hypervisor_page_is_allocated(this); }

 private:
  [[maybe_unused]] OpaqueStorage<16, 8> storage_{};
};
static_assert(sizeof(Page) == 16);
static_assert(alignof(Page) == 8);

template <typename T>
class PagePtr {
 public:
  zx::result<> Alloc() {
    auto result = page_.Alloc(0);
    if (result.is_error()) {
      return result.take_error();
    }

    ptr_ = page_.VirtualAddress<T>();
    new (ptr_) T;
    return zx::ok();
  }

  zx_paddr_t PhysicalAddress() const { return page_.PhysicalAddress(); }
  T* get() const { return ptr_; }
  T* operator->() const { return ptr_; }

 private:
  Page page_;
  T* ptr_;
};
static_assert(sizeof(PagePtr<uint8_t>) == 24);
static_assert(alignof(PagePtr<uint8_t>) == 8);

}  // namespace hypervisor

#endif  // ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_PAGE_H_
