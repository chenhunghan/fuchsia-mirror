// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// A synthetic dispatcher used to test babeldiff. It is not part of Zircon.

#include "object/doorbell_dispatcher.h"

DoorbellDispatcher::DoorbellDispatcher() = default;

DoorbellDispatcher::~DoorbellDispatcher() = default;

uint64_t DoorbellDispatcher::RingCount() const { return rust_doorbell_dispatcher_ring_count(this); }

// static
zx_status_t ChimeDoorbellDispatcher::Create(uint32_t notes,
                                            KernelHandle<DoorbellDispatcher>* handle,
                                            zx_rights_t* rights) {
  return rust_chime_doorbell_dispatcher_create(notes, handle, rights);
}

ChimeDoorbellDispatcher::ChimeDoorbellDispatcher(uint32_t notes) : notes_(notes + 1) {}

zx_status_t ChimeDoorbellDispatcher::Ring(uint32_t tone) {
  return rust_doorbell_dispatcher_ring(this, tone);
}

// static
zx_status_t BuzzerDoorbellDispatcher::Create(zx_duration_t length,
                                             KernelHandle<DoorbellDispatcher>* handle,
                                             zx_rights_t* rights) {
  return rust_buzzer_doorbell_dispatcher_create(length, handle, rights);
}

BuzzerDoorbellDispatcher::BuzzerDoorbellDispatcher(zx_duration_t length) : length_(length) {}

zx_status_t BuzzerDoorbellDispatcher::Ring(uint32_t tone) {
  return rust_doorbell_dispatcher_ring(this, tone);
}
