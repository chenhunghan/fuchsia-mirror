// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STARNIX_TESTS_SYSCALLS_CPP_PROC_TEST_BASE_H_
#define SRC_STARNIX_TESTS_SYSCALLS_CPP_PROC_TEST_BASE_H_

#include <unistd.h>

#include <optional>
#include <string>
#include <string_view>

#include <gtest/gtest.h>

#include "src/lib/files/file.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

// Some tests run in environments where /proc is not mounted by default, such as the chromiumos
// container. This harness mounts the proc filesystem to /proc if needed.

class ProcTestBase : public ::testing::Test {
 public:
  static constexpr char kProc[] = "/proc";

  void SetUp() override {
    // TODO Ideally the test container should just have /proc mounted by default
    // but first the mount point needs to exist in the image
    // (prebuilt/starnix/chromiumos-image-amd64/system.img).
    if (access(kProc, F_OK) != 0) {
      proc_dir_.emplace();
      ASSERT_FALSE(proc_dir_->path().empty());
      proc_mount_ = ASSERT_RESULT_SUCCESS_AND_RETURN(
          test_helper::ScopedMount::Mount("proc", proc_dir_->path(), "proc", 0, nullptr));
    }
  }

  std::string proc_path() const {
    if (proc_dir_.has_value()) {
      return proc_dir_->path();
    }
    return kProc;
  }

  // Returns the `/proc/self/smaps` entry containing `addr`, or `std::nullopt` if unavailable.
  std::optional<test_helper::MemoryMappingExt> FindMemoryMappingExt(uintptr_t addr) const {
    std::string smaps;
    if (!files::ReadFileToString(proc_path() + "/self/smaps", &smaps)) {
      return std::nullopt;
    }
    return test_helper::find_memory_mapping_ext(addr, smaps);
  }

  std::optional<test_helper::MemoryMappingExt> FindMemoryMappingExt(
      const volatile void* addr) const {
    return FindMemoryMappingExt(reinterpret_cast<uintptr_t>(addr));
  }

  // Returns the KiB value for `field_name` (without trailing `:`) from `/proc/self/status`.
  std::optional<size_t> GetStatusFieldInKb(std::string_view field_name) const {
    return test_helper::GetStatusFieldInKb(proc_path() + "/self/status", field_name);
  }

 private:
  // Order matters: `proc_mount_` must be unmounted before `proc_dir_` removes the mount point.
  std::optional<test_helper::ScopedTempDir> proc_dir_;
  test_helper::ScopedMount proc_mount_;
};

#endif  // SRC_STARNIX_TESTS_SYSCALLS_CPP_PROC_TEST_BASE_H_
