// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/unittest/unittest.h>

#include <arch/hypervisor.h>

namespace {

bool vmlaunch_fail() {
  BEGIN_TEST;

  // Create a Guest object, which will both determine if VMX is supported, and
  // set up the CPU state correctly if so.
  auto guest = Guest::Create();
  if (guest.is_error() && guest.status_value() == ZX_ERR_NOT_SUPPORTED) {
    printf("VMX not supported. Skipping test.\n");
    return true;
  }
  ASSERT_TRUE(guest.is_ok());

  // Attempt to launch an empty VMCS state.
  //
  // The state is invalid and we haven't performed a "vmptrld" on it, so
  // the vmlaunch will fail. However, the pointers are all valid, so we
  // shouldn't fault, but gracefully return ZX_ERR_INTERNAL.
  VmxState state = {};
  interrupt_saved_state_t interrupt_state = arch_interrupt_save();
  auto result = vmx_enter(&state);
  arch_interrupt_restore(interrupt_state);
  EXPECT_EQ(result.status_value(), ZX_ERR_INTERNAL);

  END_TEST;
}

bool vpid_allocator() {
  BEGIN_TEST;

  VpidAllocator allocator;

  // VPIDs are allocated from [1, Guest::kMaxGuestVcpus], as VPID 0 is reserved for the host.
  bool allocated[Guest::kMaxGuestVcpus + 1] = {};
  for (size_t i = 0; i < Guest::kMaxGuestVcpus; i++) {
    auto vpid = allocator.TryAlloc();
    ASSERT_TRUE(vpid.is_ok());
    ASSERT_GE(*vpid, 1u);
    ASSERT_LE(*vpid, Guest::kMaxGuestVcpus);
    EXPECT_FALSE(allocated[*vpid]);
    allocated[*vpid] = true;
  }
  EXPECT_EQ(allocator.TryAlloc().status_value(), ZX_ERR_NO_RESOURCES);

  // VPID 0 is reserved for the host, so is never allocated and cannot be freed.
  EXPECT_EQ(allocator.Free(0u).status_value(), ZX_ERR_INVALID_ARGS);
  // Neither can a VPID beyond the maximum.
  EXPECT_EQ(allocator.Free(Guest::kMaxGuestVcpus + 1).status_value(), ZX_ERR_INVALID_ARGS);

  // A freed VPID can be allocated again, but not freed twice.
  EXPECT_TRUE(allocator.Free(1u).is_ok());
  EXPECT_EQ(allocator.Free(1u).status_value(), ZX_ERR_INVALID_ARGS);
  auto vpid = allocator.TryAlloc();
  ASSERT_TRUE(vpid.is_ok());
  EXPECT_EQ(*vpid, 1u);

  END_TEST;
}

}  // namespace

UNITTEST_START_TESTCASE(x86_vmx)
UNITTEST("Exercise the error path when a vmlaunch fails", vmlaunch_fail)
UNITTEST("Allocate and free VPIDs", vpid_allocator)
UNITTEST_END_TESTCASE(x86_vmx, "x86-vmx", "x86-specific VMX unit tests")
