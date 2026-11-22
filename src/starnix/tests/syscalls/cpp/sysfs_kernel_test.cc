// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#include <fcntl.h>
#include <lib/fit/defer.h>
#include <limits.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>

#include <cerrno>
#include <charconv>
#include <optional>
#include <set>
#include <string>
#include <string_view>
#include <vector>

#include <fbl/unique_fd.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/lib/files/directory.h"
#include "src/lib/files/file.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

using testing::ContainsRegex;
using testing::IsSupersetOf;

class SysfsKernelTest : public ::testing::Test {
 public:
  void SetUp() override {
    // TODO(https://fxbug.dev/317285180) don't skip on baseline
    // Assume starnix always has /sys/kernel.
    if (!test_helper::IsStarnix() && access("/sys/kernel/wakeup_reasons", F_OK) == -1) {
      GTEST_SKIP() << "/sys/kernel/wakeup_reasons not available, skipping...";
    }
  }
};

TEST_F(SysfsKernelTest, KernelDirectoryContainsExpectedContents) {
  std::vector<std::string> files;
  EXPECT_TRUE(files::ReadDirContents("/sys/kernel", &files));
  EXPECT_THAT(files, IsSupersetOf({"tracing", "wakeup_reasons"}));
}

TEST_F(SysfsKernelTest, WakeupReasonsDirectoryContainsExpectedContents) {
  std::vector<std::string> files;
  EXPECT_TRUE(files::ReadDirContents("/sys/kernel/wakeup_reasons", &files));
  EXPECT_THAT(files, IsSupersetOf({"last_resume_reason", "last_suspend_time"}));
}

TEST_F(SysfsKernelTest, WakeupReasonsDirectoryContainDefaultsLastResumeReason) {
  std::string last_resume_reason_str;
  EXPECT_TRUE(files::ReadFileToString("/sys/kernel/wakeup_reasons/last_resume_reason",
                                      &last_resume_reason_str));
  EXPECT_THAT(last_resume_reason_str, ContainsRegex("^.*\n"));
}

TEST_F(SysfsKernelTest, WakeupReasonsDirectoryContainDefaultsLastSuspendTime) {
  std::string last_suspend_time_str;
  EXPECT_TRUE(files::ReadFileToString("/sys/kernel/wakeup_reasons/last_suspend_time",
                                      &last_suspend_time_str));
  EXPECT_THAT(last_suspend_time_str, ContainsRegex("^[0-9]+ [0-9]+\n"));
}

namespace {

std::optional<std::string> ReadSymlink(const std::string& path) {
  char buf[PATH_MAX];
  ssize_t len = readlink(path.c_str(), buf, sizeof(buf));
  if (len < 0) {
    return std::nullopt;
  }
  return std::string(buf, static_cast<size_t>(len));
}

}  // namespace

TEST(SysfsHierarchyTest, DeviceSubsystemSymlinks) {
  EXPECT_EQ(ReadSymlink("/sys/devices/system/cpu/cpu0/subsystem"), "../../../../bus/cpu");
  EXPECT_EQ(ReadSymlink("/sys/devices/virtual/mem/null/subsystem"), "../../../../class/mem");
  EXPECT_THAT(access("/sys/devices/system/cpu/subsystem", F_OK), SyscallFailsWithErrno(ENOENT));
  EXPECT_THAT(access("/sys/devices/virtual/subsystem", F_OK), SyscallFailsWithErrno(ENOENT));
}

