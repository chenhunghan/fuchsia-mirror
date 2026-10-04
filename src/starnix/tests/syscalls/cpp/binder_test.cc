// Copyright 2025 The Fuchsia Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <lib/fit/defer.h>
#include <lib/fit/function.h>
#include <poll.h>
#include <stdint.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <format>
#include <string>
#include <thread>
#include <vector>

#include <fbl/unique_fd.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <linux/android/binder.h>
#include <linux/android/binderfs.h>
#include <linux/netlink.h>

#include "src/starnix/tests/syscalls/cpp/binder/common.h"
#include "src/starnix/tests/syscalls/cpp/binder/manager_provider_client_test.h"
#include "src/starnix/tests/syscalls/cpp/binder_helper.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

bool skip_binder_tests = false;

class BinderTest : public ::testing::Test {
 public:
  static void SetUpTestSuite() {
    // The unshare() call will isolate the mount namespaces for the running
    // test process. This allows the Linux-based tests to execute syscalls with
    // root permissions, without fear of messing the environment up. While the
    // Starnix tests don't strictly need to unshare, it's beneficial to run the
    // same test binaries on Linux and on Starnix so we can be sure the semantics
    // match. As a side effect, this means that the mounted directories will not
    // be viewable in traditional ways, e.g. ffx component explore.
    // TODO(https://fxbug.dev/317285180) don't skip on baseline
    int rv = unshare(CLONE_NEWNS);
    if (rv == -1 && errno == EPERM) {
      // GTest does not support GTEST_SKIP() from a suite setup, so record that we want to skip
      // every test here and skip in SetUp().
      skip_binder_tests = true;
      return;
    }
    ASSERT_EQ(rv, 0) << "unshare(CLONE_NEWNS) failed: " << strerror(errno) << "(" << errno << ")";
  }

  void SetUp() override {
    if (skip_binder_tests) {
      GTEST_SKIP() << "Permission denied for unshare(CLONE_NEWNS), skipping suite.";
    }

    ASSERT_FALSE(temp_dir_.path().empty());

    // Listen to uevents during mount.
    uevent_fd = fbl::unique_fd(socket(AF_NETLINK, SOCK_RAW, NETLINK_KOBJECT_UEVENT));
    ASSERT_TRUE(uevent_fd) << strerror(errno);

    int buf_sz = 16 * 1024 * 1024;
    ASSERT_THAT(setsockopt(uevent_fd.get(), SOL_SOCKET, SO_RCVBUF, &buf_sz, sizeof(buf_sz)),
                SyscallSucceeds());

    struct sockaddr_nl addr = {
        .nl_family = AF_NETLINK,
        .nl_pid = 0,
        .nl_groups = 0xffffffff,
    };
    ASSERT_THAT(bind(uevent_fd.get(), (struct sockaddr*)&addr, sizeof(addr)), SyscallSucceeds());

    // Mount sysfs to check for device presence.
    ASSERT_THAT(mkdir(TestPath("sys").c_str(), 0o700), SyscallSucceeds());
    ASSERT_THAT(mount("sysfs", TestPath("sys").c_str(), "sysfs", 0, nullptr), SyscallSucceeds());

    ASSERT_THAT(mkdir(TestPath("binderfs").c_str(), 0o700), SyscallSucceeds());
    if (mount("binder", TestPath("binderfs").c_str(), "binder", 0, nullptr) < 0) {
      ASSERT_EQ(errno, ENODEV);
      GTEST_SKIP() << "binderfs is not available, skipping test.";
    }
  }

  std::string TestPath(const char* path) const { return temp_dir_.path() + "/" + path; }

  ::testing::AssertionResult NoUeventReceived() {
    char buffer[4096];
    ssize_t bytes = recv(uevent_fd.get(), buffer, sizeof(buffer), MSG_DONTWAIT);
    if (bytes != -1) {
      return ::testing::AssertionFailure() << "Received uevent";
    } else if (errno != EAGAIN && errno != EWOULDBLOCK) {
      return ::testing::AssertionFailure() << "Recv failed: " << strerror(errno);
    } else {
      return ::testing::AssertionSuccess();
    }
  }

 private:
  test_helper::ScopedTempDir temp_dir_;
  fbl::unique_fd uevent_fd;
};

TEST_F(BinderTest, NoUeventOnMount) { ASSERT_TRUE(NoUeventReceived()); }

TEST_F(BinderTest, SetContextMgrWithNull) {
  fbl::unique_fd binder =
      fbl::unique_fd(open(TestPath("binderfs/binder").c_str(), O_RDWR | O_CLOEXEC));
  ASSERT_TRUE(binder) << strerror(errno);

  EXPECT_THAT(ioctl(binder.get(), BINDER_SET_CONTEXT_MGR, 0), SyscallSucceeds());
}

TEST_F(BinderTest, InvalidCommandError) {
  fbl::unique_fd binder =
      fbl::unique_fd(open(TestPath("binderfs/binder").c_str(), O_RDWR | O_CLOEXEC));
  ASSERT_TRUE(binder) << strerror(errno);

  uint32_t writebuf[1];
  writebuf[0] = -1;
  struct binder_write_read bwr = {};
  bwr.write_buffer = (binder_uintptr_t)writebuf;
  bwr.write_size = sizeof(uint32_t);
  bwr.write_consumed = 0;

  ASSERT_THAT(ioctl(binder.get(), BINDER_WRITE_READ, &bwr), SyscallFailsWithErrno(EINVAL));

  // The failing command is not consumed.
  EXPECT_EQ(bwr.write_consumed, size_t(0));
}

