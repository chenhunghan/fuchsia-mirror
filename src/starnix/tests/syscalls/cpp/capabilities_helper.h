// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_STARNIX_TESTS_SYSCALLS_CPP_CAPABILITIES_HELPER_H_
#define SRC_STARNIX_TESTS_SYSCALLS_CPP_CAPABILITIES_HELPER_H_

#include <lib/fit/result.h>
#include <stdint.h>
#include <sys/types.h>

#include <filesystem>
#include <string>

#include <gtest/gtest.h>

namespace test_helper {

// Drops all capabilities from the effective, permitted, and inheritable sets.
void DropAllCapabilities();

// Checks if a capability is in the effective set.
bool HasCapability(int cap);

// Unsets a capability from the effective set.
void UnsetCapability(int cap);

// Checks whether a capability is in the thread's effective set.
bool HasCapabilityEffective(int cap);

// Checks whether a capability is in the thread's permitted set.
bool HasCapabilityPermitted(int cap);

// Checks whether a capability is in the thread's inheritable set.
bool HasCapabilityInheritable(int cap);

// Checks whether a capability is in the thread's ambient set.
bool HasCapabilityAmbient(int cap);

// Checks whether a capability is in the thread's bounding set.
bool HasCapabilityBounding(int cap);

// Removes a capability from the thread's effective set.
void UnsetCapabilityEffective(int cap);

// Removes a capability from the thread's permitted set.
void UnsetCapabilityPermitted(int cap);

// Removes a capability from the thread's inheritable set.
void UnsetCapabilityInheritable(int cap);

// Removes a capability from the thread's ambient set.
void UnsetCapabilityAmbient(int cap);

// Removes a capability from the thread's bounded set.
void UnsetCapabilityBounding(int cap);

// Drops all capabilities from the ambient set.
void DropAllAmbientCapabilities();

// Sets a capability in the thread's effective set.
void SetCapabilityEffective(int cap);

// Sets a capability in the thread's permitted set.
void SetCapabilityPermitted(int cap);

// Sets a capability in the thread's inheritable set.
void SetCapabilityInheritable(int cap);

// Sets a capability in the thread's ambient set.
void SetCapabilityAmbient(int cap);

// Sets v2 `security.capability` with the given `permitted` and `inheritable` capability bitmasks
// (and `VFS_CAP_FLAGS_EFFECTIVE` if `effective` is true) on `path`, returning the `setxattr`
// result.
int SetFileCapabilitiesV2(const std::filesystem::path& path, uint64_t permitted,
                          uint64_t inheritable = 0, bool effective = true);

// Attempts to set or unset a capability, returning `fit::ok()` on success or `fit::error(errno)`
// on failure.
fit::result<int> TrySetCapabilityEffective(int cap);
fit::result<int> TrySetCapabilityPermitted(int cap);
fit::result<int> TryUnsetCapabilityPermitted(int cap);
fit::result<int> TrySetCapabilityAmbient(int cap);

// Reads the maximum capability number supported by the running kernel from
// `/proc/sys/kernel/cap_last_cap`, returning `fit::error(errno)` on read failure or
// `fit::error(EINVAL)` if the file cannot be parsed.
fit::result<int, int> GetLastCap();

// Switches real, effective, and saved-set UIDs and GIDs to `uid` and `gid`, clearing any attached
// supplementary groups and updating `PR_SET_KEEPCAPS` to match `keep_caps`.
[[nodiscard]] ::testing::AssertionResult SwitchToUnprivilegedUser(uid_t uid, gid_t gid,
                                                                  bool keep_caps = false);

}  // namespace test_helper

#endif  // SRC_STARNIX_TESTS_SYSCALLS_CPP_CAPABILITIES_HELPER_H_
