// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// A synthetic dispatcher used to test babeldiff. It is not part of Zircon.

#include "object/doorbell_dispatcher.h"

#include <trace.h>

#include <fbl/alloc_checker.h>

#define LOCAL_TRACE 0

constexpr uint32_t kMaxTone = 12;

DoorbellDispatcher::DoorbellDispatcher() = default;

DoorbellDispatcher::~DoorbellDispatcher() = default;

uint64_t DoorbellDispatcher::RingCount() const {
  Guard<CriticalMutex> guard{get_lock()};
  return rings_;
}

void DoorbellDispatcher::RecordRingLocked() {
  rings_++;
  UpdateStateLocked(0u, ZX_DOORBELL_RUNG);
}

// static
zx_status_t ChimeDoorbellDispatcher::Create(uint32_t notes,
                                            KernelHandle<DoorbellDispatcher>* handle,
                                            zx_rights_t* rights) {
  if (notes == 0) {
    return ZX_ERR_INVALID_ARGS;
  }
  fbl::AllocChecker ac;
  KernelHandle new_handle(fbl::AdoptRef(new (&ac) ChimeDoorbellDispatcher(notes)));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  *rights = default_rights();
  *handle = ktl::move(new_handle);
  return ZX_OK;
}

ChimeDoorbellDispatcher::ChimeDoorbellDispatcher(uint32_t notes) : notes_(notes) {}

zx_status_t ChimeDoorbellDispatcher::Ring(uint32_t tone) {
  if (tone > kMaxTone) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  LTRACEF("chime of %u notes, tone %u\n", notes_, tone);
  Guard<CriticalMutex> guard{get_lock()};
  // Each note of the chime counts as a ring.
  for (uint32_t i = 0; i < notes_; i++) {
    RecordRingLocked();
  }
  return ZX_OK;
}

// static
zx_status_t BuzzerDoorbellDispatcher::Create(zx_duration_t length,
                                             KernelHandle<DoorbellDispatcher>* handle,
                                             zx_rights_t* rights) {
  if (length <= 0) {
    return ZX_ERR_INVALID_ARGS;
  }
  fbl::AllocChecker ac;
  KernelHandle new_handle(fbl::AdoptRef(new (&ac) BuzzerDoorbellDispatcher(length)));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  *rights = default_rights();
  *handle = ktl::move(new_handle);
  return ZX_OK;
}

BuzzerDoorbellDispatcher::BuzzerDoorbellDispatcher(zx_duration_t length) : length_(length) {}

zx_status_t BuzzerDoorbellDispatcher::Ring(uint32_t tone) {
  if (tone > kMaxTone) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  Guard<CriticalMutex> guard{get_lock()};
  // A buzzer rings once, however long it buzzes.
  RecordRingLocked();
  return ZX_OK;
}