TEST_F(BinderTest, ValidThenInvalidCommand) {
  fbl::unique_fd binder =
      fbl::unique_fd(open(TestPath("binderfs/binder").c_str(), O_RDWR | O_CLOEXEC));
  ASSERT_TRUE(binder) << strerror(errno);

  uint32_t writebuf[2];
  writebuf[0] = BC_ENTER_LOOPER;
  writebuf[1] = -1;
  struct binder_write_read bwr = {};
  bwr.write_buffer = (binder_uintptr_t)writebuf;
  bwr.write_size = 2 * sizeof(uint32_t);
  bwr.write_consumed = 0;

  ASSERT_THAT(ioctl(binder.get(), BINDER_WRITE_READ, &bwr), SyscallFailsWithErrno(EINVAL));

  // The first command is consumed.
  EXPECT_EQ(bwr.write_consumed, sizeof(uint32_t));
}

TEST_F(BinderTest, IgnoreAlreadyConsumed) {
  fbl::unique_fd binder =
      fbl::unique_fd(open(TestPath("binderfs/binder").c_str(), O_RDWR | O_CLOEXEC));
  ASSERT_TRUE(binder) << strerror(errno);

  uint32_t writebuf[2];
  writebuf[0] = -1;
  writebuf[1] = BC_ENTER_LOOPER;
  struct binder_write_read bwr = {};
  bwr.write_buffer = (binder_uintptr_t)writebuf;
  bwr.write_size = 2 * sizeof(uint32_t);
  bwr.write_consumed = sizeof(uint32_t);

  ASSERT_THAT(ioctl(binder.get(), BINDER_WRITE_READ, &bwr), SyscallSucceeds());

  EXPECT_EQ(bwr.write_consumed, 2 * sizeof(uint32_t));
  EXPECT_EQ(bwr.write_size, 2 * sizeof(uint32_t));

  // We can reuse the structure for the next write, no command will be executed.
  ASSERT_THAT(ioctl(binder.get(), BINDER_WRITE_READ, &bwr), SyscallSucceeds());
}

TEST_F(BinderTest, BinderControl) {
  fbl::unique_fd binder_control =
      fbl::unique_fd(open(TestPath("binderfs/binder-control").c_str(), O_RDWR | O_CLOEXEC));
  ASSERT_TRUE(binder_control) << strerror(errno);

  struct binderfs_device new_device = {};
  std::string kBinderName = "binder-test";
  std::ranges::copy(kBinderName, new_device.name);
  EXPECT_THAT(ioctl(binder_control.get(), BINDER_CTL_ADD, &new_device), SyscallSucceeds());

  // The ioctl set the correct major and minor numbers.
  struct stat sb = {};
  EXPECT_THAT(stat(TestPath("binderfs/binder-test").c_str(), &sb), SyscallSucceeds());
  EXPECT_EQ(sb.st_mode, S_IFCHR | 0o600u);
  EXPECT_EQ(major(sb.st_rdev), new_device.major);
  EXPECT_EQ(minor(sb.st_rdev), new_device.minor);

  // The result is a usable binder device.
  fbl::unique_fd binder =
      fbl::unique_fd(open(TestPath("binderfs/binder-test").c_str(), O_RDWR | O_CLOEXEC));
  ASSERT_TRUE(binder) << strerror(errno);
  struct binder_version version = {};
  EXPECT_THAT(ioctl(binder.get(), BINDER_VERSION, &version), SyscallSucceeds());

  EXPECT_TRUE(NoUeventReceived());
  // sysfs has entries for usual devices (1:3 is /dev/null)
  EXPECT_THAT(access(TestPath("sys/dev/char/1:3/").c_str(), F_OK), SyscallSucceeds());
  // It doesn't have an entry for our new binder device.
  EXPECT_THAT(
      access(std::format("{}/{}:{}", TestPath("sys/dev/char"), new_device.major, new_device.minor)
                 .c_str(),
             F_OK),
      SyscallFailsWithErrno(ENOENT));
  // It doesn't have an entry for binder-control.
  EXPECT_THAT(fstat(binder_control.get(), &sb), SyscallSucceeds());
  EXPECT_THAT(
      access(std::format("{}/{}:{}", TestPath("sys/dev/char"), major(sb.st_rdev), minor(sb.st_rdev))
                 .c_str(),
             F_OK),
      SyscallFailsWithErrno(ENOENT));
}

TEST_F(BinderTest, BinderControlExists) {
  fbl::unique_fd binder_control =
      fbl::unique_fd(open(TestPath("binderfs/binder-control").c_str(), O_RDWR | O_CLOEXEC));
  ASSERT_TRUE(binder_control) << strerror(errno);

  struct binderfs_device new_device = {};
  std::string kBinderName = "binder";
  std::ranges::copy(kBinderName, new_device.name);
  EXPECT_THAT(ioctl(binder_control.get(), BINDER_CTL_ADD, &new_device),
              SyscallFailsWithErrno(EEXIST));
}

