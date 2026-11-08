// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/pci/drivers/pci/config.h"

#include <lib/fdf/cpp/arena.h>
#include <zircon/assert.h>

namespace pci {

namespace {
constexpr uint32_t kConfigArenaTag = 'PCFG';
}  // namespace

// Proxy Config Implementation
zx::result<std::unique_ptr<Config>> ProxyConfig::Create(
    fuchsia_hardware_pci::wire::Bdf bdf,
    const fdf::WireSyncClient<fuchsia_hardware_pci_root::Config>& client) {
  if (!client.is_valid()) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  // Can't use std::make_unique because the constructor is private.
  return zx::ok(std::unique_ptr<ProxyConfig>(new ProxyConfig(bdf, client)));
}

uint8_t ProxyConfig::Read8(uint16_t offset) const {
  fdf::Arena arena(kConfigArenaTag);
  const fdf::WireUnownedResult result = client_.buffer(arena)->Read8(bdf(), offset);
  ZX_ASSERT(result.ok());
  ZX_ASSERT(result->is_ok());
  return result->value()->value;
}

uint16_t ProxyConfig::Read16(uint16_t offset) const {
  fdf::Arena arena(kConfigArenaTag);
  const fdf::WireUnownedResult result = client_.buffer(arena)->Read16(bdf(), offset);
  ZX_ASSERT(result.ok());
  ZX_ASSERT(result->is_ok());
  return result->value()->value;
}

uint32_t ProxyConfig::Read32(uint16_t offset) const {
  fdf::Arena arena(kConfigArenaTag);
  const fdf::WireUnownedResult result = client_.buffer(arena)->Read32(bdf(), offset);
  ZX_ASSERT(result.ok());
  ZX_ASSERT(result->is_ok());
  return result->value()->value;
}

void ProxyConfig::Write8(uint8_t val, uint16_t offset) const {
  fdf::Arena arena(kConfigArenaTag);
  const fdf::WireUnownedResult result = client_.buffer(arena)->Write8(bdf(), offset, val);
  ZX_ASSERT(result.ok());
  ZX_ASSERT(result->is_ok());
}

void ProxyConfig::Write16(uint16_t val, uint16_t offset) const {
  fdf::Arena arena(kConfigArenaTag);
  const fdf::WireUnownedResult result = client_.buffer(arena)->Write16(bdf(), offset, val);
  ZX_ASSERT(result.ok());
  ZX_ASSERT(result->is_ok());
}

void ProxyConfig::Write32(uint32_t val, uint16_t offset) const {
  fdf::Arena arena(kConfigArenaTag);
  const fdf::WireUnownedResult result = client_.buffer(arena)->Write32(bdf(), offset, val);
  ZX_ASSERT(result.ok());
  ZX_ASSERT(result->is_ok());
}

const char* ProxyConfig::type() const { return "proxy"; }

}  // namespace pci
