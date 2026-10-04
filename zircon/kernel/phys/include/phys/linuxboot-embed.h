// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_LINUXBOOT_EMBED_H_
#define ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_LINUXBOOT_EMBED_H_

#include <ktl/byte.h>
#include <ktl/span.h>

// Return any image found after the end of the link-time memory image.  Its
// size is the difference between the size embedded in the header (as perhaps
// edited by linuxboot-embed.py) and the link-time memory image size.  In an
// original binary image as linked, it will always be empty.
ktl::span<ktl::byte> GetLinuxbootEmbeddedImage();

// Return the original size of the whole memory image as defined at link time.
size_t GetLinuxbootLinkMemorySize();

// Return the size of the whole image embedded in the header, perhaps edited.
size_t GetLinuxbootHeaderMemorySize();

#endif  // ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_LINUXBOOT_EMBED_H_