TEST_F(BinderTest, BinderControlInvalidPathDot) {
  fbl::unique_fd binder_control =
      fbl::unique_fd(open(TestPath("binderfs/binder-control").c_str(), O_RDWR | O_CLOEXEC));
  ASSERT_TRUE(binder_control) << strerror(errno);

  struct binderfs_device new_device = {};
  std::string kBinderName = ".";
  std::ranges::copy(kBinderName, new_device.name);
  EXPECT_THAT(ioctl(binder_control.get(), BINDER_CTL_ADD, &new_device),
              SyscallFailsWithErrno(EACCES));
}

TEST_F(BinderTest, BinderControlInvalidPathDotDot) {
  fbl::unique_fd binder_control =
      fbl::unique_fd(open(TestPath("binderfs/binder-control").c_str(), O_RDWR | O_CLOEXEC));
  ASSERT_TRUE(binder_control) << strerror(errno);

  struct binderfs_device new_device = {};
  std::string kBinderName = "..";
  std::ranges::copy(kBinderName, new_device.name);
  EXPECT_THAT(ioctl(binder_control.get(), BINDER_CTL_ADD, &new_device),
              SyscallFailsWithErrno(EACCES));
}

TEST_F(BinderTest, BinderControlInvalidPathEmpty) {
  fbl::unique_fd binder_control =
      fbl::unique_fd(open(TestPath("binderfs/binder-control").c_str(), O_RDWR | O_CLOEXEC));
  ASSERT_TRUE(binder_control) << strerror(errno);

  struct binderfs_device new_device = {};
  std::string kBinderName;
  std::ranges::copy(kBinderName, new_device.name);
  EXPECT_THAT(ioctl(binder_control.get(), BINDER_CTL_ADD, &new_device),
              SyscallFailsWithErrno(EACCES));
}

TEST_F(BinderTest, BinderControlInvalidPathReserved) {
  fbl::unique_fd binder_control =
      fbl::unique_fd(open(TestPath("binderfs/binder-control").c_str(), O_RDWR | O_CLOEXEC));
  ASSERT_TRUE(binder_control) << strerror(errno);

  struct binderfs_device new_device = {};
  std::string kBinderName = "binder-control";
  std::ranges::copy(kBinderName, new_device.name);
  EXPECT_THAT(ioctl(binder_control.get(), BINDER_CTL_ADD, &new_device),
              SyscallFailsWithErrno(EEXIST));
}

TEST_F(BinderTest, BinderControlInvalidPathSlash) {
  fbl::unique_fd binder_control =
      fbl::unique_fd(open(TestPath("binderfs/binder-control").c_str(), O_RDWR | O_CLOEXEC));
  ASSERT_TRUE(binder_control) << strerror(errno);

  struct binderfs_device new_device = {};
  std::string kBinderName = "my/binder";
  std::ranges::copy(kBinderName, new_device.name);
  EXPECT_THAT(ioctl(binder_control.get(), BINDER_CTL_ADD, &new_device),
              SyscallFailsWithErrno(EACCES));
}

// This test verifies that sending a file descriptor to a process that is in the middle of exiting
// does not crash the kernel or return a protocol error. Instead, it should fail gracefully,
// eventually returning BR_DEAD_REPLY or BR_FAILED_REPLY.
TEST_F(BinderTest, SendFdToExitingProcess) {
  using namespace starnix_binder;

  auto manager_ready = test_helper::MakeRendezvous();
  pid_t manager_pid = 0;

  // Spawn the manager using the framework helper.
  auto manager = ManagerProcess(
      TestPath("binderfs"),
      [&](test_helper::ForkHelper& fork_helper, fit::closure manager_behavior) {
        manager_pid = fork_helper.RunInForkedProcess(std::move(manager_behavior));
        return manager_pid;
      },
      std::move(manager_ready.poker));

  manager_ready.holder.hold();

  auto binder_and_map = OpenBinderAndMap(TestPath("binderfs"));
  ASSERT_TRUE(binder_and_map.fd_);
  ASSERT_THAT(binder_and_map.mapping_, SyscallResultIsOk());
  const auto& binder = binder_and_map.fd_;

  // Prepare transaction with FD.
  fbl::unique_fd fd_to_send(SAFE_SYSCALL(open("/dev/null", O_RDONLY)));
  FdTransaction fd_transaction(0, kServiceSendFd, fd_to_send.get());
  const auto& write_buffer = fd_transaction.write_buffer;

  // Spawn a background thread that continuously sends the transaction with the FD to the manager.
  // The transaction is two-way and the manager does not reply, so the thread will block in ioctl()
  // until the manager process is killed.
  auto send_started = test_helper::MakeRendezvous();
  std::atomic<int> ioctl_errno(0);
  std::atomic<bool> got_error(false);
  std::atomic<bool> got_dead_or_failed_reply(false);
  std::thread send_thread([&, send_started = std::move(send_started.poker)]() mutable {
    bool starting = true;
    while (true) {
      struct binder_write_read bwr = {};
      bwr.write_buffer = (binder_uintptr_t)&write_buffer;
      bwr.write_size = sizeof(write_buffer);
      bwr.write_consumed = 0;

      uint32_t read_buf[32] = {};
      bwr.read_buffer = (binder_uintptr_t)read_buf;
      bwr.read_size = sizeof(read_buf);
      bwr.read_consumed = 0;

      // Wait to poke send_started until we are about to enter ioctl().
      if (starting) {
        starting = false;
        send_started.poke();
      }
      if (ioctl(binder.get(), BINDER_WRITE_READ, &bwr) < 0) {
        ioctl_errno = errno;
        break;
      }

      ParsedMessage pm = ParseMessage((binder_uintptr_t)read_buf, bwr.read_consumed);
      for (auto cmd : pm.returns_) {
        if (cmd == BR_ERROR) {
          got_error = true;
          return;
        }
        if (cmd == BR_DEAD_REPLY || cmd == BR_FAILED_REPLY) {
          got_dead_or_failed_reply = true;
          return;
        }
      }
    }
  });

  send_started.holder.hold();

  // Kill the manager and wait for the send thread to finish.
  manager.call();
  send_thread.join();

  // We expect the transactions to fail with BR_DEAD_REPLY or BR_FAILED_REPLY after the child died.
  EXPECT_FALSE(got_error.load());
  EXPECT_TRUE(got_dead_or_failed_reply.load());
  EXPECT_THAT(ioctl_errno.load(), SyscallSucceedsWithValue(0));
}