TEST(SysfsHierarchyTest, DevicetreeNodes) {
  if (!test_helper::IsStarnix() && access("/sys/firmware/devicetree/base", F_OK) != 0) {
    struct stat st{};
    EXPECT_THAT(fstatat(AT_FDCWD, "/proc/device-tree", &st, AT_SYMLINK_NOFOLLOW),
                SyscallFailsWithErrno(ENOENT));
    GTEST_SKIP() << "/sys/firmware/devicetree/base not available on this host";
  }

  struct stat base_st{};
  ASSERT_THAT(stat("/sys/firmware/devicetree/base", &base_st), SyscallSucceeds());
  EXPECT_TRUE(S_ISDIR(base_st.st_mode));

  EXPECT_EQ(ReadSymlink("/proc/device-tree"), "/sys/firmware/devicetree/base");
  struct stat proc_st{};
  ASSERT_THAT(stat("/proc/device-tree", &proc_st), SyscallSucceeds());
  EXPECT_TRUE(S_ISDIR(proc_st.st_mode));
  EXPECT_EQ(proc_st.st_dev, base_st.st_dev);
  EXPECT_EQ(proc_st.st_ino, base_st.st_ino);

  if (test_helper::IsStarnix()) {
    std::string root_name;
    ASSERT_TRUE(files::ReadFileToString("/sys/firmware/devicetree/base/name", &root_name));
    EXPECT_EQ(root_name, std::string("\0", 1));

    struct stat model_st{};
    ASSERT_THAT(stat("/sys/firmware/devicetree/base/model", &model_st), SyscallSucceeds());
    EXPECT_TRUE(S_ISREG(model_st.st_mode));
    EXPECT_EQ(model_st.st_mode & 0777, static_cast<mode_t>(0444));
    EXPECT_EQ(model_st.st_size, 13);

    std::string model;
    ASSERT_TRUE(files::ReadFileToString("/sys/firmware/devicetree/base/model", &model));
    EXPECT_EQ(model, std::string("Starnix Test\0", 13));

    std::string proc_model;
    ASSERT_TRUE(files::ReadFileToString("/proc/device-tree/model", &proc_model));
    EXPECT_EQ(proc_model, model);

    std::string compatible;
    ASSERT_TRUE(files::ReadFileToString("/sys/firmware/devicetree/base/compatible", &compatible));
    EXPECT_EQ(compatible, std::string("fuchsia,starnix-test\0", 21));

    std::string chosen_name;
    ASSERT_TRUE(files::ReadFileToString("/sys/firmware/devicetree/base/chosen/name", &chosen_name));
    EXPECT_EQ(chosen_name, std::string("chosen\0", 7));

    std::string bootargs;
    ASSERT_TRUE(
        files::ReadFileToString("/sys/firmware/devicetree/base/chosen/bootargs", &bootargs));
    EXPECT_EQ(bootargs, std::string("console=ttyS0\0", 14));

    std::string memory_name;
    ASSERT_TRUE(files::ReadFileToString("/sys/firmware/devicetree/base/memory@80000000/name",
                                        &memory_name));
    EXPECT_EQ(memory_name, std::string("memory\0", 7));
  }
}

