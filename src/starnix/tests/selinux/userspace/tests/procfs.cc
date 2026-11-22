// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <lib/fit/defer.h>
#include <signal.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

#include <string>
#include <string_view>
#include <thread>
#include <utility>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/lib/files/file.h"
#include "src/lib/fxl/strings/string_printf.h"
#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

extern std::string GetTestPolicyName() { return "procfs_policy"; }

namespace {

// Attempting to read the process' current context should return a value.
TEST(ProcAttrTest, Current) {
  EXPECT_THAT(ReadTaskAttr("current"), SyscallResultIsOk("system_u:unconfined_r:unconfined_t:s0"));
}

TEST(ProcAttrTest, AttrsAreSeekable) {
  auto current_attr = fbl::unique_fd(open("/proc/self/attr/current", O_RDONLY));
  ASSERT_TRUE(current_attr.is_valid()) << strerror(errno);

  // Seek position is retained, so it is possible to read the value in chunks.
  std::string context;
  for (;;) {
    char buf[2];
    auto result = read(current_attr.get(), buf, sizeof(buf));
    ASSERT_GE(result, 0) << strerror(errno);
    if (result == 0) {
      break;
    }
    context.append(buf, result);
  }

  EXPECT_EQ(RemoveTrailingNul(context), "system_u:unconfined_r:unconfined_t:s0");

  // It is possible to seek back to the start of the file and read it all again.
  auto result = lseek(current_attr.get(), 0, SEEK_SET);
  ASSERT_EQ(result, 0) << strerror(errno);
  ASSERT_TRUE(files::ReadFileDescriptorToString(current_attr.get(), &context));
  EXPECT_EQ(RemoveTrailingNul(context), "system_u:unconfined_r:unconfined_t:s0");

  // It is possible to seek into the middle and read from there.
  result = lseek(current_attr.get(), 9, SEEK_SET);
  ASSERT_EQ(result, 9) << strerror(errno);
  ASSERT_TRUE(files::ReadFileDescriptorToString(current_attr.get(), &context));
  EXPECT_EQ(RemoveTrailingNul(context), "unconfined_r:unconfined_t:s0");
}

TEST(ProcAttrTest, PreadPastEof) {
  auto current_attr = fbl::unique_fd(open("/proc/self/attr/current", O_RDONLY));
  ASSERT_TRUE(current_attr.is_valid()) << strerror(errno);

  char buf[256] = {};
  ssize_t initial = pread(current_attr.get(), buf, sizeof(buf), 0);
  ASSERT_THAT(initial, SyscallSucceeds());
  ASSERT_GT(initial, 0);
  ASSERT_LT(static_cast<size_t>(initial), sizeof(buf));

  EXPECT_THAT(pread(current_attr.get(), buf, sizeof(buf), initial), SyscallSucceedsWithValue(0));
  EXPECT_THAT(pread(current_attr.get(), buf, sizeof(buf), initial + 1),
              SyscallSucceedsWithValue(0));
  EXPECT_THAT(pread(current_attr.get(), buf, sizeof(buf), 4096), SyscallSucceedsWithValue(0));
  EXPECT_THAT(pread(current_attr.get(), buf, sizeof(buf), 1 << 20), SyscallSucceedsWithValue(0));
}

// Writable attributes validate the contexts written to them.
TEST(ProcAttrTest, WritableAttrsValidateContexts) {
  // Write a valid context and verify that it was set.
  EXPECT_THAT(WriteTaskAttr("exec", "system_u:unconfined_r:unconfined_t:s0"), SyscallResultIsOk());
  EXPECT_THAT(ReadTaskAttr("exec"), SyscallResultIsOk("system_u:unconfined_r:unconfined_t:s0"));

  // Write an invalid context and verify that nothing is changed.
  EXPECT_THAT(WriteTaskAttr("exec", "system_u:invalid_role_r:unconfined_t:s0"),
              SyscallResultIsErrno(EINVAL));
  EXPECT_THAT(ReadTaskAttr("exec"), SyscallResultIsOk("system_u:unconfined_r:unconfined_t:s0"));
}

// Writing a single NUL clears the attribute.
TEST(ProcAttrTest, WritableAttrsClearedByNul) {
  // Set a valid context, then clear it with NUL.
  ASSERT_THAT(WriteTaskAttr("exec", "system_u:unconfined_r:unconfined_t:s0"), SyscallResultIsOk());
  ASSERT_THAT(ReadTaskAttr("exec"), SyscallResultIsOk("system_u:unconfined_r:unconfined_t:s0"));
  EXPECT_THAT(WriteTaskAttr("exec", std::string_view("\0", 1)), SyscallResultIsOk());
  EXPECT_THAT(ReadTaskAttr("exec"), SyscallResultIsOk(""));
}

// Writing a single newline clears the attribute.
TEST(ProcAttrTest, WritableAttrsClearedByNewline) {
  // Set a valid context, then clear it with newline.
  ASSERT_THAT(WriteTaskAttr("exec", "system_u:unconfined_r:unconfined_t:s0"), SyscallResultIsOk());
  ASSERT_THAT(ReadTaskAttr("exec"), SyscallResultIsOk("system_u:unconfined_r:unconfined_t:s0"));
  EXPECT_THAT(WriteTaskAttr("exec", "\n"), SyscallResultIsOk());
  EXPECT_THAT(ReadTaskAttr("exec"), SyscallResultIsOk(""));
}

// Writing a valid context across multiple writes is not valid.
TEST(ProcAttrTest, ContextsMustBeWrittenInASingleWrite) {
  auto exec_attr = fbl::unique_fd(open("/proc/self/attr/exec", O_RDWR));
  ASSERT_TRUE(exec_attr.is_valid()) << strerror(errno);

  constexpr char kFirstPart[] = "system_u:unconfined_r";
  constexpr char kSecondPart[] = ":unconfined_t:s0";

  auto result = write(exec_attr.get(), kFirstPart, sizeof(kFirstPart));
  EXPECT_EQ(result, -1);
  EXPECT_EQ(errno, EINVAL);

  result = write(exec_attr.get(), kSecondPart, sizeof(kSecondPart));
  EXPECT_EQ(result, -1);
  EXPECT_EQ(errno, EINVAL);
}

// Writes are accepted, but only if the seek position is zero.
TEST(ProcAttrTest, WritableAttrsCanOnlyBeWrittenAtSeekPositionZero) {
  auto exec_attr = fbl::unique_fd(open("/proc/self/attr/exec", O_RDWR));
  ASSERT_TRUE(exec_attr.is_valid()) << strerror(errno);

  // Seek to a non-zero position.
  ASSERT_EQ(lseek(exec_attr.get(), 1, SEEK_SET), 1) << strerror(errno);

  // Write a valid context into the attribute.
  constexpr char kValidContext[] = "system_u:unconfined_r:unconfined_t:s0";
  auto result = write(exec_attr.get(), kValidContext, sizeof(kValidContext));
  EXPECT_EQ(result, -1);
  EXPECT_EQ(errno, EINVAL) << strerror(errno);
}

// Writes do not affect the seek position.
TEST(ProcAttrTest, WritesDoNotAffectSeekPosition) {
  auto exec_attr = fbl::unique_fd(open("/proc/self/attr/exec", O_RDWR));
  ASSERT_TRUE(exec_attr.is_valid()) << strerror(errno);

  // Write a valid context into the attribute.
  constexpr char kValidContext[] = "system_u:unconfined_r:unconfined_t:s0";
  auto result = write(exec_attr.get(), kValidContext, sizeof(kValidContext));
  ASSERT_EQ(result, (ssize_t)sizeof(kValidContext)) << strerror(errno);

  // Seek to a non-zero position.
  EXPECT_EQ(lseek(exec_attr.get(), 0, SEEK_CUR), 0) << strerror(errno);
}

// Multiple writes are accepted, so long as the contents are valid.
TEST(ProcAttrTest, WritableAttrsCanBeWrittenMoreThanOnce) {
  auto exec_attr = fbl::unique_fd(open("/proc/self/attr/exec", O_RDWR));
  ASSERT_TRUE(exec_attr.is_valid()) << strerror(errno);

  // Write a valid context into the attribute.
  constexpr char kValidContext[] = "system_u:unconfined_r:unconfined_t:s0";
  auto result = write(exec_attr.get(), kValidContext, sizeof(kValidContext));
  ASSERT_EQ(result, (ssize_t)sizeof(kValidContext)) << strerror(errno);

  // Read back the context.
  ASSERT_EQ(lseek(exec_attr.get(), 0, SEEK_SET), 0) << strerror(errno);
  std::string context;
  ASSERT_TRUE(files::ReadFileDescriptorToString(exec_attr.get(), &context));
  EXPECT_EQ(RemoveTrailingNul(context), kValidContext);

  // Seek back to position zero, so writes will be accepted, and write a NUL to
  // clear the attribute.
  ASSERT_EQ(lseek(exec_attr.get(), 0, SEEK_SET), 0) << strerror(errno);
  constexpr char kSingleNul[] = "\0";
  result = write(exec_attr.get(), kSingleNul, sizeof(kSingleNul));
  ASSERT_EQ(result, (ssize_t)sizeof(kSingleNul)) << strerror(errno);

  // Read back the context, which will now be empty.
  ASSERT_TRUE(files::ReadFileDescriptorToString(exec_attr.get(), &context));
  EXPECT_EQ(context, std::string());
}

void WaitForReadFd(int fd) {
  fd_set read_fds;
  FD_ZERO(&read_fds);
  FD_SET(fd, &read_fds);

  SAFE_SYSCALL(TEMP_FAILURE_RETRY(select(fd + 1, &read_fds, nullptr, nullptr, nullptr)));
  EXPECT_TRUE(FD_ISSET(fd, &read_fds));
}

TEST(ProcAttrTest, ZombieCurrentAttr) {
  ASSERT_TRUE(RunSubprocessAs("unconfined_u:unconfined_r:unconfined_t:s0:c0", []() {
    pid_t child_pid = SAFE_SYSCALL(fork());
    if (child_pid == 0) {
      _exit(EXIT_SUCCESS);
    }

    auto pidfd = fbl::unique_fd(test_helper::PidFdOpen(child_pid, 0));
    WaitForReadFd(pidfd.get());

    // Validate the label reported in the child's "/proc/<pid>/attr/current".
    auto current_attr_path = fxl::StringPrintf("/proc/%d/attr/current", child_pid);
    ASSERT_EQ(access(current_attr_path.data(), F_OK), 0)
        << "access() to " << current_attr_path << " failed:" << strerror(errno);
    auto current_attr = fbl::unique_fd(open(current_attr_path.data(), O_RDONLY));
    ASSERT_TRUE(current_attr.is_valid()) << strerror(errno);
    std::string context;
    ASSERT_TRUE(files::ReadFileDescriptorToString(current_attr.get(), &context));

    auto my_label = ReadTaskAttr("current");
    ASSERT_TRUE(my_label.is_ok()) << my_label.error_value();

    EXPECT_EQ(RemoveTrailingNul(context), my_label.value());
  }));
}

TEST(ProcFsTest, RestrictedProcFilesHaveOwnerOnlyReadMode) {
  constexpr const char* kRequiredRestrictedProcFiles[] = {
      "/proc/pagetypeinfo",
      "/proc/vmallocinfo",
  };
  for (const char* path : kRequiredRestrictedProcFiles) {
    struct stat st{};
    ASSERT_THAT(stat(path, &st), SyscallSucceeds()) << path;
    EXPECT_EQ(st.st_mode & 0777, 0400u) << path;
  }

  constexpr const char* kOptionalRestrictedProcFiles[] = {
      "/proc/allocinfo",
      "/proc/slabinfo",
  };
  for (const char* path : kOptionalRestrictedProcFiles) {
    struct stat st{};
    if (stat(path, &st) == 0) {
      EXPECT_EQ(st.st_mode & 0777, 0400u) << path;
    }
  }

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    constexpr uid_t kUnprivilegedUid = 1000;
    constexpr gid_t kUnprivilegedGid = 1000;
    ASSERT_THAT(setresgid(kUnprivilegedGid, kUnprivilegedGid, kUnprivilegedGid), SyscallSucceeds());
    ASSERT_THAT(setresuid(kUnprivilegedUid, kUnprivilegedUid, kUnprivilegedUid), SyscallSucceeds());
    for (const char* path : kRequiredRestrictedProcFiles) {
      EXPECT_THAT(open(path, O_RDONLY), SyscallFailsWithErrno(EACCES)) << path;
    }
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

constexpr char kInitialContext[] = "test_u:test_r:test_procfs_initial_t:s0";
constexpr char kDynTransitionContext[] = "test_u:test_r:test_procfs_dyntransition_t:s0";
constexpr char kExecContext[] = "test_u:test_r:test_procfs_exec_t:s0";
constexpr char kUnboundedContext[] = "test_u:test_r:test_procfs_unbounded_t:s0";
constexpr char kBoundedContext[] = "test_u:test_r:test_procfs_bounded_t:s0";

fit::result<int, std::string> GetLinkLabelAt(const std::string& path) {
  char buf[256];
  ssize_t result = lgetxattr(path.c_str(), "security.selinux", buf, sizeof(buf));
  if (result < 0) {
    return fit::error(errno);
  }
  return RemoveTrailingNul(std::string(buf, result));
}

// Holds pre-opened file descriptors to representative nodes in a `/proc/<pid>` or
// `/proc/<pid>/task/<tid>` subtree so tests can verify path-based (`getxattr`/`lgetxattr`) and
// open-FD (`fgetxattr`) labels across task transitions and exit states.
struct OpenedTaskSubtree {
  std::string base_dir;
  fbl::unique_fd dir_fd;
  fbl::unique_fd status_fd;
  fbl::unique_fd attr_dir_fd;
  fbl::unique_fd attr_current_fd;

  static OpenedTaskSubtree Open(std::string base_dir) {
    OpenedTaskSubtree subtree{.base_dir = std::move(base_dir)};
    subtree.dir_fd.reset(open(subtree.base_dir.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC));
    subtree.status_fd.reset(open((subtree.base_dir + "/status").c_str(), O_RDONLY | O_CLOEXEC));
    subtree.attr_dir_fd.reset(
        open((subtree.base_dir + "/attr").c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC));
    subtree.attr_current_fd.reset(
        open((subtree.base_dir + "/attr/current").c_str(), O_RDONLY | O_CLOEXEC));
    return subtree;
  }

  void AssertValid() const {
    ASSERT_TRUE(dir_fd.is_valid()) << base_dir << ": " << strerror(errno);
    ASSERT_TRUE(status_fd.is_valid()) << base_dir << "/status: " << strerror(errno);
    ASSERT_TRUE(attr_dir_fd.is_valid()) << base_dir << "/attr: " << strerror(errno);
    ASSERT_TRUE(attr_current_fd.is_valid()) << base_dir << "/attr/current: " << strerror(errno);
  }

  void ExpectPathAndFdLabels(std::string_view expected, bool include_fd_link = true) const {
    // Query path labels before open-FD labels so VFS path resolution revalidates cached directory
    // entries before `fgetxattr` or `pread` inspects the shared node.
    EXPECT_THAT(GetLabel(base_dir), SyscallResultIsOk(expected));
    EXPECT_THAT(GetLabel(base_dir + "/status"), SyscallResultIsOk(expected));
    EXPECT_THAT(GetLabel(base_dir + "/attr"), SyscallResultIsOk(expected));
    EXPECT_THAT(GetLabel(base_dir + "/attr/current"), SyscallResultIsOk(expected));
    if (include_fd_link) {
      EXPECT_THAT(GetLinkLabelAt(base_dir + "/fd/0"), SyscallResultIsOk(expected));
    }
    ExpectOpenFdLabels(expected);
  }

  void ExpectOpenFdLabels(std::string_view expected) const {
    EXPECT_THAT(GetLabel(dir_fd.get()), SyscallResultIsOk(expected));
    EXPECT_THAT(GetLabel(status_fd.get()), SyscallResultIsOk(expected));
    EXPECT_THAT(GetLabel(attr_dir_fd.get()), SyscallResultIsOk(expected));
    EXPECT_THAT(GetLabel(attr_current_fd.get()), SyscallResultIsOk(expected));
  }
};

struct ProcTaskTransitionParam {
  const char* name;
  const char* target_context;
  bool via_exec;
};

class ProcTaskTransitionTest : public ::testing::TestWithParam<ProcTaskTransitionParam> {};

// Verifies that `/proc/<pid>` and `/proc/<pid>/task/<pid>` nodes update their labels across both
// dynamic (`setcurrent`) and `execve` transitions, re-evaluate permissions on already-open FDs
// against the updated label, and retain that label through zombie and reaped task states.
TEST_P(ProcTaskTransitionTest, UpdatesAndRetainsLabelsAcrossLifecycle) {
  const ProcTaskTransitionParam& param = GetParam();
  auto enforce = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs(kInitialContext, [&] {
    test_helper::ScopedPipe sync_pipe(O_CLOEXEC);

    pid_t child_pid = -1;
    ASSERT_THAT((child_pid = fork()), SyscallSucceeds());
    if (child_pid == 0) {
      sync_pipe.WriteSide().reset();
      char buf = 0;
      if (TEMP_FAILURE_RETRY(read(sync_pipe.ReadSide().get(), &buf, sizeof(buf))) != 0) {
        _exit(1);
      }
      sync_pipe.ReadSide().reset();

      if (param.via_exec) {
        if (WriteTaskAttr("exec", param.target_context).is_error()) {
          _exit(1);
        }
        std::string binary_name = "stop_bin";
        std::string path_for_exec = "data/bin/" + binary_name;
        char* const args[] = {binary_name.data(), nullptr};
        execv(path_for_exec.c_str(), args);
        _exit(1);
      }
      if (WriteTaskAttr("current", param.target_context).is_error()) {
        _exit(1);
      }
      raise(SIGSTOP);
      _exit(0);
    }

    sync_pipe.ReadSide().reset();
    auto cleanup_child = fit::defer([&] {
      if (child_pid > 0) {
        kill(child_pid, SIGKILL);
        int wstatus = 0;
        TEMP_FAILURE_RETRY(waitpid(child_pid, &wstatus, 0));
      }
    });

    auto pid_nodes = OpenedTaskSubtree::Open(fxl::StringPrintf("/proc/%d", child_pid));
    ASSERT_NO_FATAL_FAILURE(pid_nodes.AssertValid());
    auto tid_nodes =
        OpenedTaskSubtree::Open(fxl::StringPrintf("/proc/%d/task/%d", child_pid, child_pid));
    ASSERT_NO_FATAL_FAILURE(tid_nodes.AssertValid());

    pid_nodes.ExpectPathAndFdLabels(kInitialContext);
    tid_nodes.ExpectPathAndFdLabels(kInitialContext);
    char buf = 0;
    EXPECT_THAT(pread(pid_nodes.status_fd.get(), &buf, sizeof(buf), 0),
                SyscallSucceedsWithValue(1));

    // Release the child to transition (`setcurrent` + `SIGSTOP`, or `execve` into `stop_bin`).
    sync_pipe.WriteSide().reset();
    int wstatus = 0;
    ASSERT_THAT(TEMP_FAILURE_RETRY(waitpid(child_pid, &wstatus, WUNTRACED)),
                SyscallSucceedsWithValue(child_pid));
    ASSERT_TRUE(WIFSTOPPED(wstatus));

    pid_nodes.ExpectPathAndFdLabels(param.target_context);
    tid_nodes.ExpectPathAndFdLabels(param.target_context);
    EXPECT_THAT(pread(pid_nodes.status_fd.get(), &buf, sizeof(buf), 0),
                SyscallFailsWithErrno(EACCES));

    // Resume the child so it exits and becomes an unreaped zombie.
    ASSERT_THAT(kill(child_pid, SIGCONT), SyscallSucceeds());
    siginfo_t siginfo{};
    ASSERT_THAT(TEMP_FAILURE_RETRY(waitid(P_PID, child_pid, &siginfo, WEXITED | WNOWAIT)),
                SyscallSucceeds());
    EXPECT_THAT(access(pid_nodes.base_dir.c_str(), F_OK), SyscallSucceeds());
    pid_nodes.ExpectPathAndFdLabels(param.target_context, /* include_fd_link = */ false);
    if (!test_helper::IsStarnix()) {
      tid_nodes.ExpectPathAndFdLabels(param.target_context, /* include_fd_link = */ false);
    } else {
      // TODO: https://fxbug.dev/584223615 - Verify path labels on `tid_nodes` once Starnix keeps
      // `/proc/<pid>/task/<pid>` accessible while `<pid>` is an unreaped zombie.
      tid_nodes.ExpectOpenFdLabels(param.target_context);
    }

    // Reap the child: `/proc/<pid>` disappears, while open FDs retain the task's label.
    ASSERT_THAT(TEMP_FAILURE_RETRY(waitpid(child_pid, &wstatus, 0)),
                SyscallSucceedsWithValue(child_pid));
    cleanup_child.cancel();
    ASSERT_TRUE(WIFEXITED(wstatus));
    EXPECT_EQ(WEXITSTATUS(wstatus), 0);

    EXPECT_THAT(access(pid_nodes.base_dir.c_str(), F_OK), SyscallFailsWithErrno(ENOENT));
    EXPECT_THAT(access(tid_nodes.base_dir.c_str(), F_OK), SyscallFailsWithErrno(ENOENT));
    pid_nodes.ExpectOpenFdLabels(param.target_context);
    tid_nodes.ExpectOpenFdLabels(param.target_context);
  }));
}

INSTANTIATE_TEST_SUITE_P(ProcFs, ProcTaskTransitionTest,
                         ::testing::Values(
                             ProcTaskTransitionParam{
                                 .name = "Dynamic",
                                 .target_context = kDynTransitionContext,
                                 .via_exec = false,
                             },
                             ProcTaskTransitionParam{
                                 .name = "Exec",
                                 .target_context = kExecContext,
                                 .via_exec = true,
                             }),
                         [](const ::testing::TestParamInfo<ProcTaskTransitionParam>& info) {
                           return info.param.name;
                         });

// Verifies that when a non-leader thread transitions to a bounded domain, `/proc/<pid>/task/<tid>`
// and its child nodes (both already-instantiated and newly looked up) reflect `<tid>`'s label and
// enforce permissions against it, while `/proc/<pid>` and `/proc/<pid>/task/<pid>` continue to
// reflect the thread-group leader's label.
TEST(ProcFsTest, ProcTaskTidNodesReflectBoundedThreadLabel) {
  auto enforce = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs(kUnboundedContext, [&] {
    test_helper::ScopedPipe to_thread(O_CLOEXEC);
    test_helper::ScopedPipe from_thread(O_CLOEXEC);

    std::thread worker([to_read = std::move(to_thread.ReadSide()),
                        from_write = std::move(from_thread.WriteSide())]() {
      pid_t tid = static_cast<pid_t>(syscall(SYS_gettid));
      if (TEMP_FAILURE_RETRY(write(from_write.get(), &tid, sizeof(tid))) != sizeof(tid)) {
        return;
      }

      char cmd = 0;
      if (TEMP_FAILURE_RETRY(read(to_read.get(), &cmd, sizeof(cmd))) != sizeof(cmd)) {
        return;
      }

      auto res = WriteExistingFile("/proc/thread-self/attr/current", kBoundedContext);
      int err = res.is_error() ? res.error_value() : 0;
      if (TEMP_FAILURE_RETRY(write(from_write.get(), &err, sizeof(err))) != sizeof(err)) {
        return;
      }

      // Wait until the main thread finishes verifying labels and closes `to_thread`.
      TEMP_FAILURE_RETRY(read(to_read.get(), &cmd, sizeof(cmd)));
    });

    auto cleanup_worker = fit::defer([&] {
      to_thread.WriteSide().reset();
      if (worker.joinable()) {
        worker.join();
      }
    });

    pid_t tid = -1;
    ASSERT_THAT(TEMP_FAILURE_RETRY(read(from_thread.ReadSide().get(), &tid, sizeof(tid))),
                SyscallSucceedsWithValue(sizeof(tid)));
    pid_t pid = getpid();
    ASSERT_NE(tid, pid);

    auto pid_nodes = OpenedTaskSubtree::Open(fxl::StringPrintf("/proc/%d", pid));
    ASSERT_NO_FATAL_FAILURE(pid_nodes.AssertValid());
    auto leader_tid_nodes =
        OpenedTaskSubtree::Open(fxl::StringPrintf("/proc/%d/task/%d", pid, pid));
    ASSERT_NO_FATAL_FAILURE(leader_tid_nodes.AssertValid());
    auto worker_tid_nodes =
        OpenedTaskSubtree::Open(fxl::StringPrintf("/proc/%d/task/%d", pid, tid));
    ASSERT_NO_FATAL_FAILURE(worker_tid_nodes.AssertValid());

    pid_nodes.ExpectPathAndFdLabels(kUnboundedContext);
    leader_tid_nodes.ExpectPathAndFdLabels(kUnboundedContext);
    worker_tid_nodes.ExpectPathAndFdLabels(kUnboundedContext);
    char buf = 0;
    EXPECT_THAT(pread(pid_nodes.status_fd.get(), &buf, sizeof(buf), 0),
                SyscallSucceedsWithValue(1));
    EXPECT_THAT(pread(worker_tid_nodes.status_fd.get(), &buf, sizeof(buf), 0),
                SyscallSucceedsWithValue(1));

    // Signal the non-leader thread to transition to `kBoundedContext`.
    char go = 1;
    ASSERT_THAT(TEMP_FAILURE_RETRY(write(to_thread.WriteSide().get(), &go, sizeof(go))),
                SyscallSucceedsWithValue(sizeof(go)));
    int transition_err = -1;
    ASSERT_THAT(TEMP_FAILURE_RETRY(
                    read(from_thread.ReadSide().get(), &transition_err, sizeof(transition_err))),
                SyscallSucceedsWithValue(sizeof(transition_err)));
    ASSERT_EQ(transition_err, 0) << strerror(transition_err);

    // Process-level and leader-task subtrees stay in `kUnboundedContext`, while the worker thread's
    // subtree (including newly instantiated child entries) reflects `kBoundedContext`.
    pid_nodes.ExpectPathAndFdLabels(kUnboundedContext);
    leader_tid_nodes.ExpectPathAndFdLabels(kUnboundedContext);
    EXPECT_THAT(pread(pid_nodes.status_fd.get(), &buf, sizeof(buf), 0),
                SyscallSucceedsWithValue(1));

    worker_tid_nodes.ExpectPathAndFdLabels(kBoundedContext);
    EXPECT_THAT(GetLabel(worker_tid_nodes.base_dir + "/comm"), SyscallResultIsOk(kBoundedContext));
    EXPECT_THAT(pread(worker_tid_nodes.status_fd.get(), &buf, sizeof(buf), 0),
                SyscallFailsWithErrno(EACCES));
  }));
}

}  // namespace
