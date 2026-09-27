// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sys/stat.h>
#include <unistd.h>

#include <array>
#include <filesystem>
#include <span>
#include <string>
#include <unordered_set>

#include <gtest/gtest.h>

#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

extern std::string DoPrePolicyLoadWork() { return "cgroupfs_policy"; }

namespace {

constexpr char kDefaultLabel[] = "system_u:object_r:unconfined_t:s0";
constexpr char kContextLabel[] = "test_u:object_r:test_cgroupfs_context_t:s0";
constexpr char kRootContextLabel[] = "test_u:object_r:test_cgroupfs_rootcontext_t:s0";

// A cgroup v2 filesystem, mounted in its own temporary directory, holding a single child
// cgroup.
struct CgroupV2 {
  ~CgroupV2() {
    // cgroup v2 hierarchy state outlives the mount, so a child cgroup left behind here
    // would re-appear the next time the filesystem is mounted.
    if (!child_path.empty()) {
      EXPECT_THAT(rmdir(child_path.c_str()), SyscallSucceeds());
    }
  }

  // Mounts a cgroup v2 filesystem with `options`, and creates a child cgroup in it.
  void Mount(const std::string& options = "") {
    root_path = temp_dir.path() + "/cgroup2_mnt";
    ASSERT_THAT(mkdir(root_path.c_str(), 0755), SyscallSucceeds());

    mount = ASSERT_RESULT_SUCCESS_AND_RETURN(
        test_helper::ScopedMount::Mount("none", root_path, "cgroup2", 0, options.c_str()));

    std::string child = root_path + "/child";
    ASSERT_THAT(mkdir(child.c_str(), 0755), SyscallSucceeds());
    child_path = std::move(child);
  }

  test_helper::ScopedTempDir temp_dir;
  test_helper::ScopedMount mount;
  std::string root_path;
  std::string child_path;
};

constexpr std::array kRootInterfaceFiles = {
    "cgroup.procs",
    "cgroup.controllers",
    "cgroup.subtree_control",
};

constexpr std::array kChildInterfaceFiles = {
    "cgroup.procs", "cgroup.controllers", "cgroup.events",          "cgroup.freeze",
    "cgroup.kill",  "cgroup.type",        "cgroup.subtree_control",
};

void VerifyDirectoryEntriesLabel(const std::string& dir_path, const std::string& expected_label,
                                 std::span<const char* const> expected_files) {
  std::unordered_set<std::string> found_files;
  for (const auto& entry : std::filesystem::directory_iterator(dir_path)) {
    found_files.insert(entry.path().filename().string());
    EXPECT_THAT(GetLabel(entry.path().string()), SyscallResultIsOk(expected_label))
        << "for " << entry.path();
  }
  EXPECT_THAT(found_files, testing::IsSupersetOf(expected_files));
}

struct CgroupV2TestParam {
  const char* name;
  std::string mount_options;
  std::string expected_root_dir_label;
  std::string expected_child_label;
};

}  // namespace

class CgroupFsTest : public IsolatedMountNamespaceTest,
                     public testing::WithParamInterface<CgroupV2TestParam> {};

// Verify SELinux security labels when mounting a cgroup v2 filesystem after a prior mount
// has occurred. In SELinux, mount options such as context= or rootcontext= cannot be changed
// while a superblock is actively mounted; therefore, the initial mount must be unmounted before
// testing subsequent mounts with different SELinux options.
TEST_P(CgroupFsTest, SecondMountWithDifferentOptionsChangesLabels) {
  // Mount and unmount cgroup2 first to initialize internal filesystem state before testing
  // subsequent mounts with different SELinux mount options.
  {
    CgroupV2 cgroupfs;
    ASSERT_NO_FATAL_FAILURE(cgroupfs.Mount());
  }

  const CgroupV2TestParam& param = GetParam();
  CgroupV2 cgroupfs;
  ASSERT_NO_FATAL_FAILURE(cgroupfs.Mount(param.mount_options));
  auto enforce = ScopedEnforcement::SetEnforcing();

  EXPECT_THAT(GetLabel(cgroupfs.root_path), SyscallResultIsOk(param.expected_root_dir_label));
  VerifyDirectoryEntriesLabel(cgroupfs.root_path, param.expected_child_label, kRootInterfaceFiles);

  EXPECT_THAT(GetLabel(cgroupfs.child_path), SyscallResultIsOk(param.expected_child_label));
  VerifyDirectoryEntriesLabel(cgroupfs.child_path, param.expected_child_label,
                              kChildInterfaceFiles);
}

INSTANTIATE_TEST_SUITE_P(CgroupFs, CgroupFsTest,
                         ::testing::Values(
                             CgroupV2TestParam{
                                 .name = "Default",
                                 .mount_options = "",
                                 .expected_root_dir_label = kDefaultLabel,
                                 .expected_child_label = kDefaultLabel,
                             },
                             CgroupV2TestParam{
                                 .name = "Context",
                                 .mount_options = std::string("context=") + kContextLabel,
                                 .expected_root_dir_label = kContextLabel,
                                 .expected_child_label = kContextLabel,
                             },
                             CgroupV2TestParam{
                                 .name = "RootContext",
                                 .mount_options = std::string("rootcontext=") + kRootContextLabel,
                                 .expected_root_dir_label = kRootContextLabel,
                                 .expected_child_label = kDefaultLabel,
                             }),
                         [](const testing::TestParamInfo<CgroupV2TestParam>& info) {
                           return info.param.name;
                         });