TEST(SysfsHierarchyTest, MmDirectoryNodes) {
  if (test_helper::IsStarnix()) {
    std::vector<std::string> files;
    ASSERT_TRUE(files::ReadDirContents("/sys/kernel/mm", &files));
    EXPECT_THAT(files,
                IsSupersetOf({"cma", "lru_gen", "pgsize_migration", "transparent_hugepage"}));

    std::vector<std::string> cma_entries;
    ASSERT_TRUE(files::ReadDirContents("/sys/kernel/mm/cma", &cma_entries));
    EXPECT_THAT(cma_entries, testing::UnorderedElementsAre(".", ".."));
  }

  if (test_helper::IsStarnix() || access("/sys/kernel/mm/cma", F_OK) == 0) {
    struct stat st{};
    ASSERT_THAT(stat("/sys/kernel/mm/cma", &st), SyscallSucceeds());
    EXPECT_TRUE(S_ISDIR(st.st_mode));
    EXPECT_EQ(st.st_mode & 0777u, static_cast<mode_t>(0755));
  }

  struct EnabledNodeCase {
    const char* path;
    const char* pattern;
    const char* starnix_expected;
  };
  for (const auto& [path, pattern, starnix_expected] : {
           EnabledNodeCase{
               "/sys/kernel/mm/transparent_hugepage/enabled",
               R"(^(\[always\] madvise never|always \[madvise\] never|always madvise \[never\]))"
               "\n$",
               "always madvise [never]\n",
           },
           EnabledNodeCase{"/sys/kernel/mm/lru_gen/enabled", "^0x[0-9a-fA-F]+\n$", "0x0000\n"},
           EnabledNodeCase{"/sys/kernel/mm/pgsize_migration/enabled", "^[01]\n$", "0\n"},
       }) {
    SCOPED_TRACE(path);
    if (test_helper::IsStarnix() || access(path, F_OK) == 0) {
      struct stat st{};
      ASSERT_THAT(stat(path, &st), SyscallSucceeds());
      EXPECT_TRUE(S_ISREG(st.st_mode));
      EXPECT_EQ(st.st_mode & 0777u, static_cast<mode_t>(0644));

      std::string enabled;
      ASSERT_TRUE(files::ReadFileToString(path, &enabled));
      EXPECT_THAT(enabled, ContainsRegex(pattern));
      if (test_helper::IsStarnix()) {
        EXPECT_EQ(enabled, starnix_expected);
      }

      if (test_helper::IsStarnix() ? geteuid() == 0 : access(path, W_OK) == 0) {
        fbl::unique_fd fd(open(path, O_WRONLY | O_CLOEXEC));
        ASSERT_TRUE(fd.is_valid()) << strerror(errno);
        if (!test_helper::IsStarnix() &&
            std::string_view(path) == "/sys/kernel/mm/pgsize_migration/enabled" &&
            sysconf(_SC_PAGESIZE) != 4096) {
          // On non-4 KB Android GKI Linux kernels, `store_pgsize_migration_enabled` returns `n` as
          // a no-op before parsing input.
          EXPECT_THAT(write(fd.get(), "invalid\n", 8), SyscallSucceedsWithValue(8));
        } else {
          EXPECT_THAT(write(fd.get(), "invalid\n", 8), SyscallFailsWithErrno(EINVAL));
        }

        std::string after_write;
        ASSERT_TRUE(files::ReadFileToString(path, &after_write));
        EXPECT_EQ(after_write, enabled);
      }
    }
  }
}

TEST(SysfsHierarchyTest, DefaultClassesAndCpuDevices) {
  EXPECT_THAT(access("/sys/class/bdi", F_OK), SyscallSucceeds());
  EXPECT_THAT(access("/sys/class/net", F_OK), SyscallSucceeds());
  EXPECT_THAT(access("/sys/class/power_supply", F_OK), SyscallSucceeds());

  EXPECT_THAT(access("/sys/devices/system/cpu/uevent", F_OK), SyscallSucceeds());
  EXPECT_THAT(access("/sys/devices/system/cpu/online", F_OK), SyscallSucceeds());
  EXPECT_THAT(access("/sys/devices/system/cpu/possible", F_OK), SyscallSucceeds());
  if (test_helper::IsStarnix()) {
    EXPECT_THAT(access("/sys/devices/system/cpu/cpufreq", F_OK), SyscallSucceeds());
  }
  EXPECT_THAT(access("/sys/devices/system/cpu/cpu0/uevent", F_OK), SyscallSucceeds());
  EXPECT_THAT(access("/sys/devices/system/cpu/cpu0/topology", F_OK), SyscallSucceeds());
  EXPECT_THAT(access("/sys/bus/cpu/devices/cpu0", F_OK), SyscallSucceeds());

  EXPECT_EQ(ReadSymlink("/sys/devices/system/cpu/cpu0/subsystem"), "../../../../bus/cpu");
  EXPECT_EQ(ReadSymlink("/sys/bus/cpu/devices/cpu0"), "../../../devices/system/cpu/cpu0");
}

