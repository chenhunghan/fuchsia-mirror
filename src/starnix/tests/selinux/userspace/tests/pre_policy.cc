// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/xattr.h>
#include <unistd.h>

#include <map>
#include <optional>
#include <string>
#include <utility>
#include <vector>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <linux/capability.h>

#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

constexpr char kDefaultTmpFsNodeLabel[] = "system_u:object_r:tmpfs_t:s0";
constexpr char kTestSecurityXattr[] = "system_u:object_r:unconfined_t:s0";
constexpr char kTestInvalidSecurityXattr[] = "not a valid context";

constexpr char kTestFileWithoutXattr[] = "/tmp/test-file-without-xattr";
constexpr char kTestFileWithXattr[] = "/tmp/test-file-with-xattr";
constexpr char kTestFileWithInvalidXattr[] = "/tmp/test-file-with-invalid-xattr";

constexpr char kSelinuxAttrName[] = "security.selinux";
constexpr char kTestSecurityAttrName[] = "security.test";
constexpr char kTestAttrValue[] = "test_value";
constexpr char kTestXattrFileLabel[] = "test_u:object_r:test_xattr_access_file_t:s0";

const char* kSelinuxMountOptions[] = {"context", "fscontext", "defcontext", "rootcontext"};

struct PrePolicyProcFsLabels {
  fit::result<int, std::string> meminfo;
  fit::result<int, std::string> stat;
};

struct PrePolicyProcAttrs {
  fit::result<int, std::string> current;
  fit::result<int, std::string> exec;
  fit::result<int, std::string> fscreate;
  fit::result<int, std::string> keycreate;
  fit::result<int, std::string> prev;
  fit::result<int, std::string> sockcreate;
};

std::optional<PrePolicyProcFsLabels> g_pre_policy_procfs_labels;
fbl::unique_fd g_before_policy_userfaultfd;
std::map<std::string, fit::result<int>> g_pre_policy_mount_results;
std::optional<PrePolicyProcAttrs> g_pre_policy_procattrs;
std::vector<std::pair<int, fit::result<int, std::string>>> g_pre_policy_peersec;
std::optional<bool> g_pre_policy_skipcap_passed;

int SetXattr(const std::string& path, const char* name, const std::string& value) {
  return setxattr(path.c_str(), name, value.c_str(), value.size() + 1, 0);
}

std::string SocketTypeToString(int type) {
  switch (type) {
    case SOCK_STREAM:
      return "SOCK_STREAM";
    case SOCK_SEQPACKET:
      return "SOCK_SEQPACKET";
    default:
      return std::to_string(type);
  }
}

}  // namespace

extern std::string GetTestPolicyName() { return "minimal_policy"; }

