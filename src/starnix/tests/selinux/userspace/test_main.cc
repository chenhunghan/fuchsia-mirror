// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/cmdline/args_parser.h>
#include <lib/fit/defer.h>
#include <lib/fit/result.h>
#include <lib/stdcompat/string_view.h>
#include <sys/mount.h>

#include <cstring>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/starnix/tests/selinux/userspace/audit_checker.h"
#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

namespace {

struct CommandLineOptions {
  bool generate_json = false;
  bool skip_audit = false;
};

std::string g_initial_task_security_context;

// Perform one-time initialization of the test system.
void PrepareTestEnvironment() {
  // Check if selinuxfs is already mounted as a proxy for environment setup.
  auto mounts = ReadFile("/proc/mounts");
  if (mounts.is_ok() && cpp23::contains(mounts.value(), "/sys/fs/selinux selinuxfs")) {
    // Environment appears to be already initialized (e.g. a subsequent test chunk in the same
    // container). Suites that define `DoPrePolicyLoadWork()` require a fresh pre-policy
    // environment and must not be split across multiple invocations.
    if (DoPrePolicyLoadWork) {
      FAIL() << "DoPrePolicyLoadWork() cannot run because the test environment was already "
                "initialized by a prior invocation.";
    }
    return;
  }
  ASSERT_THAT(mkdir("/proc", 0755), SyscallSucceeds());
  ASSERT_THAT(mkdir("/sys", 0755), SyscallSucceeds());
  ASSERT_THAT(mkdir("/tmp", 0755), SyscallSucceeds());
  // `/dev` already exists on Linux.
  if (test_helper::IsStarnix()) {
    ASSERT_THAT(mkdir("/dev", 0755), SyscallSucceeds());
  }
  ASSERT_THAT(mount("proc", "/proc", "proc", MS_NOEXEC | MS_NOSUID | MS_NODEV, 0),
              SyscallSucceeds());
  ASSERT_THAT(mount("sysfs", "/sys", "sysfs", MS_NOEXEC | MS_NOSUID | MS_NODEV, 0),
              SyscallSucceeds());
  ASSERT_THAT(mount("selinuxfs", "/sys/fs/selinux", "selinuxfs", MS_NOEXEC | MS_NOSUID, nullptr),
              SyscallSucceeds());
  ASSERT_THAT(mount("bpf", "/sys/fs/bpf", "bpf", MS_NOEXEC | MS_NOSUID, nullptr),
              SyscallSucceeds());
  ASSERT_THAT(mount("tmpfs", "/tmp", "tmpfs", MS_RELATIME, nullptr), SyscallSucceeds());
  ASSERT_THAT(mount("tmpfs", "/dev", "tmpfs", MS_NOEXEC | MS_NOSUID | MS_STRICTATIME, nullptr),
              SyscallSucceeds());

  if (DoPrePolicyLoadWork) {
    DoPrePolicyLoadWork();
  }

  // Ensure that no previous policy has been loaded.
  auto previous_policy = ReadFile("/sys/fs/selinux/policy");
  ASSERT_THAT(previous_policy, SyscallResultIsErrno(EINVAL));

  LoadPolicy(GetTestPolicyName());
}

class TaskSecurityContextChecker : public ::testing::EmptyTestEventListener {
 public:
  void OnTestStart(const testing::TestInfo& test_info) override {
    auto current_policy = ReadFile("/sys/fs/selinux/policy");
    if (current_policy.is_error()) {
      ADD_FAILURE() << "Failed to read loaded policy at start of test "
                    << test_info.test_suite_name() << "." << test_info.name() << ": "
                    << strerror(current_policy.error_value());
      test_start_policy_.clear();
      return;
    }
    test_start_policy_ = std::move(current_policy.value());
  }

  void OnTestEnd(const testing::TestInfo& test_info) override {
    CheckPolicy(test_info, "end");
    CheckSecurityContext(test_info, "end");
  }