TEST(SysfsHierarchyTest, BdiLifecycleOnMountAndUnmount) {
  auto root_mount_info = test_helper::ReadMountInfoLine("/");
  ASSERT_TRUE(root_mount_info.has_value());
  std::string root_bdi = root_mount_info->major_minor;
  if (test_helper::IsStarnix() || access(("/sys/class/bdi/" + root_bdi).c_str(), F_OK) == 0) {
    EXPECT_THAT(access(("/sys/class/bdi/" + root_bdi + "/read_ahead_kb").c_str(), F_OK),
                SyscallSucceeds());
    EXPECT_THAT(access(("/sys/class/bdi/" + root_bdi + "/max_ratio").c_str(), F_OK),
                SyscallSucceeds());
    EXPECT_EQ(ReadSymlink("/sys/class/bdi/" + root_bdi), "../../devices/virtual/bdi/" + root_bdi);
    EXPECT_EQ(ReadSymlink("/sys/devices/virtual/bdi/" + root_bdi + "/subsystem"),
              "../../../../class/bdi");
  }

  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Requires CAP_SYS_ADMIN to mount filesystems";
  }

  test_helper::ScopedTempDir temp_dir;
  auto tmpfs_mount_result =
      test_helper::ScopedMount::Mount("tmpfs", temp_dir.path(), "tmpfs", MS_NODEV, nullptr);
  ASSERT_THAT(tmpfs_mount_result, SyscallResultIsOk());
  test_helper::ScopedMount tmpfs_mount = std::move(tmpfs_mount_result.value());

  // tmpfs uses a no-op backing_dev_info and does not register an entry under /sys/class/bdi.
  auto tmpfs_mount_info = test_helper::ReadMountInfoLine(temp_dir.path());
  ASSERT_TRUE(tmpfs_mount_info.has_value());
  std::string tmpfs_bdi_path = "/sys/class/bdi/" + tmpfs_mount_info->major_minor;
  EXPECT_THAT(access(tmpfs_bdi_path.c_str(), F_OK), SyscallFailsWithErrno(ENOENT));

  fbl::unique_fd fuse_fd(open("/dev/fuse", O_RDWR | O_CLOEXEC));
  if (!fuse_fd.is_valid() && !test_helper::IsStarnix() && (errno == ENOENT || errno == ENODEV)) {
    GTEST_SKIP() << "/dev/fuse is not available";
  }
  ASSERT_TRUE(fuse_fd.is_valid()) << strerror(errno);

  std::string fuse_dir = temp_dir.path() + "/fuse";
  std::string options =
      "fd=" + std::to_string(fuse_fd.get()) + ",rootmode=40000,user_id=0,group_id=0";
  auto fuse_mount_result = test_helper::ScopedMount::CreateDirAndMount(
      "fuse", fuse_dir, "fuse", MS_NODEV | MS_NOSUID, options.c_str());
  ASSERT_THAT(fuse_mount_result, SyscallResultIsOk());
  test_helper::ScopedMount fuse_mount = std::move(fuse_mount_result.value());
  fuse_fd.reset();

  auto fuse_mount_info = test_helper::ReadMountInfoLine(fuse_dir);
  ASSERT_TRUE(fuse_mount_info.has_value());
  std::string bdi_name = fuse_mount_info->major_minor;
  std::string class_bdi_path = "/sys/class/bdi/" + bdi_name;
  std::string virtual_bdi_path = "/sys/devices/virtual/bdi/" + bdi_name;

  EXPECT_THAT(access((class_bdi_path + "/read_ahead_kb").c_str(), F_OK), SyscallSucceeds());
  EXPECT_THAT(access((class_bdi_path + "/max_ratio").c_str(), F_OK), SyscallSucceeds());
  EXPECT_EQ(ReadSymlink(class_bdi_path), "../../devices/virtual/bdi/" + bdi_name);
  EXPECT_EQ(ReadSymlink(virtual_bdi_path + "/subsystem"), "../../../../class/bdi");

  fuse_mount.Unmount();

  EXPECT_THAT(access(class_bdi_path.c_str(), F_OK), SyscallFailsWithErrno(ENOENT));
  EXPECT_THAT(access(virtual_bdi_path.c_str(), F_OK), SyscallFailsWithErrno(ENOENT));
}

class TracefsTest : public ::testing::Test {
 protected:
  void SetUp() override {
    if (!test_helper::HasSysAdmin()) {
      GTEST_SKIP() << "Requires CAP_SYS_ADMIN to mount tracefs";
    }
    auto mount_result =
        test_helper::ScopedMount::Mount("tracefs", temp_dir_.path(), "tracefs", MS_NODEV, nullptr);
    if (mount_result.is_error() && mount_result.error_value() == ENODEV) {
      GTEST_SKIP() << "tracefs not supported by kernel";
    }
    ASSERT_THAT(mount_result, SyscallResultIsOk());
    mount_ = std::move(mount_result.value());
  }