// Linux's binder driver rejects mmap with PROT_WRITE and clears VM_MAYWRITE to
// prevent mprotect from adding write permission. Verify Starnix matches this
// behavior.
TEST_F(BinderTest, MmapRejectsProtWrite) {
  using namespace starnix_binder;
  fbl::unique_fd binder =
      fbl::unique_fd(open(TestPath("binderfs/binder").c_str(), O_RDWR | O_CLOEXEC));
  ASSERT_TRUE(binder) << strerror(errno);

  auto mapping = test_helper::ScopedMMap::MMap(nullptr, kBinderMMapSize, PROT_READ | PROT_WRITE,
                                               MAP_PRIVATE, binder.get(), 0);
  ASSERT_TRUE(mapping.is_error()) << "mmap with PROT_WRITE should fail with EPERM";
  EXPECT_EQ(mapping.error_value(), EPERM) << strerror(mapping.error_value());
}

TEST_F(BinderTest, MprotectCannotAddWriteToBinder) {
  using namespace starnix_binder;
  fbl::unique_fd binder =
      fbl::unique_fd(open(TestPath("binderfs/binder").c_str(), O_RDWR | O_CLOEXEC));
  ASSERT_TRUE(binder) << strerror(errno);

  auto mapping = test_helper::ScopedMMap::MMap(nullptr, kBinderMMapSize, PROT_READ, MAP_PRIVATE,
                                               binder.get(), 0);
  ASSERT_TRUE(mapping.is_ok()) << mapping.error_value();

  EXPECT_THAT(mprotect(mapping->mapping(), kBinderMMapSize, PROT_READ | PROT_WRITE),
              SyscallFailsWithErrno(EACCES))
      << "mprotect should not be able to add PROT_WRITE to a binder mapping";
}

