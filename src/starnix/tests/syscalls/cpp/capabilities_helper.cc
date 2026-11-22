// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"

#include <endian.h>
#include <fcntl.h>
#include <grp.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/xattr.h>
#include <unistd.h>

#include <charconv>
#include <filesystem>
#include <string>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <linux/capability.h>
#include <linux/prctl.h>

#include "src/lib/files/file.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace test_helper {

namespace {

int CapGet(__user_cap_data_struct (&caps)[_LINUX_CAPABILITY_U32S_3]) {
  __user_cap_header_struct header = {.version = _LINUX_CAPABILITY_VERSION_3, .pid = 0};
  return static_cast<int>(syscall(SYS_capget, &header, &caps));
}

int CapSet(const __user_cap_data_struct (&caps)[_LINUX_CAPABILITY_U32S_3]) {
  __user_cap_header_struct header = {.version = _LINUX_CAPABILITY_VERSION_3, .pid = 0};
  return static_cast<int>(syscall(SYS_capset, &header, &caps));
}

bool IsValidCap(int cap) { return cap >= 0 && CAP_TO_INDEX(cap) < _LINUX_CAPABILITY_U32S_3; }

bool HasCapabilityInSet(int cap, __u32 __user_cap_data_struct::*set) {
  if (!IsValidCap(cap)) {
    ADD_FAILURE() << "Invalid capability: " << cap;
    errno = EINVAL;
    return false;
  }
  __user_cap_data_struct caps[_LINUX_CAPABILITY_U32S_3] = {};
  if (CapGet(caps) < 0) {
    int err = errno;
    ADD_FAILURE() << "capget failed: " << strerror(err);
    errno = err;
    return false;
  }
  return (caps[CAP_TO_INDEX(cap)].*set & CAP_TO_MASK(cap)) != 0;
}

fit::result<int> ResultFromSyscall(int res) {
  if (res < 0) {
    return fit::error(errno);
  }
  return fit::ok();
}

fit::result<int> TryModifyCapabilityInSet(int cap, __u32 __user_cap_data_struct::*set,
                                          bool enable) {
  if (!IsValidCap(cap)) {
    return fit::error(EINVAL);
  }
  __user_cap_data_struct caps[_LINUX_CAPABILITY_U32S_3] = {};
  if (CapGet(caps) < 0) {
    return fit::error(errno);
  }
  if (enable) {
    caps[CAP_TO_INDEX(cap)].*set |= CAP_TO_MASK(cap);
  } else {
    caps[CAP_TO_INDEX(cap)].*set &= ~CAP_TO_MASK(cap);
  }
  return ResultFromSyscall(CapSet(caps));
}

}  // namespace

// Drops all capabilities from the effective, permitted, and inheritable sets.
void DropAllCapabilities() {
  __user_cap_data_struct caps[_LINUX_CAPABILITY_U32S_3] = {{0, 0, 0}};
  SAFE_SYSCALL(CapSet(caps));
}

// Checks if a capability is in the effective set.
bool HasCapability(int cap) { return HasCapabilityEffective(cap); }

// Unsets a capability from the effective set.
void UnsetCapability(int cap) { UnsetCapabilityEffective(cap); }

// Checks whether a capability is in the thread's effective set.
bool HasCapabilityEffective(int cap) {
  return HasCapabilityInSet(cap, &__user_cap_data_struct::effective);
}

// Checks whether a capability is in the thread's permitted set.
bool HasCapabilityPermitted(int cap) {
  return HasCapabilityInSet(cap, &__user_cap_data_struct::permitted);
}

// Checks whether a capability is in the thread's inheritable set.
bool HasCapabilityInheritable(int cap) {
  return HasCapabilityInSet(cap, &__user_cap_data_struct::inheritable);
}

bool HasCapabilityAmbient(int cap) {
  return SAFE_SYSCALL(prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, cap, 0, 0)) == 1;
}

bool HasCapabilityBounding(int cap) { return SAFE_SYSCALL(prctl(PR_CAPBSET_READ, cap)) == 1; }

fit::result<int> TryUnsetCapabilityPermitted(int cap) {
  return TryModifyCapabilityInSet(cap, &__user_cap_data_struct::permitted, /*enable=*/false);
}

fit::result<int> TrySetCapabilityEffective(int cap) {
  return TryModifyCapabilityInSet(cap, &__user_cap_data_struct::effective, /*enable=*/true);
}

fit::result<int> TrySetCapabilityPermitted(int cap) {
  return TryModifyCapabilityInSet(cap, &__user_cap_data_struct::permitted, /*enable=*/true);
}

fit::result<int> TrySetCapabilityAmbient(int cap) {
  return ResultFromSyscall(prctl(PR_CAP_AMBIENT, static_cast<unsigned long>(PR_CAP_AMBIENT_RAISE),
                                 static_cast<unsigned long>(cap), 0UL, 0UL));
}

// Removes a capability from the thread's effective set.
void UnsetCapabilityEffective(int cap) {
  ASSERT_THAT(TryModifyCapabilityInSet(cap, &__user_cap_data_struct::effective, /*enable=*/false),
              SyscallResultIsOk());
}

// Removes a capability from the thread's permitted set.
void UnsetCapabilityPermitted(int cap) {
  ASSERT_THAT(TryUnsetCapabilityPermitted(cap), SyscallResultIsOk());
}