  std::string Path(std::string_view relative_path) const {
    return temp_dir_.path() + "/" + std::string(relative_path);
  }

  static fbl::unique_fd OpenForWrite(const std::string& path) {
    return fbl::unique_fd(open(path.c_str(), O_WRONLY | O_TRUNC));
  }

  static bool WriteString(const std::string& path, std::string_view value) {
    return files::WriteFile(path, value.data(), value.size());
  }

  static std::optional<size_t> ReadSizeKb(const std::string& path) {
    std::string content;
    if (!files::ReadFileToString(path, &content)) {
      return std::nullopt;
    }
    size_t value = 0;
    const char* begin = content.data();
    const char* end = begin + content.size();
    auto [ptr, ec] = std::from_chars(begin, end, value);
    if (ec != std::errc() || ptr == begin) {
      return std::nullopt;
    }
    return value;
  }

 private:
  test_helper::ScopedTempDir temp_dir_;
  test_helper::ScopedMount mount_;
};

TEST_F(TracefsTest, BufferSizeKbReadWrite) {
  ASSERT_TRUE(WriteString(Path("tracing_on"), "0\n"));

  auto initial_kb = ReadSizeKb(Path("buffer_size_kb"));
  ASSERT_TRUE(initial_kb.has_value());
  EXPECT_GT(*initial_kb, 0u);

  // Writing a valid size succeeds and updates buffer_size_kb.
  {
    fbl::unique_fd fd = OpenForWrite(Path("buffer_size_kb"));
    ASSERT_TRUE(fd.is_valid()) << strerror(errno);
    EXPECT_THAT(write(fd.get(), "4096\n", 5), SyscallSucceedsWithValue(5));
  }
  std::string content;
  ASSERT_TRUE(files::ReadFileToString(Path("buffer_size_kb"), &content));
  EXPECT_THAT(content, ContainsRegex("^[0-9]+\n$"));
  auto size_4096 = ReadSizeKb(Path("buffer_size_kb"));
  ASSERT_TRUE(size_4096.has_value());
  // Linux divides by usable subbuffer capacity (PAGE_SIZE - 16), reading back 4099 KB on 4K pages.
  EXPECT_GE(*size_4096, 4096u);
  EXPECT_LE(*size_4096, 4160u);

  // Writing a small size succeeds and clamps up to the minimum ring buffer size.
  {
    fbl::unique_fd fd = OpenForWrite(Path("buffer_size_kb"));
    ASSERT_TRUE(fd.is_valid()) << strerror(errno);
    EXPECT_THAT(write(fd.get(), "4\n", 2), SyscallSucceedsWithValue(2));
  }
  auto clamped_kb = ReadSizeKb(Path("buffer_size_kb"));
  ASSERT_TRUE(clamped_kb.has_value());
  EXPECT_GT(*clamped_kb, 4u);
  EXPECT_LE(*clamped_kb, 64u);

  // Writing 0 fails with EINVAL.
  {
    fbl::unique_fd fd = OpenForWrite(Path("buffer_size_kb"));
    ASSERT_TRUE(fd.is_valid()) << strerror(errno);
    EXPECT_THAT(write(fd.get(), "0\n", 2), SyscallFailsWithErrno(EINVAL));
  }

  // Writing non-numeric input fails with EINVAL.
  {
    fbl::unique_fd fd = OpenForWrite(Path("buffer_size_kb"));
    ASSERT_TRUE(fd.is_valid()) << strerror(errno);
    EXPECT_THAT(write(fd.get(), "invalid\n", 8), SyscallFailsWithErrno(EINVAL));
  }

  // Resizing while tracing_on is enabled succeeds.
  ASSERT_TRUE(WriteString(Path("tracing_on"), "1\n"));
  {
    fbl::unique_fd fd = OpenForWrite(Path("buffer_size_kb"));
    ASSERT_TRUE(fd.is_valid()) << strerror(errno);
    EXPECT_THAT(write(fd.get(), "2048\n", 5), SyscallSucceedsWithValue(5));
  }
  auto size_2048 = ReadSizeKb(Path("buffer_size_kb"));
  ASSERT_TRUE(size_2048.has_value());
  EXPECT_GE(*size_2048, 2048u);
  EXPECT_LE(*size_2048, 2112u);

  ASSERT_TRUE(WriteString(Path("tracing_on"), "0\n"));
}

