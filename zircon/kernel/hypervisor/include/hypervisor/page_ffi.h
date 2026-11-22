// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_PAGE_FFI_H_
#define ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_PAGE_FFI_H_

// TODO(https://fxbug.dev/572853263): Remove once all C++ callers are ported to Rust.

#include <stdint.h>
#include <zircon/types.h>

namespace hypervisor {
class Page;
}  // namespace hypervisor

extern "C" {

void rust_hypervisor_page_init(hypervisor::Page* page);
void rust_hypervisor_page_free(hypervisor::Page* page);
zx_status_t rust_hypervisor_page_alloc(hypervisor::Page* page, uint8_t fill);
void* rust_hypervisor_page_virtual_address(const hypervisor::Page* page);
zx_paddr_t rust_hypervisor_page_physical_address(const hypervisor::Page* page);
bool rust_hypervisor_page_is_allocated(const hypervisor::Page* page);

}  // extern "C"

#endif  // ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_PAGE_FFI_H_
