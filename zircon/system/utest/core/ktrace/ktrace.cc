// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fit/defer.h>
#include <lib/fzl/vmo-mapper.h>
#include <lib/standalone-test/standalone.h>
#include <lib/zircon-internal/ktrace.h>
#include <lib/zx/event.h>
#include <lib/zx/resource.h>
#include <lib/zx/result.h>
#include <stddef.h>
#include <stdint.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/resource.h>
#include <zircon/types.h>

#include <utility>

#include <zxtest/zxtest.h>

namespace {

zx::resource GetSystemResourceWithBase(uint64_t base) {
  zx::unowned_resource system_resource = standalone::GetSystemResource();
  zx::result<zx::resource> result = standalone::GetSystemResourceWithBase(system_resource, base);
  ZX_ASSERT(result.is_ok());
  return std::move(result.value());
}

zx::resource GetTracingResource() { return GetSystemResourceWithBase(ZX_RSRC_SYSTEM_TRACING_BASE); }

zx::resource GetCpuResource() { return GetSystemResourceWithBase(ZX_RSRC_SYSTEM_CPU_BASE); }

TEST(Ktrace, InvalidHandle) {
  size_t actual = 0;
  uint8_t byte = 0;
  EXPECT_STATUS(zx_ktrace_read(ZX_HANDLE_INVALID, nullptr, 0, 0, &actual), ZX_ERR_BAD_HANDLE);
  EXPECT_STATUS(zx_ktrace_read(ZX_HANDLE_INVALID, &byte, 0, sizeof(byte), &actual),
                ZX_ERR_BAD_HANDLE);
  EXPECT_STATUS(zx_ktrace_control(ZX_HANDLE_INVALID, KTRACE_ACTION_STOP, 0, nullptr),
                ZX_ERR_BAD_HANDLE);
}

TEST(Ktrace, WrongHandleType) {
  zx::event event;
  ASSERT_OK(zx::event::create(0, &event));
  size_t actual = 0;
  EXPECT_STATUS(zx_ktrace_read(event.get(), nullptr, 0, 0, &actual), ZX_ERR_WRONG_TYPE);
  EXPECT_STATUS(zx_ktrace_control(event.get(), KTRACE_ACTION_STOP, 0, nullptr), ZX_ERR_WRONG_TYPE);
}

TEST(Ktrace, WrongResource) {
  zx::resource cpu_resource = GetCpuResource();
  size_t actual = 0;
  EXPECT_STATUS(zx_ktrace_read(cpu_resource.get(), nullptr, 0, 0, &actual), ZX_ERR_OUT_OF_RANGE);
  EXPECT_STATUS(zx_ktrace_control(cpu_resource.get(), KTRACE_ACTION_STOP, 0, nullptr),
                ZX_ERR_OUT_OF_RANGE);
}

TEST(Ktrace, ReadLengthValidationAndReadback) {
  zx::resource tracing_resource = GetTracingResource();

  size_t required_size = 0;
  ASSERT_OK(zx_ktrace_read(tracing_resource.get(), nullptr, 0, 0, &required_size));
  if (required_size == 0) {
    ZXTEST_SKIP("ktrace buffer is disabled (ktrace.bufsize=0)");
  }

  auto cleanup_tracing = fit::defer([&] {
    zx_ktrace_control(tracing_resource.get(), KTRACE_ACTION_STOP, 0, nullptr);
    zx_ktrace_control(tracing_resource.get(), KTRACE_ACTION_REWIND, 0, nullptr);
  });

  ASSERT_OK(zx_ktrace_control(tracing_resource.get(), KTRACE_ACTION_STOP, 0, nullptr));
  ASSERT_OK(zx_ktrace_control(tracing_resource.get(), KTRACE_ACTION_REWIND, 0, nullptr));
  ASSERT_OK(
      zx_ktrace_control(tracing_resource.get(), KTRACE_ACTION_START, KTRACE_GRP_ALL, nullptr));

  // Verify oversized and undersized lengths fail with ZX_ERR_INVALID_ARGS without panicking or
  // touching a 1-byte stack buffer (regression test for https://fxbug.dev/504710479).
  char small_buf[1] = {};
  size_t actual = 0;
  EXPECT_STATUS(zx_ktrace_read(tracing_resource.get(), small_buf, 0, 0x1'0000'0000ULL, &actual),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(zx_ktrace_read(tracing_resource.get(), small_buf, 0, 0x1'0000'0010ULL, &actual),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(zx_ktrace_read(tracing_resource.get(), small_buf, 0, SIZE_MAX, &actual),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(zx_ktrace_read(tracing_resource.get(), small_buf, 0, 0, &actual),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(zx_ktrace_read(tracing_resource.get(), small_buf, 0, required_size - 1, &actual),
                ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(small_buf[0], 0);

  const size_t page_size = zx_system_get_page_size();
  const size_t map_size = (required_size + page_size - 1) & ~(page_size - 1);
  fzl::VmoMapper mapper;
  ASSERT_OK(mapper.CreateAndMap(map_size, ZX_VM_PERM_READ | ZX_VM_PERM_WRITE));
  void* mapped_ptr = mapper.start();

  EXPECT_STATUS(zx_ktrace_read(tracing_resource.get(), mapped_ptr, 0, 0x1'0000'0000ULL, &actual),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(zx_ktrace_read(tracing_resource.get(), mapped_ptr, 0, 0x1'0000'0010ULL, &actual),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(zx_ktrace_read(tracing_resource.get(), mapped_ptr, 0, SIZE_MAX, &actual),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(zx_ktrace_read(tracing_resource.get(), mapped_ptr, 0, 0, &actual),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(zx_ktrace_read(tracing_resource.get(), mapped_ptr, 0, required_size - 1, &actual),
                ZX_ERR_INVALID_ARGS);

  ASSERT_OK(zx_ktrace_control(tracing_resource.get(), KTRACE_ACTION_STOP, 0, nullptr));
  ASSERT_OK(zx_ktrace_read(tracing_resource.get(), mapped_ptr, 0, required_size, &actual));
  EXPECT_GT(actual, 0u);
  EXPECT_LE(actual, required_size);

  ASSERT_OK(zx_ktrace_control(tracing_resource.get(), KTRACE_ACTION_REWIND, 0, nullptr));
  cleanup_tracing.cancel();
}

}  // namespace