// Removes a capability from the thread's inheritable set.
void UnsetCapabilityInheritable(int cap) {
  ASSERT_THAT(TryModifyCapabilityInSet(cap, &__user_cap_data_struct::inheritable, /*enable=*/false),
              SyscallResultIsOk());
}

// Removes a capability from the thread's ambient set.
void UnsetCapabilityAmbient(int cap) {
  ASSERT_THAT(prctl(PR_CAP_AMBIENT, static_cast<unsigned long>(PR_CAP_AMBIENT_LOWER),
                    static_cast<unsigned long>(cap), 0UL, 0UL),
              SyscallSucceeds());
}

// Removes a capability from the thread's bounding set.
void UnsetCapabilityBounding(int cap) {
  ASSERT_THAT(prctl(PR_CAPBSET_DROP, static_cast<unsigned long>(cap), 0UL, 0UL, 0UL),
              SyscallSucceeds());
}

// Sets a capability in the thread's effective set.
void SetCapabilityEffective(int cap) {
  ASSERT_THAT(TrySetCapabilityEffective(cap), SyscallResultIsOk());
}

// Sets a capability in the thread's permitted set.
void SetCapabilityPermitted(int cap) {
  ASSERT_THAT(TrySetCapabilityPermitted(cap), SyscallResultIsOk());
}

// Sets a capability in the thread's inheritable set.
void SetCapabilityInheritable(int cap) {
  ASSERT_THAT(TryModifyCapabilityInSet(cap, &__user_cap_data_struct::inheritable, /*enable=*/true),
              SyscallResultIsOk());
}

// Sets a capability in the thread's ambient set.
void SetCapabilityAmbient(int cap) {
  ASSERT_THAT(TrySetCapabilityAmbient(cap), SyscallResultIsOk());
}

void DropAllAmbientCapabilities() {
  SAFE_SYSCALL(prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0L, 0L, 0L));
}

int SetFileCapabilitiesV2(const std::filesystem::path &path, uint64_t permitted,
                          uint64_t inheritable, bool effective) {
  uint32_t magic = VFS_CAP_REVISION_2;
  if (effective) {
    magic |= VFS_CAP_FLAGS_EFFECTIVE;
  }

  vfs_cap_data cap_data{};
  cap_data.magic_etc = htole32(magic);
  cap_data.data[0].permitted = htole32(static_cast<uint32_t>(permitted));
  cap_data.data[0].inheritable = htole32(static_cast<uint32_t>(inheritable));
  cap_data.data[1].permitted = htole32(static_cast<uint32_t>(permitted >> 32u));
  cap_data.data[1].inheritable = htole32(static_cast<uint32_t>(inheritable >> 32u));
  return setxattr(path.c_str(), "security.capability", &cap_data, sizeof(cap_data), 0);
}

fit::result<int, int> GetLastCap() {
  fbl::unique_fd fd(HANDLE_EINTR(open("/proc/sys/kernel/cap_last_cap", O_RDONLY)));
  if (!fd.is_valid()) {
    return fit::error(errno);
  }
  std::string content;
  if (!files::ReadFileDescriptorToString(fd.get(), &content)) {
    return fit::error(errno);
  }
  int cap_last_cap = -1;
  const char *begin = content.data();
  const char *end = begin + content.size();
  auto [ptr, ec] = std::from_chars(begin, end, cap_last_cap);
  if (ec != std::errc() || ptr == begin || (ptr != end && *ptr != '\n') || cap_last_cap < 0) {
    return fit::error(EINVAL);
  }
  return fit::ok(cap_last_cap);
}

::testing::AssertionResult SwitchToUnprivilegedUser(uid_t uid, gid_t gid, bool keep_caps) {
  int ngroups = getgroups(0, nullptr);
  if (ngroups < 0) {
    int err = errno;
    return ::testing::AssertionFailure()
           << "getgroups(0, nullptr) failed: " << strerror(err) << " (" << err << ")";
  }
  if (ngroups > 0 && setgroups(0, nullptr) != 0) {
    int err = errno;
    return ::testing::AssertionFailure()
           << "setgroups(0, nullptr) failed: " << strerror(err) << " (" << err << ")";
  }
  if (setresgid(gid, gid, gid) != 0) {
    int err = errno;
    return ::testing::AssertionFailure() << "setresgid(" << gid << ", " << gid << ", " << gid
                                         << ") failed: " << strerror(err) << " (" << err << ")";
  }
  int current_keep_caps = prctl(PR_GET_KEEPCAPS, 0UL, 0UL, 0UL, 0UL);
  if (current_keep_caps < 0) {
    int err = errno;
    return ::testing::AssertionFailure()
           << "prctl(PR_GET_KEEPCAPS) failed: " << strerror(err) << " (" << err << ")";
  }
  unsigned long expected_keep_caps = keep_caps ? 1UL : 0UL;
  if (static_cast<unsigned long>(current_keep_caps) != expected_keep_caps &&
      prctl(PR_SET_KEEPCAPS, expected_keep_caps, 0UL, 0UL, 0UL) != 0) {
    int err = errno;
    return ::testing::AssertionFailure() << "prctl(PR_SET_KEEPCAPS, " << expected_keep_caps
                                         << ") failed: " << strerror(err) << " (" << err << ")";
  }
  if (setresuid(uid, uid, uid) != 0) {
    int err = errno;
    return ::testing::AssertionFailure() << "setresuid(" << uid << ", " << uid << ", " << uid
                                         << ") failed: " << strerror(err) << " (" << err << ")";
  }
  return ::testing::AssertionSuccess();
}

}  // namespace test_helper
