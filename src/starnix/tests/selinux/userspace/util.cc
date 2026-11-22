// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/starnix/tests/selinux/userspace/util.h"

#include <fcntl.h>
#include <mntent.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/xattr.h>

#include <filesystem>
#include <string>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/lib/files/file.h"
#include "src/lib/files/file_descriptor.h"
#include "src/lib/fxl/strings/string_printf.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {
constexpr char kProcSelfAttrPath[] = "/proc/self/attr/";
}

void LoadPolicy(const std::string& name) {
  auto policy_path = "data/policies/" + name;
  auto binary_policy = ReadFile(policy_path);
  ASSERT_THAT(binary_policy, SyscallResultIsOk()) << "Read of policy at " << policy_path;
  auto result = WriteExistingFile("/sys/fs/selinux/load", binary_policy.value());
  ASSERT_THAT(result, SyscallResultIsOk()) << "Load of policy from " << policy_path;

  // Ensure that the binary policy is reported by the kernel as having been loaded.
  auto loaded_policy = ReadFile("/sys/fs/selinux/policy");
  ASSERT_THAT(loaded_policy, SyscallResultIsOk());
}

fit::result<int, std::string> ReadPolicyCap(const char* capability) {
  auto path = fxl::StringPrintf("/sys/fs/selinux/policy_capabilities/%s", capability);
  return ReadFile(path);
}

bool IsPolicyCapSupported(const char* capability) {
  auto value = ReadPolicyCap(capability);
  if (value.is_ok()) {
    return true;
  }
  EXPECT_THAT(value, SyscallResultIsErrno(ENOENT));
  return false;
}

bool IsPolicyCapEnabled(const char* capability) {
  auto value = ReadPolicyCap(capability);
  if (value.is_ok()) {
    return atoi(value.value().c_str()) != 0;
  }
  EXPECT_THAT(value, SyscallResultIsErrno(ENOENT));
  return false;
}

fit::result<int> WriteExistingFile(const std::string& path, std::string_view data) {
  auto fd = fbl::unique_fd(open(path.c_str(), O_WRONLY | O_TRUNC, 0777));
  if (!fd.is_valid()) {
    return fit::error(errno);
  }
  if (!fxl::WriteFileDescriptor(fd.get(), data.data(), data.size())) {
    return fit::error(errno);
  }
  return fit::ok();
}

fit::result<int> EnsureDeviceNode(std::string_view path, mode_t mode, dev_t dev) {
  std::string path_str(path);
  std::filesystem::path fs_path(path_str);
  if (fs_path.has_parent_path()) {
    std::error_code ec;
    std::filesystem::create_directories(fs_path.parent_path(), ec);
    if (ec) {
      return fit::error(ec.value());
    }
  }
  if (mknod(path_str.c_str(), mode, dev) != 0 && errno != EEXIST) {
    return fit::error(errno);
  }
  if (chmod(path_str.c_str(), mode & 07777) != 0) {
    return fit::error(errno);
  }
  return fit::ok();
}

fit::result<int, std::string> RemoveTrailingNul(std::string in) {
  if (!in.empty()) {
    if (in[in.size() - 1] != 0) {
      ADD_FAILURE() << "Expected NUL-terminator.";
      return fit::error(EOVERFLOW);
    }
    in.pop_back();
  }
  return fit::ok(in);
}

fit::result<int, std::string> ReadFile(const std::string& path) {
  fbl::unique_fd fd(open(path.c_str(), O_RDONLY));
  std::string result;
  if (fd.is_valid() && files::ReadFileDescriptorToString(fd.get(), &result)) {
    return fit::ok(std::move(result));
  }
  return fit::error(errno);
}

fit::result<int, std::string> ReadTaskAttr(std::string_view attr_name) {
  std::string attr_path(kProcSelfAttrPath);
  attr_path.append(attr_name);

  auto attr = ReadFile(attr_path);
  if (attr.is_error()) {
    return attr;
  }
  return RemoveTrailingNul(attr.value());
}

fit::result<int> WriteTaskAttr(std::string_view attr_name, std::string_view context) {
  std::string attr_path(kProcSelfAttrPath);
  attr_path.append(attr_name);

  return WriteExistingFile(attr_path, context);
}

ScopedTaskAttrResetter ScopedTaskAttrResetter::SetTaskAttr(std::string_view attr_name,
                                                           std::string_view new_value) {
  auto old_value = ReadTaskAttr(attr_name);
  if (old_value.is_error()) {
    ADD_FAILURE() << "Saving task attr " << attr_name
                  << " error:" << strerror(old_value.error_value());
    return ScopedTaskAttrResetter("", "");
  }
  auto write_result = WriteTaskAttr(attr_name, new_value);
  if (write_result.is_error()) {
    ADD_FAILURE() << "Setting attr " << attr_name << " to \"" << new_value
                  << "\" error:" << strerror(write_result.error_value());
    return ScopedTaskAttrResetter("", "");
  }
  return ScopedTaskAttrResetter(attr_name, old_value.value());
}

ScopedTaskAttrResetter::ScopedTaskAttrResetter(std::string_view attr_name,
                                               std::string_view old_value) {
  attr_name_ = std::string(attr_name);
  old_value_ = std::string(old_value);
}

