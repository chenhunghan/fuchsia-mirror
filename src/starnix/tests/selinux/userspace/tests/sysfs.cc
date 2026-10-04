// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>

#include <array>
#include <filesystem>
#include <span>
#include <string>
#include <unordered_set>

#include <fbl/unique_fd.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <linux/loop.h>

#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

extern std::string DoPrePolicyLoadWork() {
  EXPECT_THAT(mknod("/dev/loop-control", S_IFCHR | 0600, makedev(10, 237)),
              testing::AnyOf(SyscallSucceeds(), SyscallFailsWithErrno(EEXIST)));
  return "sysfs_policy";
}

namespace {

constexpr char kDefaultLabel[] = "system_u:object_r:unconfined_t:s0";
constexpr char kParentXattrLabel[] = "system_u:object_r:test_sysfs_parent_t:s0";
constexpr char kFscreateLabel[] = "test_u:object_r:test_sysfs_fscreate_t:s0";
constexpr char kTransitionParentLabel[] = "system_u:object_r:test_sysfs_transition_parent_t:s0";
constexpr char kTransitionDirLabel[] = "system_u:object_r:test_sysfs_transition_dir_t:s0";
constexpr char kTransitionFileLabel[] = "system_u:object_r:test_sysfs_transition_file_t:s0";

constexpr unsigned int kLoopMajor = 7;
// Minor number from which to search for an unused loop device. Starts above the pre-created
// `loop0`-`loop7` devices to avoid needlessly probing them.
constexpr int kFirstLoopMinor = 100;

// Interface files created in `/devices/virtual/block/loop<N>` by `LOOP_CTL_ADD`.
constexpr std::array kBlockDeviceInterfaceFiles = {
    "dev",
    "size",
    "uevent",
};

// Interface file created in the `loop<N>/loop` attribute group when a backing file is attached.
// Linux also creates other files in this group (e.g. `offset`, `sizelimit`), but Starnix only
// implements `backing_file`.
constexpr std::array kLoopAttributeFiles = {
    "backing_file",
};

void VerifyDirectoryEntriesLabel(const std::string& dir_path, const std::string& expected_label,
                                 std::span<const char* const> expected_files) {
  std::unordered_set<std::string> found_files;
  for (const auto& entry : std::filesystem::directory_iterator(dir_path)) {
    // Only check regular interface files; skip subdirectories and symlinks.
    if (entry.is_symlink() || entry.is_directory()) {
      continue;
    }
    found_files.insert(entry.path().filename().string());
    EXPECT_THAT(GetLabel(entry.path().string()), SyscallResultIsOk(expected_label))
        << "for " << entry.path();
  }
  EXPECT_THAT(found_files, testing::IsSupersetOf(expected_files));
}

struct SysFsTestParam {
  std::string name;
  // Label to set on the parent directory, or empty to leave it without a `security.selinux` xattr.
  std::string parent_label;
  // Label to set as the task's fscreate attribute, or empty to leave it unset.
  std::string fscreate_label;
  std::string expected_dir_label;
  std::string expected_file_label;
};

// Fixture providing a mounted sysfs filesystem and a loop device created by the test.
//
// All sysfs mounts, regardless of mount or network namespace, share the same underlying kernfs
// nodes, and `security.selinux` xattrs are stored on those nodes. An xattr set on a pre-existing
// node (e.g. `/devices/virtual/block`) can't be removed, and would leak into later tests and the
// rest of the system. Tests must therefore only set xattrs on nodes that the fixture creates and
// removes, i.e. `device_path_` and its descendants.
class SysFsTest : public IsolatedMountNamespaceTest,
                  public testing::WithParamInterface<SysFsTestParam> {
 protected:
  void SetUp() override {
    IsolatedMountNamespaceTest::SetUp();

    std::string root_path = temp_dir_.path() + "/sysfs_mnt";
    ASSERT_THAT(mkdir(root_path.c_str(), 0755), SyscallSucceeds());

    mount_ = ASSERT_RESULT_SUCCESS_AND_RETURN(
        test_helper::ScopedMount::Mount("none", root_path, "sysfs", 0, ""));

    loop_control_fd_ = fbl::unique_fd(open("/dev/loop-control", O_RDWR));
    ASSERT_TRUE(loop_control_fd_.is_valid()) << strerror(errno);

    // Creates `/devices/virtual/block/loop<N>` and its interface files. Uses the first unused minor
    // rather than `LOOP_CTL_GET_FREE`, which may return a pre-existing device whose sysfs nodes
    // were not created (and will not be removed) by this test.
    int minor = kFirstLoopMinor;
    while (ioctl(loop_control_fd_.get(), LOOP_CTL_ADD, minor) < 0) {
      ASSERT_EQ(errno, EEXIST) << strerror(errno);
      ++minor;
    }
    loop_minor_ = minor;
    device_path_ = root_path + "/devices/virtual/block/loop" + std::to_string(loop_minor_);

    // devtmpfs creates the device node automatically; create it if the environment lacks one.
    dev_node_path_ = "/dev/loop" + std::to_string(loop_minor_);
    if (mknod(dev_node_path_.c_str(), S_IFBLK | 0600, makedev(kLoopMajor, loop_minor_)) == 0) {
      created_dev_node_ = true;
    } else {
      ASSERT_EQ(errno, EEXIST) << strerror(errno);
    }
    loop_fd_ = fbl::unique_fd(open(dev_node_path_.c_str(), O_RDWR));
    ASSERT_TRUE(loop_fd_.is_valid()) << strerror(errno);

    backing_fd_ = fbl::unique_fd(
        open((temp_dir_.path() + "/backing_file").c_str(), O_RDWR | O_CREAT | O_EXCL, 0600));
    ASSERT_TRUE(backing_fd_.is_valid()) << strerror(errno);
    ASSERT_THAT(ftruncate(backing_fd_.get(), 4096), SyscallSucceeds());
  }

  void TearDown() override {
    if (backing_attached_) {
      // On recent kernels this only marks the device for autoclear, and the backing file (and the
      // `loop` attribute group) is detached when the last reference to the device is closed.
      EXPECT_THAT(ioctl(loop_fd_.get(), LOOP_CLR_FD, 0), SyscallSucceeds());
    }
    loop_fd_.reset();
    if (created_dev_node_) {
      EXPECT_THAT(unlink(dev_node_path_.c_str()), SyscallSucceeds());
    }
    if (loop_minor_ >= 0) {
      // Removes `device_path_` and all its descendants, including any xattrs set on them.
      // TODO: https://fxbug.dev/567481478 - On Starnix, `LOOP_CLR_FD` resets the loop device's
      // state, including its sysfs device handle, so `LOOP_CTL_REMOVE` fails with `EINVAL` and
      // leaves `device_path_` (and its xattrs) behind.
      EXPECT_THAT(ioctl(loop_control_fd_.get(), LOOP_CTL_REMOVE, loop_minor_), SyscallSucceeds());
    }
  }

  // Attaches the backing file to the loop device. On Linux, this makes the loop driver create the
  // `loop` attribute group subdirectory and its interface files under `device_path_`, in the
  // context of the calling task.
  void AttachBackingFile() {
    ASSERT_THAT(ioctl(loop_fd_.get(), LOOP_SET_FD, backing_fd_.get()), SyscallSucceeds());
    backing_attached_ = true;
  }

  test_helper::ScopedTempDir temp_dir_;
  test_helper::ScopedMount mount_;
  fbl::unique_fd loop_control_fd_;
  fbl::unique_fd loop_fd_;
  fbl::unique_fd backing_fd_;
  std::string device_path_;
  std::string dev_node_path_;
  // Minor number of the loop device created by `SetUp`, or -1 if none was created.
  int loop_minor_ = -1;
  bool created_dev_node_ = false;
  bool backing_attached_ = false;
};

// Verify that when a parent sysfs directory has an explicit `security.selinux` xattr set,
// `selinux_kernfs_init_security` applies the appropriate SELinux label (parent xattr inheritance,
// type transition rules, or task fscreate attribute) to both newly created subdirectories and
// their auto-populated interface files. When the parent has no xattr, the new nodes are left
// unlabeled, and receive the `genfscon` label regardless of the task's fscreate attribute.
TEST_P(SysFsTest, SubdirectoryLabels) {
  const SysFsTestParam& param = GetParam();
  auto enforce = ScopedEnforcement::SetEnforcing();

  if (!param.parent_label.empty()) {
    ASSERT_THAT(SetLabel(device_path_, param.parent_label), SyscallResultIsOk());
    EXPECT_THAT(GetLabel(device_path_), SyscallResultIsOk(param.parent_label));
  } else {
    EXPECT_THAT(GetLabel(device_path_), SyscallResultIsOk(kDefaultLabel));
  }

  // Pre-existing interface files in `device_path_` keep `kDefaultLabel`, whether or not the parent
  // was labeled.
  VerifyDirectoryEntriesLabel(device_path_, kDefaultLabel, kBlockDeviceInterfaceFiles);

  auto test_child_creation = [&]() {
    // The `loop` attribute group must be created by `AttachBackingFile`, after the parent has been
    // labeled, for `selinux_kernfs_init_security` to apply to it.
    const std::string sub_path = device_path_ + "/loop";
    ASSERT_FALSE(std::filesystem::exists(sub_path));
    ASSERT_NO_FATAL_FAILURE(AttachBackingFile());
    ASSERT_TRUE(std::filesystem::exists(sub_path));

    EXPECT_THAT(GetLabel(sub_path), SyscallResultIsOk(param.expected_dir_label));
    VerifyDirectoryEntriesLabel(sub_path, param.expected_file_label, kLoopAttributeFiles);
  };

  if (!param.fscreate_label.empty()) {
    auto reset_fscreate = ScopedTaskAttrResetter::SetTaskAttr("fscreate", param.fscreate_label);
    test_child_creation();
  } else {
    test_child_creation();
  }
}

INSTANTIATE_TEST_SUITE_P(SysFs, SysFsTest,
                         testing::Values(
                             SysFsTestParam{
                                 .name = "InheritsParentXattrLabel",
                                 .parent_label = kParentXattrLabel,
                                 .fscreate_label = {},
                                 .expected_dir_label = kParentXattrLabel,
                                 .expected_file_label = kParentXattrLabel,
                             },
                             SysFsTestParam{
                                 .name = "AppliesTypeTransitionWhenParentHasXattr",
                                 .parent_label = kTransitionParentLabel,
                                 .fscreate_label = {},
                                 .expected_dir_label = kTransitionDirLabel,
                                 .expected_file_label = kTransitionFileLabel,
                             },
                             SysFsTestParam{
                                 .name = "UsesFscreateLabelWhenParentHasXattr",
                                 .parent_label = kParentXattrLabel,
                                 .fscreate_label = kFscreateLabel,
                                 .expected_dir_label = kFscreateLabel,
                                 .expected_file_label = kFscreateLabel,
                             },
                             SysFsTestParam{
                                 .name = "UsesDefaultLabelWhenParentHasNoXattr",
                                 .parent_label = {},
                                 .fscreate_label = {},
                                 .expected_dir_label = kDefaultLabel,
                                 .expected_file_label = kDefaultLabel,
                             },
                             SysFsTestParam{
                                 .name = "IgnoresFscreateLabelWhenParentHasNoXattr",
                                 .parent_label = {},
                                 .fscreate_label = kFscreateLabel,
                                 .expected_dir_label = kDefaultLabel,
                                 .expected_file_label = kDefaultLabel,
                             }),
                         [](const testing::TestParamInfo<SysFsTestParam>& info) {
                           return info.param.name;
                         });

}  // namespace
