// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <assert.h>
#include <stddef.h>
#include <sys/types.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <arch/ops.h>
#include <arch/user_copy.h>
#include <arch/x86/user_copy_ffi.h>

static_assert(sizeof(X64CopyToFromUserRet) == 16, "X64CopyToFromUserRet has unexpected size");
static_assert(alignof(X64CopyToFromUserRet) == 8, "X64CopyToFromUserRet has unexpected alignment");

// FFI wrappers to help marshal x86-64 specific return args into UserCopyCaptureFaultsResult
// which contains a std::optional that is currently not possible to get across the language
// barrier.
UserCopyCaptureFaultsResult arch_copy_from_user_capture_faults(void* dst, const void* src,
                                                               size_t len, CopyContext context) {
  DEBUG_ASSERT(arch_num_spinlocks_held() == 0 || context == CopyContext::kBlockingNotAllowed);
  X64CopyToFromUserRet ret = rust_arch_copy_from_user_capture_faults(dst, src, len);
  if (ret.status == ZX_OK) {
    return UserCopyCaptureFaultsResult{ZX_OK};
  }
  // The rust copy routine returns OUT_OF_RANGE if the src/len is outside of user space.
  // The UserCopyCaptureFaultsResult contract still specifies ERR_INVALID_ARGS for both
  // cases, so convert it back to an ERR_INVALID_ARGS but with the std::optional<FaultInfo>
  // unset.
  if (ret.status == ZX_ERR_OUT_OF_RANGE) {
    return UserCopyCaptureFaultsResult{ZX_ERR_INVALID_ARGS};
  }
  return {ret.status, {ret.pf_va, ret.pf_flags}};
}

UserCopyCaptureFaultsResult arch_copy_to_user_capture_faults(void* dst, const void* src, size_t len,
                                                             CopyContext context) {
  DEBUG_ASSERT(arch_num_spinlocks_held() == 0 || context == CopyContext::kBlockingNotAllowed);
  X64CopyToFromUserRet ret = rust_arch_copy_to_user_capture_faults(dst, src, len);
  if (ret.status == ZX_OK) {
    return UserCopyCaptureFaultsResult{ZX_OK};
  }
  // The rust copy routine returns OUT_OF_RANGE if the dst/len is outside of user space.
  // The UserCopyCaptureFaultsResult contract still specifies ERR_INVALID_ARGS for both
  // cases, so convert it back to an ERR_INVALID_ARGS but with the std::optional<FaultInfo>
  // unset.
  if (ret.status == ZX_ERR_OUT_OF_RANGE) {
    return UserCopyCaptureFaultsResult{ZX_ERR_INVALID_ARGS};
  }
  return {ret.status, {ret.pf_va, ret.pf_flags}};
}
