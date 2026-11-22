// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <string.h>
#include <threads.h>
#include <zircon/process.h>
#include <zircon/syscalls.h>
#include <zircon/threads.h>

#include <array>

#include <gtest/gtest.h>

namespace {

// Must match the constants in perftest_trace_events_test.py.
constexpr char kThreadName[] = "nul_padded_thread";
constexpr size_t kThreadCount = 8;

// Sets `handle`'s ZX_PROP_NAME to `name` using a full ZX_MAX_NAME_LEN buffer,
// so the kernel receives the name followed by NUL padding.
zx_status_t SetNameWithNulPadding(zx_handle_t handle, const char* name) {
  char padded[ZX_MAX_NAME_LEN] = {};
  strncpy(padded, name, sizeof(padded) - 1);
  return zx_object_set_property(handle, ZX_PROP_NAME, padded, sizeof(padded));
}

int RenameSelf(void* arg) { return SetNameWithNulPadding(zx_thread_self(), kThreadName); }

}  // namespace

// Each object is renamed to the name it already has. Kernel object records in
// a trace carry no timestamp, so a trace reader may apply an object's creation
// record and its rename record in either order. Keeping the name unchanged
// makes the name a reader reports independent of that order.
TEST(SetNameHelper, RenameWithNulPaddedBuffer) {
  char process_name[ZX_MAX_NAME_LEN] = {};
  ASSERT_EQ(
      zx_object_get_property(zx_process_self(), ZX_PROP_NAME, process_name, sizeof(process_name)),
      ZX_OK);
  ASSERT_EQ(SetNameWithNulPadding(zx_process_self(), process_name), ZX_OK);

  std::array<thrd_t, kThreadCount> threads;
  for (thrd_t& thread : threads) {
    ASSERT_EQ(thrd_create_with_name(&thread, RenameSelf, nullptr, kThreadName), thrd_success);
  }
  for (thrd_t& thread : threads) {
    int rename_status = ZX_ERR_INTERNAL;
    ASSERT_EQ(thrd_join(thread, &rename_status), thrd_success);
    EXPECT_EQ(rename_status, ZX_OK);
  }
}
