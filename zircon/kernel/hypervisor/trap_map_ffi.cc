// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <stddef.h>

#include <new>

#include <hypervisor/trap_map.h>
#include <kernel/ffi.h>

extern "C" {

zx_status_t rust_trap_queue(const hypervisor::Trap* trap, const zx_port_packet_t* packet);
bool rust_trap_has_port(const hypervisor::Trap* trap);
uint32_t rust_trap_kind(const hypervisor::Trap* trap);
uint64_t rust_trap_key(const hypervisor::Trap* trap);

void rust_trap_map_new(hypervisor::TrapMap* trap_map);
void rust_trap_map_destroy(hypervisor::TrapMap* trap_map);
zx_status_t rust_trap_map_insert_trap(hypervisor::TrapMap* trap_map, uint32_t kind,
                                      zx_gpaddr_t addr, size_t len, PortDispatcher* port,
                                      uint64_t key);
zx_status_t rust_trap_map_find_trap(const hypervisor::TrapMap* trap_map, uint32_t kind,
                                    zx_gpaddr_t addr, hypervisor::Trap** out);

}  // extern "C"

namespace hypervisor {

zx::result<> Trap::Queue(const zx_port_packet_t& packet) const {
  return zx::make_result(rust_trap_queue(this, &packet));
}

bool Trap::HasPort() const { return rust_trap_has_port(this); }

uint32_t Trap::kind() const { return rust_trap_kind(this); }

uint64_t Trap::key() const { return rust_trap_key(this); }

TrapMap::TrapMap() { rust_trap_map_new(this); }

TrapMap::~TrapMap() { rust_trap_map_destroy(this); }

zx::result<> TrapMap::InsertTrap(uint32_t kind, zx_gpaddr_t addr, size_t len,
                                 fbl::RefPtr<PortDispatcher> port, uint64_t key) {
  return zx::make_result(
      rust_trap_map_insert_trap(this, kind, addr, len, fbl::ExportToRawPtr(&port), key));
}

zx::result<Trap*> TrapMap::FindTrap(uint32_t kind, zx_gpaddr_t addr) const {
  Trap* out = nullptr;
  zx_status_t status = rust_trap_map_find_trap(this, kind, addr, &out);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(out);
}

}  // namespace hypervisor
