// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_TRAP_MAP_H_
#define ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_TRAP_MAP_H_

#include <zircon/syscalls/port.h>

#include <fbl/arena.h>
#include <fbl/ref_ptr.h>
#include <hypervisor/state_invalidator.h>
#include <kernel/ffi.h>
#include <kernel/mutex.h>
#include <kernel/spinlock.h>
#include <object/port_dispatcher.h>

namespace hypervisor {

class PortPacketArena {
 public:
  zx_status_t Init(const char* name, size_t ob_size,
                   size_t max_count) TA_NO_THREAD_SAFETY_ANALYSIS {
    return arena_.Init(name, ob_size, max_count);
  }

  void* Alloc() {
    mutex_.Acquire();
    void* addr = arena_.Alloc();
    mutex_.Release();
    return addr;
  }

  void Free(void* mem) {
    mutex_.Acquire();
    arena_.Free(mem);
    mutex_.Release();
  }

 private:
  Mutex mutex_;
  fbl::Arena arena_ TA_GUARDED(mutex_);
};

// Describes a single trap within a guest.
class Trap {
 public:
  Trap() = delete;
  ~Trap() = delete;
  Trap(const Trap&) = delete;
  Trap& operator=(const Trap&) = delete;

  zx::result<> Queue(const zx_port_packet_t& packet, StateInvalidator* invalidator = nullptr) const;
  bool HasPort() const;
  uint32_t kind() const;
  uint64_t key() const;
};

// Contains all the traps within a guest.
class TrapMap {
 public:
  TrapMap();
  ~TrapMap();
  TrapMap(const TrapMap&) = delete;
  TrapMap& operator=(const TrapMap&) = delete;

  zx::result<> InsertTrap(uint32_t kind, zx_gpaddr_t addr, size_t len,
                          fbl::RefPtr<PortDispatcher> port, uint64_t key);
  zx::result<Trap*> FindTrap(uint32_t kind, zx_gpaddr_t addr) const;

 private:
  using LockType = DECLARE_SPINLOCK(TrapMap);
  static constexpr size_t kLockSize = (sizeof(LockType) + 7) & ~size_t{7};
#ifdef ARCH_X86
  static constexpr size_t kStorageSize = kLockSize + 48;
#else
  static constexpr size_t kStorageSize = kLockSize + 24;
#endif

  [[maybe_unused]] OpaqueStorage<kStorageSize, 8> storage_{};
};

static_assert(alignof(TrapMap) == 8);

}  // namespace hypervisor

extern "C" {
void cpp_port_packet_arena_init(ffi::Uninitialized<hypervisor::PortPacketArena>* arena);
void cpp_port_packet_arena_destroy(hypervisor::PortPacketArena* arena);
zx_status_t cpp_port_packet_arena_init_pool(hypervisor::PortPacketArena* arena, const char* name,
                                            size_t ob_size, size_t max_count);
void* cpp_port_packet_arena_alloc(hypervisor::PortPacketArena* arena);
void cpp_port_packet_arena_free(hypervisor::PortPacketArena* arena, void* port_packet);
}

#endif  // ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_TRAP_MAP_H_
