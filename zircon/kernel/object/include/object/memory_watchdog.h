// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MEMORY_WATCHDOG_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MEMORY_WATCHDOG_H_

#include <lib/object-constants.h>
#include <stdint.h>
#include <zircon/types.h>

#include <fbl/ref_ptr.h>
#include <object/event_dispatcher.h>
#include <object/opaque_storage.h>

struct Thread;

// Object is thread safe.
// Delegates business logic to its Rust state stored in opaque_storage_.
class MemoryWatchdog {
 public:
  enum PressureLevel : uint8_t {
    kOutOfMemory = 0,
    kImminentOutOfMemory,
    kCritical,
    kWarning,
    kNormal,
    kNumLevels,
  };

  MemoryWatchdog();
  ~MemoryWatchdog();

  MemoryWatchdog(const MemoryWatchdog&) = delete;
  MemoryWatchdog& operator=(const MemoryWatchdog&) = delete;

  fbl::RefPtr<EventDispatcher> GetMemPressureEvent(uint32_t kind);

  uint64_t DebugNumBytesTillPressureLevel(PressureLevel level);
  void Dump();

  // Debug method to retrieve any current worker thread. Only to be used for testing / debugging
  // purposes. It is up to the caller to know if this objects is alive or not.
  Thread* DebugGetWorkerThread();

 private:
  OpaqueStorage<kMemoryWatchdogStateSize, kMemoryWatchdogStateAlign> opaque_storage_;
};

MemoryWatchdog& GetMemoryWatchdog();

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MEMORY_WATCHDOG_H_