extern void DoPrePolicyLoadWork() {
  // 1. Capture pre-policy procfs node labels.
  g_pre_policy_procfs_labels = PrePolicyProcFsLabels{
      .meminfo = GetLabel("/proc/meminfo"),
      .stat = GetLabel("/proc/self/stat"),
  };

  // 2. Create tmpfs files before policy load with various security.selinux xattr values.
  auto fd = fbl::unique_fd(open(kTestFileWithoutXattr, O_CREAT | O_RDWR, 0644));
  EXPECT_TRUE(fd.is_valid()) << "Failed to create test file: " << strerror(errno);

  fd = fbl::unique_fd(open(kTestFileWithXattr, O_CREAT | O_RDWR, 0644));
  EXPECT_TRUE(fd.is_valid()) << "Failed to create test file: " << strerror(errno);
  EXPECT_THAT(SetLabel(kTestFileWithXattr, kTestSecurityXattr), SyscallResultIsOk());

  fd = fbl::unique_fd(open(kTestFileWithInvalidXattr, O_CREAT | O_RDWR, 0644));
  EXPECT_TRUE(fd.is_valid()) << "Failed to create test file: " << strerror(errno);
  EXPECT_THAT(SetLabel(kTestFileWithInvalidXattr, kTestInvalidSecurityXattr), SyscallResultIsOk());

  // 3. Create an anonymous inode (userfaultfd) before policy load.
  g_before_policy_userfaultfd =
      fbl::unique_fd(static_cast<int>(syscall(SYS_userfaultfd, O_CLOEXEC)));
  EXPECT_TRUE(g_before_policy_userfaultfd.is_valid())
      << "Failed pre-policy userfaultfd: " << strerror(errno);
  EXPECT_THAT(GetLabel(g_before_policy_userfaultfd.get()), SyscallResultIsErrno(ENOTSUP));

  // 4. Attempt mounts with SELinux context options before policy load.
  for (const char* option : kSelinuxMountOptions) {
    test_helper::ScopedTempDir mount_dir;
    std::string options = std::string(option) + "=system_u:object_r:tmp_t:s0";
    if (mount("tmpfs", mount_dir.path().c_str(), "tmpfs", 0, options.c_str()) != -1) {
      umount(mount_dir.path().c_str());
      g_pre_policy_mount_results.emplace(option, fit::ok());
    } else {
      g_pre_policy_mount_results.emplace(option, fit::error(errno));
    }
  }

  // 5. Read `/proc/self/attr/*` attributes before policy load.
  g_pre_policy_procattrs = PrePolicyProcAttrs{
      .current = ReadTaskAttr("current"),
      .exec = ReadTaskAttr("exec"),
      .fscreate = ReadTaskAttr("fscreate"),
      .keycreate = ReadTaskAttr("keycreate"),
      .prev = ReadTaskAttr("prev"),
      .sockcreate = ReadTaskAttr("sockcreate"),
  };

  // 6. Query `SO_PEERSEC` on connected Unix-domain sockets created before policy load.
  for (int type : {SOCK_STREAM, SOCK_SEQPACKET}) {
    test_helper::ScopedSocketPair sockets(type);
    g_pre_policy_peersec.emplace_back(type, GetPeerSec(sockets.client_fd()));
  }

  // 7. Verify that `security.selinux` skips `CAP_SYS_ADMIN` before policy load while other
  // `security.*` xattrs still require `CAP_SYS_ADMIN`.
  {
    test_helper::ScopedTempFD file;
    test_helper::ForkHelper helper;
    helper.RunInForkedProcess([&] {
      test_helper::UnsetCapabilityEffective(CAP_SYS_ADMIN);
      EXPECT_THAT(SetXattr(file.name(), kSelinuxAttrName, kTestXattrFileLabel), SyscallSucceeds());
      EXPECT_THAT(fsetxattr(file.fd(), kSelinuxAttrName, kTestXattrFileLabel,
                            sizeof(kTestXattrFileLabel), 0),
                  SyscallSucceeds());
      EXPECT_THAT(lsetxattr(file.name().c_str(), kSelinuxAttrName, kTestXattrFileLabel,
                            sizeof(kTestXattrFileLabel), 0),
                  SyscallSucceeds());
      EXPECT_THAT(removexattr(file.name().c_str(), kSelinuxAttrName), SyscallSucceeds());
      EXPECT_THAT(SetXattr(file.name(), kTestSecurityAttrName, kTestAttrValue),
                  SyscallFailsWithErrno(EPERM));
      EXPECT_THAT(removexattr(file.name().c_str(), kTestSecurityAttrName),
                  SyscallFailsWithErrno(EPERM));
    });
    g_pre_policy_skipcap_passed = helper.WaitForChildren();
  }
}

