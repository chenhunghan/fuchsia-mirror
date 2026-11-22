// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/memory_barriers/memory_barriers.h>

#include <zxtest/zxtest.h>

namespace {

TEST(MemoryBarriersTest, AllBarriers) {
  DataMemoryBarrier();
  ReadDataMemoryBarrier();
  WriteDataMemoryBarrier();
  DataSynchronizationBarrier();
  ReadDataSynchronizationBarrier();
  WriteDataSynchronizationBarrier();
  BarrierAfterFlush();
  BarrierBeforeInvalidate();
  BarrierBeforeRelease();
}

}  // namespace
