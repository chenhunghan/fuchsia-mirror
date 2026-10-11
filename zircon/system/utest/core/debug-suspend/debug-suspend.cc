// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/standalone-test/standalone.h>
#include <lib/zx/resource.h>
#include <lib/zx/result.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/syscalls-next.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/resource.h>

#include <zxtest/zxtest.h>

#include "../needs-next.h"

NEEDS_NEXT_SYSCALL(zx_debug_suspend);
NEEDS_NEXT_SYSCALL(zx_debug_resume);

namespace {

zx::resource GetSystemResourceWithBase(uint64_t base) {
  zx::unowned_resource system_resource = standalone::GetSystemResource();
  zx::result<zx::resource> result = standalone::GetSystemResourceWithBase(system_resource, base);
  ZX_ASSERT(result.is_ok());
  return std::move(result.value());
}

zx::resource GetDebugResource() { return GetSystemResourceWithBase(ZX_RSRC_SYSTEM_DEBUG_BASE); }

TEST(DebugSuspend, NotSupported) {
  NEEDS_NEXT_SKIP(zx_debug_suspend);
  NEEDS_NEXT_SKIP(zx_debug_resume);
  zx::resource debug_resource = GetDebugResource();
  EXPECT_STATUS(zx_debug_suspend(debug_resource.get()), ZX_ERR_NOT_SUPPORTED);
  EXPECT_STATUS(zx_debug_resume(debug_resource.get()), ZX_ERR_NOT_SUPPORTED);
}

}  // namespace