namespace {

TEST(PolicyLoadTest, TasksUseKernelSid) {
  // All processes created prior to policy loading are labeled with the kernel SID.
  EXPECT_THAT(ReadTaskAttr("current"), SyscallResultIsOk("system_u:unconfined_r:unconfined_t:s0"));
}

TEST(PolicyLoadTest, TmpFsFsUseTransIgnoresSecurityXattr) {
  // "tmpfs" is defined to use the `fs_use_trans` labeling scheme, which ignores the xattr value.
  EXPECT_THAT(GetLabel(kTestFileWithoutXattr), SyscallResultIsOk(kDefaultTmpFsNodeLabel));
  EXPECT_THAT(GetLabel(kTestFileWithXattr), SyscallResultIsOk(kDefaultTmpFsNodeLabel));
  EXPECT_THAT(GetLabel(kTestFileWithInvalidXattr), SyscallResultIsOk(kDefaultTmpFsNodeLabel));
}

TEST(PolicyLoadTest, ProcFsLabels) {
  ASSERT_TRUE(g_pre_policy_procfs_labels.has_value());
  // Procfs does not support xattrs prior to policy load.
  EXPECT_THAT(g_pre_policy_procfs_labels->meminfo, SyscallResultIsErrno(EOPNOTSUPP));
  EXPECT_THAT(g_pre_policy_procfs_labels->stat, SyscallResultIsErrno(EOPNOTSUPP));

  // After policy load, non-PID nodes use the genfscon label and PID nodes use the task label.
  EXPECT_THAT(GetLabel("/proc/meminfo"), SyscallResultIsOk("system_u:object_r:unconfined_t:s0"));
  EXPECT_THAT(GetLabel("/proc/self/stat"),
              SyscallResultIsOk("system_u:unconfined_r:unconfined_t:s0"));
}

TEST(AnonInodeTest, EventFdBeforePolicy) {
  // userfaultfd() created before policy load should have been labeled based on the "kernel" SID.
  EXPECT_THAT(GetLabel(g_before_policy_userfaultfd.get()),
              SyscallResultIsOk("unlabeled_u:unlabeled_r:unlabeled_t:s0"));
}

class MountOptionValidationTest : public ::testing::TestWithParam<const char*> {};

TEST_P(MountOptionValidationTest, RejectedBeforePolicyLoad) {
  const char* option_name = GetParam();
  auto it = g_pre_policy_mount_results.find(option_name);
  ASSERT_NE(it, g_pre_policy_mount_results.end()) << "Result for " << option_name << " not found";
  const auto& result = it->second;

  EXPECT_THAT(result, SyscallResultIsErrno(EINVAL))
      << "Mount with '" << option_name << "' unexpectedly succeeded or failed with wrong error";
}

INSTANTIATE_TEST_SUITE_P(MountTests, MountOptionValidationTest,
                         ::testing::ValuesIn(kSelinuxMountOptions),
                         [](const testing::TestParamInfo<const char*>& info) {
                           return info.param;
                         });

TEST(ProcAttrTest, PrePolicyAttrs) {
  ASSERT_TRUE(g_pre_policy_procattrs.has_value());
  EXPECT_THAT(g_pre_policy_procattrs->current, SyscallResultIsOk("kernel"));
  EXPECT_THAT(g_pre_policy_procattrs->prev, SyscallResultIsOk("kernel"));
  EXPECT_THAT(g_pre_policy_procattrs->exec, SyscallResultIsOk(""));
  EXPECT_THAT(g_pre_policy_procattrs->fscreate, SyscallResultIsOk(""));
  EXPECT_THAT(g_pre_policy_procattrs->keycreate, SyscallResultIsOk(""));
  EXPECT_THAT(g_pre_policy_procattrs->sockcreate, SyscallResultIsOk(""));
}

// Before a policy is loaded, sockets are labeled with the "kernel" initial SID, and Linux reports
// the name of that initial SID as the peer label.
TEST(SocketPeerSecTest, PrePolicyLoad) {
  ASSERT_EQ(g_pre_policy_peersec.size(), 2u);
  for (const auto& [type, peersec] : g_pre_policy_peersec) {
    EXPECT_THAT(peersec, SyscallResultIsOk("kernel")) << SocketTypeToString(type);
  }
}

TEST(XattrTest, PrePolicySecuritySelinuxSkipsCapSysAdmin) {
  ASSERT_TRUE(g_pre_policy_skipcap_passed.has_value());
  EXPECT_TRUE(*g_pre_policy_skipcap_passed);
}

}  // namespace