// In Linux Binder, binder objects are uniquely identified by the userspace pointer (ptr).
// If userspace sends a binder object with an address that is already registered with a
// different cookie, the driver detects a cookie mismatch and fails the transaction with
// BR_FAILED_REPLY.
TEST_F(BinderTest, BinderObjectCookieMismatch) {
  using namespace starnix_binder;

  auto receiver_ready = test_helper::MakeRendezvous();
  test_helper::ForkHelper fork_helper;
  fork_helper.OnlyWaitForForkedChildren();

  pid_t receiver_pid = fork_helper.RunInForkedProcess([&] {
    auto fd_and_mapping = OpenBinderAndMap(TestPath("binderfs"));
    ASSERT_THAT(ioctl(fd_and_mapping.fd_.get(), BINDER_SET_CONTEXT_MGR, 0), SyscallSucceeds());
    EnterLooper(fd_and_mapping.fd_);
    receiver_ready.poker.poke();
    while (true) {
      std::array<uint32_t, 32> read_buffer = {};
      struct binder_write_read write_read = {
          .read_size = sizeof(read_buffer),
          .read_consumed = 0,
          .read_buffer = (binder_uintptr_t)read_buffer.data(),
      };
      if (ioctl(fd_and_mapping.fd_.get(), BINDER_WRITE_READ, &write_read) < 0) {
        break;
      }
    }
  });

  receiver_ready.holder.hold();

  auto cleanup_receiver = fit::defer([&] {
    ASSERT_THAT(kill(receiver_pid, SIGKILL), SyscallSucceeds());
    fork_helper.ExpectSignal(SIGKILL);
    ASSERT_TRUE(fork_helper.WaitForChildren());
  });

  auto binder_and_map = OpenBinderAndMap(TestPath("binderfs"));
  ASSERT_TRUE(binder_and_map.fd_);
  ASSERT_THAT(binder_and_map.mapping_, SyscallResultIsOk());
  const auto& binder = binder_and_map.fd_;

  uintptr_t binder_ptr = 0x12345000;
  uintptr_t cookie1 = 0xaaaa0000;
  uintptr_t cookie2 = 0xbbbb0000;

  auto send_binder_object = [&](uintptr_t ptr, uintptr_t cookie) {
    struct flat_binder_object obj = {
        .hdr = {.type = BINDER_TYPE_BINDER},
        .flags = 0x7f | FLAT_BINDER_FLAG_ACCEPTS_FDS,
        .binder = ptr,
        .cookie = cookie,
    };
    binder_size_t offset = 0;
    TransactionWriteBuffer write_buffer = {
        .command = BC_TRANSACTION,
        .data =
            {
                .target = {.handle = kServiceManagerHandle},
                .cookie = 0,
                .code = 1,
                .flags = TF_ONE_WAY,
                .data_size = sizeof(obj),
                .offsets_size = sizeof(offset),
                .data =
                    {
                        .ptr =
                            {
                                .buffer = (binder_uintptr_t)&obj,
                                .offsets = (binder_uintptr_t)&offset,
                            },
                    },
            },
    };
    uint32_t read_buf[32] = {};
    struct binder_write_read bwr = {
        .write_size = sizeof(write_buffer),
        .write_buffer = (binder_uintptr_t)&write_buffer,
        .read_size = sizeof(read_buf),
        .read_buffer = (binder_uintptr_t)read_buf,
    };
    EXPECT_THAT(ioctl(binder.get(), BINDER_WRITE_READ, &bwr), SyscallSucceeds());
    return ParseMessage((binder_uintptr_t)read_buf, bwr.read_consumed);
  };

  // 1. Send first transaction with (binder_ptr, cookie1) to the receiver.
  ParsedMessage pm1 = send_binder_object(binder_ptr, cookie1);

  // When a new binder object is registered, the driver informs userspace that a reference
  // was acquired (BR_ACQUIRE), followed by transaction completion (BR_TRANSACTION_COMPLETE).
  if (std::find(pm1.returns_.begin(), pm1.returns_.end(), BR_TRANSACTION_COMPLETE) ==
      pm1.returns_.end()) {
    EXPECT_THAT(pm1.returns_, testing::Contains(BR_ACQUIRE));
    uint32_t read_buf1_comp[32] = {};
    struct binder_write_read bwr1_comp = {
        .read_size = sizeof(read_buf1_comp),
        .read_buffer = (binder_uintptr_t)read_buf1_comp,
    };
    ASSERT_THAT(ioctl(binder.get(), BINDER_WRITE_READ, &bwr1_comp), SyscallSucceeds());
    ParsedMessage pm1_comp =
        ParseMessage((binder_uintptr_t)read_buf1_comp, bwr1_comp.read_consumed);
    EXPECT_THAT(pm1_comp.returns_, testing::Contains(BR_TRANSACTION_COMPLETE));
  } else {
    EXPECT_THAT(pm1.returns_, testing::Contains(BR_TRANSACTION_COMPLETE));
  }
  EXPECT_THAT(pm1.returns_, testing::Not(testing::Contains(BR_FAILED_REPLY)));

  // 2. Send second transaction with the SAME binder_ptr, but DIFFERENT cookie2.
  // This must fail with BR_FAILED_REPLY due to cookie mismatch.
  ParsedMessage pm2 = send_binder_object(binder_ptr, cookie2);
  EXPECT_THAT(pm2.returns_, testing::Contains(BR_FAILED_REPLY));
  EXPECT_THAT(pm2.returns_, testing::Not(testing::Contains(BR_TRANSACTION_COMPLETE)));

  // 3. Send third transaction with the SAME binder_ptr and matching cookie1.
  // This should succeed.
  ParsedMessage pm3 = send_binder_object(binder_ptr, cookie1);
  EXPECT_THAT(pm3.returns_, testing::Contains(BR_TRANSACTION_COMPLETE));
  EXPECT_THAT(pm3.returns_, testing::Not(testing::Contains(BR_FAILED_REPLY)));
}

// Only the process that opened a binder FD may mmap it, regardless of whether the mapping is
// private or shared.
TEST_F(BinderTest, CrossProcessMmapFails) {
  using namespace starnix_binder;
  for (int flags : {MAP_PRIVATE, MAP_SHARED}) {
    SCOPED_TRACE(flags == MAP_PRIVATE ? "MAP_PRIVATE" : "MAP_SHARED");

    // Each binder FD can only be mapped once, so open a new one for each mapping type.
    fbl::unique_fd binder =
        fbl::unique_fd(open(TestPath("binderfs/binder").c_str(), O_RDWR | O_CLOEXEC));
    ASSERT_TRUE(binder) << strerror(errno);

    test_helper::ForkHelper helper;
    helper.RunInForkedProcess([&] {
      // Mapping a binder FD from a process other than the one that opened it must fail with
      // EINVAL.
      EXPECT_THAT(test_helper::ScopedMMap::MMap(nullptr, kBinderMMapSize, PROT_READ, flags,
                                                binder.get(), 0),
                  SyscallResultIsErrno(EINVAL));
    });
    ASSERT_TRUE(helper.WaitForChildren());

    // The process that originally opened the binder FD can still mmap it.
    EXPECT_THAT(
        test_helper::ScopedMMap::MMap(nullptr, kBinderMMapSize, PROT_READ, flags, binder.get(), 0),
        SyscallResultIsOk());
  }
}

// Unlike mmap, ioctls on a binder FD are permitted from processes other than the one that opened
// it, e.g. after the FD is inherited across fork() or passed to another process.
TEST_F(BinderTest, CrossProcessIoctlSucceeds) {
  fbl::unique_fd binder =
      fbl::unique_fd(open(TestPath("binderfs/binder").c_str(), O_RDWR | O_CLOEXEC));
  ASSERT_TRUE(binder) << strerror(errno);

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    struct binder_version version = {};
    EXPECT_THAT(ioctl(binder.get(), BINDER_VERSION, &version), SyscallSucceeds());
  });
  ASSERT_TRUE(helper.WaitForChildren());
}

