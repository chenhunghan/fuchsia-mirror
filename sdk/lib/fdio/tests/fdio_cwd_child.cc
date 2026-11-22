// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Used by the tests in fdio_startup.cc. This checks that the working directory
// the process starts with is the one named by its only argument, returning
// distinct non-zero exit codes so failure modes remain distinguishable even
// when stdout is not installed.

#include <errno.h>
#include <limits.h>
#include <string.h>
#include <unistd.h>

#include <format>
#include <string>

namespace {

void WriteDiagnostic(const std::string& msg) {
  // Ignore EBADF when stdout is not installed (no-startup-cwd-child); unlike
  // std::println, raw write() does not abort on unbound fd 1.
  [[maybe_unused]] const ssize_t written = write(STDOUT_FILENO, msg.data(), msg.size());
}

}  // namespace

int main(int argc, char** argv) {
  if (argc != 2) {
    WriteDiagnostic(std::format("ERROR: unexpected argc={}\nUSAGE: {} EXPECTED_CWD\n", argc,
                                argc > 0 ? argv[0] : "cwd-child"));
    return 2;
  }

  char buf[PATH_MAX];
  const char* cwd = getcwd(buf, sizeof(buf));
  if (cwd == nullptr) {
    const int err = errno;
    WriteDiagnostic(std::format("getcwd returned nullptr, errno={}({})\n", err, strerror(err)));
    return 3;
  }
  if (strcmp(cwd, argv[1]) != 0) {
    WriteDiagnostic(std::format("cwd={}, expected {}\n", cwd, argv[1]));
    return 4;
  }

  return 0;
}