 private:
  void CheckPolicy(const testing::TestInfo& test_info, const char* phase) {
    if (test_start_policy_.empty()) {
      return;
    }
    auto current_policy = ReadFile("/sys/fs/selinux/policy");
    if (current_policy.is_error()) {
      ADD_FAILURE() << "Failed to read loaded policy at " << phase << " of test "
                    << test_info.test_suite_name() << "." << test_info.name() << ": "
                    << strerror(current_policy.error_value());
      return;
    }
    if (current_policy.value() != test_start_policy_) {
      ADD_FAILURE() << "Loaded policy was modified at " << phase << " of test "
                    << test_info.test_suite_name() << "." << test_info.name()
                    << " and not restored.";
      auto reset_result = WriteExistingFile("/sys/fs/selinux/load", test_start_policy_);
      if (reset_result.is_error()) {
        ADD_FAILURE() << "Failed to restore policy after test " << test_info.test_suite_name()
                      << "." << test_info.name() << ": " << strerror(reset_result.error_value());
      }
    }
  }
  void CheckSecurityContext(const testing::TestInfo& test_info, const char* phase) {
    auto current_context = ReadTaskAttr("current");
    if (current_context.is_error()) {
      ADD_FAILURE() << "Failed to read current security context at " << phase << " of test "
                    << test_info.test_suite_name() << "." << test_info.name() << ": "
                    << strerror(current_context.error_value());
      return;
    }
    if (current_context.value() != g_initial_task_security_context) {
      ADD_FAILURE() << "Security context mismatch at " << phase << " of test "
                    << test_info.test_suite_name() << "." << test_info.name() << ". Expected '"
                    << g_initial_task_security_context << "', got '" << current_context.value()
                    << "'";
      // Reset back to initial context to avoid poisoning subsequent tests.
      auto reset_result = WriteTaskAttr("current", g_initial_task_security_context);
      if (reset_result.is_error()) {
        ADD_FAILURE() << "Failed to reset security context back to '"
                      << g_initial_task_security_context
                      << "': " << strerror(reset_result.error_value());
      }
    }
  }

  std::string test_start_policy_;
};

class UserspaceTestEnvironment : public ::testing::Environment {
 public:
  void SetUp() override {
    PrepareTestEnvironment();

    auto initial_context = ReadTaskAttr("current");
    if (initial_context.is_error()) {
      fprintf(stderr, "Failed to read initial security context: %s\n",
              strerror(initial_context.error_value()));
      _exit(1);
    }
    g_initial_task_security_context = initial_context.value();

    // gTest is documented as treating `Environment::SetUp` fatal failures as fatal, but does not
    // appear to actually do so, so we manually terminate the attempt on setup failures.
    if (::testing::Test::HasFailure()) {
      fprintf(stderr, "Test environment setup failed => failing all tests.\n");
      fflush(stdout);
      fflush(stderr);
      _exit(1);
    }
  }
};

}  // namespace

// Parse the arguments into valid options structure.
fit::result<std::string, CommandLineOptions> parse_args(int argc, char** argv) {
  cmdline::ArgsParser<CommandLineOptions> parser;
  CommandLineOptions options;
  parser.AddSwitch("json", 'j', "--json\tGenerate audit log JSON objects for expectations.",
                   &CommandLineOptions::generate_json);
  parser.AddSwitch("skip-audit", 0, "--skip-audit\tSkip audit log checking.",
                   &CommandLineOptions::skip_audit);
  std::vector<std::string> params;
  if (auto status = parser.Parse(argc, const_cast<const char**>(argv), &options, &params);
      status.has_error()) {
    return fit::error("Error: " + status.error_message());
  }
  if (params.size()) {
    return fit::error("Error: arguments with parameters found.");
  }
  return fit::ok(options);
}

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);

  auto parse_res = parse_args(argc, argv);
  if (parse_res.is_error()) {
    fprintf(stderr, "%s\n", parse_res.error_value().c_str());
    return 1;
  }

  // Set up gTest to perform test environment setup at-most-once.
  GTEST_FLAG_SET(recreate_environments_when_repeating, false);
  ::testing::AddGlobalTestEnvironment(new UserspaceTestEnvironment);

  testing::TestEventListeners& listeners = testing::UnitTest::GetInstance()->listeners();
  listeners.Append(new TaskSecurityContextChecker);
  // The `with_json_generation()` function can be used to get an `AuditChecker` which will
  // generate audit log JSON objects.
  if (parse_res.value().generate_json) {
    listeners.Append(AuditChecker::with_json_generation());
  } else if (!parse_res.value().skip_audit) {
    listeners.Append(new AuditChecker);
  }

  return RUN_ALL_TESTS();
}