// The commands returned by binder reads, except BR_NOOP, and the payloads of the returned
// BR_TRANSACTION commands.
struct BinderReadResult {
  std::vector<binder_driver_return_protocol> commands;
  std::vector<binder_transaction_data> transactions;

  bool Contains(binder_driver_return_protocol command) const {
    return std::ranges::find(commands, command) != commands.end();
  }
};

// Writes the `write_size` bytes of `write_buffer` to `binder`, and then reads from it, with a
// single BINDER_WRITE_READ.
BinderReadResult WriteRead(const fbl::unique_fd& binder, const void* write_buffer,
                           size_t write_size) {
  std::array<uint32_t, 32> read_buffer = {};
  struct binder_write_read write_read = {
      .write_size = write_size,
      .write_consumed = 0,
      .write_buffer = (binder_uintptr_t)write_buffer,
      .read_size = sizeof(read_buffer),
      .read_consumed = 0,
      .read_buffer = (binder_uintptr_t)read_buffer.data(),
  };
  EXPECT_THAT(ioctl(binder.get(), BINDER_WRITE_READ, &write_read), SyscallSucceeds());

  BinderReadResult result;
  const char* const data = reinterpret_cast<const char*>(read_buffer.data());
  size_t offset = 0;
  while (offset < write_read.read_consumed) {
    binder_driver_return_protocol command;
    memcpy(&command, data + offset, sizeof(command));
    offset += sizeof(command);
    if (command == BR_TRANSACTION) {
      binder_transaction_data transaction;
      memcpy(&transaction, data + offset, sizeof(transaction));
      result.transactions.push_back(transaction);
    }
    if (command != BR_NOOP) {
      result.commands.push_back(command);
    }
    // The size of the parameters of a command is encoded in its value.
    offset += _IOC_SIZE(command);
  }
  EXPECT_EQ(offset, write_read.read_consumed) << "binder read buffer did not parse cleanly";
  return result;
}

// Reads from `binder` until `command`, or a transaction failure, has been returned. Returns all
// the commands returned, starting with those of `result`.
BinderReadResult ReadUntil(const fbl::unique_fd& binder, BinderReadResult result,
                           binder_driver_return_protocol command) {
  while (!result.Contains(command) && !result.Contains(BR_DEAD_REPLY) &&
         !result.Contains(BR_FAILED_REPLY) && !testing::Test::HasFailure()) {
    BinderReadResult next = WriteRead(binder, nullptr, 0);
    result.commands.insert(result.commands.end(), next.commands.begin(), next.commands.end());
    result.transactions.insert(result.transactions.end(), next.transactions.begin(),
                               next.transactions.end());
  }
  return result;
}

BinderReadResult ReadUntil(const fbl::unique_fd& binder, binder_driver_return_protocol command) {
  return ReadUntil(binder, BinderReadResult(), command);
}

// Sends a two-way transaction without data to `handle`, and returns the commands read afterwards.
BinderReadResult SendTwoWayTransaction(const fbl::unique_fd& binder, uint32_t handle) {
  const starnix_binder::TransactionWriteBuffer transaction = {
      .command = BC_TRANSACTION,
      .data = {.target = {.handle = handle}},
  };
  return WriteRead(binder, &transaction, sizeof(transaction));
}

// Replies without data to the transaction being handled by the calling thread, and returns the
// commands read afterwards.
BinderReadResult SendEmptyReply(const fbl::unique_fd& binder) {
  const starnix_binder::ReplyWriteBuffer reply = {.command = BC_REPLY, .data = {}};
  return WriteRead(binder, &reply, sizeof(reply));
}

// A binder object owned by the calling process.
constexpr flat_binder_object kLocalObject = {
    .hdr = {.type = BINDER_TYPE_BINDER},
    .flags = 0x7f | FLAT_BINDER_FLAG_ACCEPTS_FDS,
    .binder = 0x1000,
    .cookie = 0x2000,
};

// Returns the data of a transaction to `handle` that only contains `*object`, at offset `*offset`.
binder_transaction_data TransactionDataWithObject(uint32_t handle, uint32_t flags,
                                                  const flat_binder_object* object,
                                                  const binder_size_t* offset) {
  return {
      .target = {.handle = handle},
      .flags = flags,
      .data_size = sizeof(*object),
      .offsets_size = sizeof(*offset),
      .data = {.ptr = {.buffer = (binder_uintptr_t)object, .offsets = (binder_uintptr_t)offset}},
  };
}

// Forks a process that becomes the context manager of the binder device in `binder_dir`, and then
// runs `serve` with its binder fd. Returns once the context manager has been set.
void ForkContextManager(test_helper::ForkHelper& fork_helper, const std::string& binder_dir,
                        fit::function<void(const fbl::unique_fd&)> serve) {
  auto ready = test_helper::MakeRendezvous();
  fork_helper.RunInForkedProcess([&] {
    auto fd_and_mapping = starnix_binder::OpenBinderAndMap(binder_dir);
    ASSERT_TRUE(fd_and_mapping.fd_);
    ASSERT_THAT(fd_and_mapping.mapping_, SyscallResultIsOk());
    ASSERT_THAT(ioctl(fd_and_mapping.fd_.get(), BINDER_SET_CONTEXT_MGR, 0), SyscallSucceeds());
    starnix_binder::EnterLooper(fd_and_mapping.fd_);
    ready.poker.poke();
    serve(fd_and_mapping.fd_);
  });
  // Close the parent's copy of the pipe's write side, so that `hold()` returns if the child exits
  // without poking.
  ready.poker = test_helper::Poker();
  ready.holder.hold();
}

