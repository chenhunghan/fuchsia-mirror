// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "phys/linuxboot-embed.h"

#include <stddef.h>
#include <stdint.h>
#include <zircon/assert.h>
#include <zircon/compiler.h>

#include <ktl/memory.h>

extern "C" __LOCAL uint64_t __executable_start[];  // NOLINT(bugprone-reserved-identifier)
extern "C" __LOCAL std::byte _end[];

size_t GetLinuxbootLinkMemorySize() {
  return _end - reinterpret_cast<std::byte*>(__executable_start);
}

size_t GetLinuxbootHeaderMemorySize() { return __executable_start[2]; }

ktl::span<ktl::byte> GetLinuxbootEmbeddedImage() {
  const size_t link_size = GetLinuxbootLinkMemorySize();
  const size_t header_size = GetLinuxbootHeaderMemorySize();
  ZX_ASSERT_MSG(header_size >= link_size,
                "Linux boot header memory size %#zx < link-time size %#zx",  //
                header_size, link_size);
  ktl::span image{_end, header_size - link_size};
  if (!image.empty()) {
    // There is no guarantee _end itself will be aligned; it depends on what
    // variables the program has.  However, in the Linux world there is always
    // an expectation that an initrd / initramfs image is 4-byte aligned in
    // memory.  The image-pasting tool (linuxboot-embed.py or equivalent) will
    // have padded to 4-byte alignment before each concatenated image.
    void* ptr = image.data();
    size_t space = image.size_bytes();
    ZX_ASSERT_MSG(ktl::align(4, 1, ptr, space),
                  "Linux boot header memory size %#zx is %zu bytes larger"
                  "than link-time size %#zx but leaves no space for a"
                  " 4-byte aligned payload",
                  header_size, header_size - link_size, link_size);
    image = {static_cast<std::byte*>(ptr), space};
  }
  return image;
}