ScopedTaskAttrResetter::~ScopedTaskAttrResetter() {
  if (attr_name_.empty()) {
    return;
  }
  auto to_write = old_value_.empty() ? std::string(1, 0) : old_value_;
  auto result = WriteTaskAttr(attr_name_, to_write);
  if (result.is_error()) {
    ADD_FAILURE() << "Restoring task attr " << attr_name_ << " to \"" << old_value_
                  << "\": " << strerror(result.error_value());
  }
}

fit::result<int, std::string> GetLabel(int fd) {
  char buf[256];
  ssize_t result = fgetxattr(fd, "security.selinux", buf, sizeof(buf));
  if (result < 0) {
    return fit::error(errno);
  }
  return RemoveTrailingNul(std::string(buf, result));
}

fit::result<int, std::string> GetLinkLabel(int fd) {
  char buf[256];
  std::string proc_path = fxl::StringPrintf("/proc/self/fd/%d", fd);

  ssize_t result = lgetxattr(proc_path.c_str(), "security.selinux", buf, sizeof(buf));
  if (result < 0) {
    return fit::error(errno);
  }
  return RemoveTrailingNul(std::string(buf, result));
}

fit::result<int, std::string> GetLabel(const std::string& path) {
  char buf[256];
  ssize_t result = getxattr(path.c_str(), "security.selinux", buf, sizeof(buf));
  if (result < 0) {
    return fit::error(errno);
  }
  return RemoveTrailingNul(std::string(buf, result));
}

fit::result<int, std::string> GetPeerSec(int fd) {
  char label_buf[256]{};
  socklen_t label_len = sizeof(label_buf);
  if (getsockopt(fd, SOL_SOCKET, SO_PEERSEC, label_buf, &label_len) == -1) {
    return fit::error(errno);
  }
  return RemoveTrailingNul(std::string(label_buf, label_len));
}

fit::result<int> SetLabel(const std::string& path, const std::string_view label) {
  ssize_t result = setxattr(path.c_str(), "security.selinux", label.data(), label.size(), 0);
  if (result < 0) {
    return fit::error(errno);
  }
  return fit::ok();
}

std::string MakeTestSecurityContext(std::string_view test_domain) {
  return std::string("test_u:test_r:") + std::string(test_domain) + ":s0";
}

fit::result<int, bool> IsSameInode(int fd_1, int fd_2) {
  struct stat fd_1_info;
  if (fstat(fd_1, &fd_1_info) < 0) {
    return fit::error(errno);
  }
  struct stat fd_2_info;
  if (fstat(fd_2, &fd_2_info) < 0) {
    return fit::error(errno);
  }
  return fit::ok(fd_1_info.st_dev == fd_2_info.st_dev && fd_1_info.st_ino == fd_2_info.st_ino);
}

std::string MountOptionsFor(std::string_view mount_path) {
  FILE* mounts = setmntent("/proc/mounts", "r");
  if (!mounts) {
    ADD_FAILURE() << "Failed to open /proc/mounts: " << strerror(errno);
    return "";
  }
  std::string result;
  for (struct mntent* entry = nullptr; (entry = getmntent(mounts));) {
    if (mount_path == entry->mnt_dir) {
      result = entry->mnt_opts;
      break;
    }
  }
  endmntent(mounts);
  return result;
}

pid_t RunInForkedProcessWithLabel(test_helper::ForkHelper& fork_helper, std::string_view label,
                                  fit::function<void()> action) {
  return fork_helper.RunInForkedProcess([label = std::string(label), action = std::move(action)] {
    if (auto result = WriteTaskAttr("current", label); result.is_error()) {
      FAIL() << "Could not write \"" << label
             << "\" to \"current\": " << strerror(result.error_value());
    }
    action();
  });
}

ScopedEnforcement ScopedEnforcement::SetEnforcing() {
  return ScopedEnforcement(/*enforcing=*/true);
}

ScopedEnforcement ScopedEnforcement::SetPermissive() {
  return ScopedEnforcement(/*enforcing=*/false);
}

ScopedEnforcement::ScopedEnforcement(bool enforcing) {
  auto prev = ReadFile("/sys/fs/selinux/enforce");
  if (prev.is_error()) {
    ADD_FAILURE() << "Failed to read /sys/fs/selinux/enforce: " << strerror(prev.error_value());
    return;
  }
  std::string new_state = enforcing ? "1" : "0";
  if (auto write_res = WriteExistingFile("/sys/fs/selinux/enforce", new_state);
      write_res.is_error()) {
    ADD_FAILURE() << "Failed to write /sys/fs/selinux/enforce: "
                  << strerror(write_res.error_value());
    return;
  }
  previous_state_ = std::move(prev.value());
}

ScopedEnforcement::~ScopedEnforcement() {
  if (!previous_state_.empty()) {
    auto write_res = WriteExistingFile("/sys/fs/selinux/enforce", previous_state_);
    EXPECT_TRUE(write_res.is_ok()) << strerror(write_res.error_value());
  }
}

test_helper::ScopedTempFD ScopedTempFDWithLabel(std::string_view label) {
  auto fscreate = ScopedTaskAttrResetter::SetTaskAttr("fscreate", label);
  return test_helper::ScopedTempFD();
}