// Waits until `fd` is readable, for at most 10 seconds.
void WaitUntilReadable(const fbl::unique_fd& fd) {
  struct pollfd pfd = {.fd = fd.get(), .events = POLLIN, .revents = 0};
  EXPECT_THAT(HANDLE_EINTR(poll(&pfd, 1, 10'000)), SyscallSucceedsWithValue(1));
}

// A single write/read of a two-way transaction returns both its BR_TRANSACTION_COMPLETE and its
// BR_REPLY. The BR_TRANSACTION_COMPLETE of the reply is returned on its own.
TEST_F(BinderTest, TwoWayTransactionCompleteIsReadWithReply) {
  using namespace starnix_binder;
  test_helper::ForkHelper fork_helper;
  fork_helper.OnlyWaitForForkedChildren();
  ForkContextManager(fork_helper, TestPath("binderfs"), [](const fbl::unique_fd& binder) {
    ASSERT_TRUE(ReadUntil(binder, BR_TRANSACTION).Contains(BR_TRANSACTION));
    EXPECT_THAT(SendEmptyReply(binder).commands, testing::ElementsAre(BR_TRANSACTION_COMPLETE));
  });

  auto binder_and_map = OpenBinderAndMap(TestPath("binderfs"));
  ASSERT_TRUE(binder_and_map.fd_);
  ASSERT_THAT(binder_and_map.mapping_, SyscallResultIsOk());
  EXPECT_THAT(SendTwoWayTransaction(binder_and_map.fd_, kServiceManagerHandle).commands,
              testing::ElementsAre(BR_TRANSACTION_COMPLETE, BR_REPLY));
  EXPECT_TRUE(fork_helper.WaitForChildren());
}

// A single write/read of a two-way transaction returns both its BR_TRANSACTION_COMPLETE and its
// BR_DEAD_REPLY, if the recipient exits without replying.
TEST_F(BinderTest, TwoWayTransactionCompleteIsReadWithDeadReply) {
  using namespace starnix_binder;
  test_helper::ForkHelper fork_helper;
  fork_helper.OnlyWaitForForkedChildren();
  // The context manager exits as soon as it has received the transaction.
  ForkContextManager(fork_helper, TestPath("binderfs"), [](const fbl::unique_fd& binder) {
    EXPECT_TRUE(ReadUntil(binder, BR_TRANSACTION).Contains(BR_TRANSACTION));
  });

  auto binder_and_map = OpenBinderAndMap(TestPath("binderfs"));
  ASSERT_TRUE(binder_and_map.fd_);
  ASSERT_THAT(binder_and_map.mapping_, SyscallResultIsOk());
  EXPECT_THAT(SendTwoWayTransaction(binder_and_map.fd_, kServiceManagerHandle).commands,
              testing::ElementsAre(BR_TRANSACTION_COMPLETE, BR_DEAD_REPLY));
  EXPECT_TRUE(fork_helper.WaitForChildren());
}

// When a thread replies to a transaction nested in its own two-way transaction (A -> B -> A), the
// BR_TRANSACTION_COMPLETE of its reply is returned on its own: the read does not wait for the
// reply to its own transaction.
TEST_F(BinderTest, TransactionCompleteForNestedReplyIsReadAlone) {
  using namespace starnix_binder;
  test_helper::ForkHelper fork_helper;
  fork_helper.OnlyWaitForForkedChildren();
  // Written to by A once its reply to the nested transaction has returned.
  test_helper::ScopedPipe nested_reply_sent;

  // B, the context manager.
  ForkContextManager(fork_helper, TestPath("binderfs"), [&](const fbl::unique_fd& binder) {
    // Receive A's transaction, which carries a binder object owned by A.
    BinderReadResult result = ReadUntil(binder, BR_TRANSACTION);
    ASSERT_EQ(result.transactions.size(), 1u);
    const binder_transaction_data& transaction = result.transactions[0];
    ASSERT_EQ(transaction.offsets_size, sizeof(binder_size_t));
    binder_size_t offset;
    memcpy(&offset, (const void*)transaction.data.ptr.offsets, sizeof(offset));
    flat_binder_object object;
    memcpy(&object, (const void*)(transaction.data.ptr.buffer + offset), sizeof(object));
    ASSERT_EQ(object.hdr.type, BINDER_TYPE_HANDLE);

    // Send a transaction to A's object. It is nested in A's transaction, so A's thread receives it.
    EXPECT_TRUE(ReadUntil(binder, SendTwoWayTransaction(binder, object.handle), BR_REPLY)
                    .Contains(BR_REPLY));

    // Only reply to A's transaction once A's reply to the nested transaction has returned. If that
    // reply waited for this one, the wait times out instead of deadlocking, and A reads both.
    WaitUntilReadable(nested_reply_sent.ReadSide());
    SendEmptyReply(binder);
  });

  // A.
  auto binder_and_map = OpenBinderAndMap(TestPath("binderfs"));
  ASSERT_TRUE(binder_and_map.fd_);
  ASSERT_THAT(binder_and_map.mapping_, SyscallResultIsOk());
  const auto& binder = binder_and_map.fd_;

  // Send a transaction carrying a binder object owned by A to B, and receive B's nested
  // transaction.
  const flat_binder_object object = kLocalObject;
  const binder_size_t offset = 0;
  const TransactionWriteBuffer transaction = {
      .command = BC_TRANSACTION,
      .data = TransactionDataWithObject(kServiceManagerHandle, 0, &object, &offset),
  };
  ASSERT_TRUE(
      ReadUntil(binder, WriteRead(binder, &transaction, sizeof(transaction)), BR_TRANSACTION)
          .Contains(BR_TRANSACTION));

  // Reply to the nested transaction. Only its BR_TRANSACTION_COMPLETE is returned, even though A's
  // own transaction is still waiting for its reply.
  BinderReadResult result = SendEmptyReply(binder);
  EXPECT_THAT(result.commands, testing::ElementsAre(BR_TRANSACTION_COMPLETE));

  // Let B reply to A's transaction.
  ASSERT_THAT(write(nested_reply_sent.WriteSide().get(), "", 1), SyscallSucceedsWithValue(1));
  EXPECT_TRUE(ReadUntil(binder, std::move(result), BR_REPLY).Contains(BR_REPLY));
  EXPECT_TRUE(fork_helper.WaitForChildren());
}

// The BR_TRANSACTION_COMPLETE of a two-way transaction is not lost when the same read also returns
// a refcount command queued after it.
TEST_F(BinderTest, TwoWayTransactionCompleteIsNotOverwrittenByRefcountCommand) {
  using namespace starnix_binder;
  test_helper::ForkHelper fork_helper;
  fork_helper.OnlyWaitForForkedChildren();
  // Written to once the first read has returned.
  test_helper::ScopedPipe first_read_done;
  ForkContextManager(fork_helper, TestPath("binderfs"), [&](const fbl::unique_fd& binder) {
    ASSERT_TRUE(ReadUntil(binder, BR_TRANSACTION).Contains(BR_TRANSACTION));
    // Only reply once the first read has returned, so that it cannot return the reply.
    WaitUntilReadable(first_read_done.ReadSide());
    SendEmptyReply(binder);
  });

  auto binder_and_map = OpenBinderAndMap(TestPath("binderfs"));
  ASSERT_TRUE(binder_and_map.fd_);
  ASSERT_THAT(binder_and_map.mapping_, SyscallResultIsOk());
  const auto& binder = binder_and_map.fd_;

  // With a single write, send a two-way transaction, and then a oneway transaction carrying a
  // binder object sent for the first time. The BR_ACQUIRE for the object is queued after the
  // BR_TRANSACTION_COMPLETE of the two-way transaction.
  const flat_binder_object object = kLocalObject;
  const binder_size_t offset = 0;
  const struct __attribute__((packed)) {
    TransactionWriteBuffer two_way;
    TransactionWriteBuffer oneway;
  } write_buffer = {
      .two_way = {.command = BC_TRANSACTION, .data = {.target = {.handle = kServiceManagerHandle}}},
      .oneway = {.command = BC_TRANSACTION,
                 .data = TransactionDataWithObject(kServiceManagerHandle, TF_ONE_WAY, &object,
                                                   &offset)},
  };
  BinderReadResult result = WriteRead(binder, &write_buffer, sizeof(write_buffer));
  ASSERT_THAT(write(first_read_done.WriteSide().get(), "", 1), SyscallSucceedsWithValue(1));
  result = ReadUntil(binder, std::move(result), BR_REPLY);

  // Each transaction has its BR_TRANSACTION_COMPLETE.
  EXPECT_EQ(std::ranges::count(result.commands, BR_TRANSACTION_COMPLETE), 2);
  EXPECT_TRUE(result.Contains(BR_ACQUIRE));
  EXPECT_TRUE(result.Contains(BR_REPLY));
  EXPECT_TRUE(fork_helper.WaitForChildren());
}

}  // namespace

namespace starnix_binder {

class WithoutSEStarnix : public WithOrWithoutSEStarnix {
 public:
  pid_t SpawnManager(test_helper::ForkHelper& fork_helper, fit::closure manager_behavior) override {
    return fork_helper.RunInForkedProcess(std::move(manager_behavior));
  }
  pid_t SpawnProvider(test_helper::ForkHelper& fork_helper,
                      fit::closure provider_behavior) override {
    return fork_helper.RunInForkedProcess(std::move(provider_behavior));
  }
  pid_t SpawnClient(test_helper::ForkHelper& fork_helper, fit::closure client_behavior) override {
    return fork_helper.RunInForkedProcess(std::move(client_behavior));
  }
  void ValidateClientSecctxSeenByProvider(std::string_view secctx) override {
    // Nothing to validate here in the Not-SE flavor.
  }
  bool SkipEntirely() override { return !test_helper::IsStarnix(); }
};

INSTANTIATE_TYPED_TEST_SUITE_P(BinderWithoutSEStarnix, ManagerProviderClientTest, WithoutSEStarnix);

}  // namespace starnix_binder
