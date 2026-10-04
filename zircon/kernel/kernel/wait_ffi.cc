// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/kconcurrent/chainlock.h>
#include <lib/object-constants.h>

#include <kernel/wait.h>

// Layout contract between the C++ wait queue types and their Rust counterparts.
//
// These types are embedded by value in both C++ and Rust structures, so their
// sizes and alignments are single-sourced from <lib/object-constants.h> and
// asserted from both languages.  See //zircon/kernel/kernel/wait.rs for the
// matching Rust assertions.

static_assert(sizeof(ChainLock) == kChainLockSize, "ChainLock size mismatch");
static_assert(alignof(ChainLock) == kChainLockAlign, "ChainLock alignment mismatch");

static_assert(sizeof(WaitQueueCollection) == kWaitQueueCollectionSize,
              "WaitQueueCollection size mismatch");
static_assert(alignof(WaitQueueCollection) == kWaitQueueCollectionAlign,
              "WaitQueueCollection alignment mismatch");

static_assert(sizeof(WaitQueue) == kWaitQueueSize, "WaitQueue size mismatch");
static_assert(alignof(WaitQueue) == kWaitQueueAlign, "WaitQueue alignment mismatch");

// WaitQueue adds no members of its own; the Rust side models only one type.
static_assert(sizeof(WaitQueueBase) == sizeof(WaitQueue),
              "WaitQueueBase and WaitQueue must have identical layout");