TEST_F(TracefsTest, TracePipeRawReadsPartialPage) {
  ASSERT_TRUE(WriteString(Path("tracing_on"), "1\n"));
  auto stop_tracing = fit::defer([this] { WriteString(Path("tracing_on"), "0\n"); });

  // One small event leaves the page it lands on far from full.
  constexpr std::string_view kMarker = "trace_pipe_raw_partial_page_marker";
  {
    fbl::unique_fd fd = OpenForWrite(Path("trace_marker"));
    ASSERT_TRUE(fd.is_valid()) << strerror(errno);
    ASSERT_THAT(write(fd.get(), kMarker.data(), kMarker.size()),
                SyscallSucceedsWithValue(static_cast<ssize_t>(kMarker.size())));
  }

  // The event can be on any CPU's buffer. Drain every buffer until it is empty.
  std::vector<std::string> cpus;
  ASSERT_TRUE(files::ReadDirContents(Path("per_cpu"), &cpus));
  std::vector<char> page(static_cast<size_t>(sysconf(_SC_PAGESIZE)));
  bool found = false;
  for (const std::string& cpu : cpus) {
    if (cpu == "." || cpu == "..") {
      continue;
    }
    std::string path = Path("per_cpu/" + cpu + "/trace_pipe_raw");
    fbl::unique_fd fd(open(path.c_str(), O_RDONLY | O_NONBLOCK));
    ASSERT_TRUE(fd.is_valid()) << path << ": " << strerror(errno);
    ssize_t bytes_read;
    while ((bytes_read = read(fd.get(), page.data(), page.size())) > 0) {
      found |= std::string_view(page.data(), bytes_read).find(kMarker) != std::string_view::npos;
    }
    EXPECT_THAT(bytes_read, SyscallFailsWithErrno(EAGAIN)) << path;
  }
  EXPECT_TRUE(found) << "no trace_pipe_raw read returned the partial page with the marker";
}

TEST_F(TracefsTest, StubTracepointEventIds) {
  struct TracepointIdTestCase {
    std::string_view relative_path;
    bool required_on_linux;
  };
  constexpr TracepointIdTestCase kTracepointIdCases[] = {
      // Standard upstream Linux tracepoints expected whenever tracefs is mounted.
      {"events/vmscan/mm_vmscan_direct_reclaim_begin/id", true},
      {"events/vmscan/mm_vmscan_direct_reclaim_end/id", true},
      {"events/vmscan/mm_vmscan_kswapd_wake/id", true},
      {"events/vmscan/mm_vmscan_kswapd_sleep/id", true},
      {"events/oom/mark_victim/id", true},
      {"events/sched/sched_switch/id", true},
      // Optional on Linux (requires CONFIG_FUNCTION_TRACER=y).
      {"events/ftrace/function/id", false},
      // Android GKI vendor hook and Android Common Kernel tracepoints absent on upstream Linux.
      {"events/android_vendor_lmk/android_trigger_vendor_lmk_kill/id", false},
      {"events/kmem/mm_calculate_totalreserve_pages/id", false},
  };

  std::set<std::string> seen_tracepoint_ids;
  for (const TracepointIdTestCase& test_case : kTracepointIdCases) {
    std::string full_path = Path(test_case.relative_path);
    if (!test_helper::IsStarnix() && !test_case.required_on_linux &&
        access(full_path.c_str(), F_OK) == -1) {
      EXPECT_EQ(errno, ENOENT) << "Path: " << test_case.relative_path;
      continue;
    }

    std::string content;
    ASSERT_TRUE(files::ReadFileToString(full_path, &content))
        << "Failed to read " << test_case.relative_path;
    ASSERT_THAT(content, ContainsRegex("^[1-9][0-9]*\n$")) << "Path: " << test_case.relative_path;
    EXPECT_TRUE(seen_tracepoint_ids.insert(content).second)
        << "Duplicate tracepoint ID at " << test_case.relative_path << ": " << content;
  }
}
