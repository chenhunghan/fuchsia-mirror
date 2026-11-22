// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fdio/limits.h>
#include <lib/fdio/spawn.h>
#include <lib/zx/channel.h>
#include <lib/zx/process.h>
#include <lib/zx/socket.h>
#include <unistd.h>
#include <zircon/processargs.h>

#include <iterator>
#include <string>

#include <test-utils/test-utils.h>
#include <zxtest/zxtest.h>

namespace {

void RunCwdChild(const char* bin, const char* const* envp, const char* expected_cwd) {
  zx::socket stdout_parent, stdout_child;
  ASSERT_OK(zx::socket::create(ZX_SOCKET_STREAM, &stdout_parent, &stdout_child));

  const fdio_spawn_action_t actions[] = {
      {
          .action = FDIO_SPAWN_ACTION_ADD_HANDLE,
          .h =
              {
                  .id = PA_HND(PA_FD, STDOUT_FILENO),
                  .handle = stdout_child.release(),
              },
      },
  };
  const char* const argv[] = {bin, expected_cwd, nullptr};
  zx::process process;
  char err_msg[FDIO_SPAWN_ERR_MSG_MAX_LENGTH];
  ASSERT_OK(fdio_spawn_etc(
                ZX_HANDLE_INVALID,
                FDIO_SPAWN_DEFAULT_LDSVC | FDIO_SPAWN_CLONE_NAMESPACE | FDIO_SPAWN_CLONE_UTC_CLOCK,
                argv[0], argv, envp, std::size(actions), actions, process.reset_and_get_address(),
                err_msg),
            "%s", err_msg);

  ASSERT_OK(process.wait_one(ZX_PROCESS_TERMINATED, zx::time::infinite(), nullptr));

  std::string output;
  char buffer[1024];
  size_t actual = 0;
  while (stdout_parent.read(0, buffer, sizeof(buffer), &actual) == ZX_OK && actual > 0) {
    output.append(buffer, actual);
  }

  zx_info_process_t pinfo;
  ASSERT_OK(process.get_info(ZX_INFO_PROCESS, &pinfo, sizeof(pinfo), nullptr, nullptr));
  EXPECT_EQ(pinfo.return_code, 0, "%s", output.c_str());
}

// Ensure that the child crashes when given bogus fd handle info.
template <uint32_t... Info>
void BadFdTest() {
  const char* const argv[] = {"/pkg/bin/hello-world", nullptr};

  zx::channel bs_parent, bs_child;
  ASSERT_OK(zx::channel::create(0, &bs_parent, &bs_child));

  struct Message {
    zx_proc_args_t header;
    uint32_t handle_info[sizeof...(Info)];
  } msg{};
  zx_handle_t handles[sizeof...(Info)];

  msg.header.protocol = ZX_PROCARGS_PROTOCOL;
  msg.header.version = ZX_PROCARGS_VERSION;
  msg.header.handle_info_off = offsetof(Message, handle_info);

  zx_handle_t* h = handles;
  uint32_t* i = msg.handle_info;
  for (uint32_t info : {Info...}) {
    zx::channel c0, c1;
    ASSERT_OK(zx::channel::create(0, &c0, &c1));
    *h++ = c1.release();
    *i++ = info;
  }

  ASSERT_OK(bs_parent.write(0, &msg, sizeof(msg), handles, sizeof...(Info)));

  auto sb = tu_launch_init(ZX_HANDLE_INVALID, nullptr, 1, argv, 0, nullptr, 0, nullptr, nullptr);

  springboard_set_bootstrap(sb, bs_child.release());

  zx::process proc(tu_launch_fini(sb));
  zx_signals_t signals;
  ASSERT_OK(proc.wait_one(ZX_PROCESS_TERMINATED, zx::time::infinite(), &signals));
  ASSERT_TRUE(signals & ZX_PROCESS_TERMINATED);

  zx_info_process_t pinfo;
  ASSERT_OK(proc.get_info(ZX_INFO_PROCESS, &pinfo, sizeof(pinfo), nullptr, nullptr));

  EXPECT_NE(pinfo.return_code, 0);
}

TEST(Startup, InvalidFd) { BadFdTest<PA_HND(PA_FD, FDIO_MAX_FD)>(); }

TEST(Startup, DuplicateFd) { BadFdTest<PA_HND(PA_FD, 0), PA_HND(PA_FD, 0)>(); }

TEST(Startup, CwdUnsetPwd) {
  const char* const envp[] = {nullptr};
  RunCwdChild("/pkg/bin/cwd-child", envp, "/");
}

TEST(Startup, CwdAbsolutePwd) {
  const char* const envp[] = {"PWD=/pkg/bin/..", nullptr};
  RunCwdChild("/pkg/bin/cwd-child", envp, "/pkg");
}

TEST(Startup, CwdRelativePwd) {
  const char* const envp[] = {"PWD=pkg/./bin", nullptr};
  RunCwdChild("/pkg/bin/cwd-child", envp, "/pkg/bin");
}

TEST(Startup, CwdWithoutFdioStartup) {
  const char* const envp[] = {"PWD=/pkg", nullptr};
  RunCwdChild("/pkg/bin/no-startup-cwd-child", envp, "/");
}

}  // namespace
