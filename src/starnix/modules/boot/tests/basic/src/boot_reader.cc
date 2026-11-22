// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main() {
  int fd = open("/dev/booted", O_RDWR);
  if (fd < 0) {
    abort();
  }
  char buf[1];
  memset(buf, 0xff, sizeof(buf));

  ssize_t count = read(fd, buf, 1);
  if (count < 0) {
    abort();
  }
  if (count != 1) {
    abort();
  }
  if (buf[0] != 0) {
    abort();
  }

  constexpr char kSystemBoot[] =
      "system_boot; Android sys.boot_completed=1 (Device Encrypted storage unlocked)";
  constexpr ssize_t kSystemBootLen = sizeof(kSystemBoot) - 1;
  count = write(fd, kSystemBoot, kSystemBootLen);
  if (count < 0) {
    abort();
  }
  if (count != kSystemBootLen) {
    abort();
  }

  // Duplicate tags must be rejected with EEXIST.
  errno = 0;
  count = write(fd, kSystemBoot, kSystemBootLen);
  if (count != -1 || errno != EEXIST) {
    abort();
  }

  constexpr char kCeUnlocked[] =
      "ce_unlocked; Android sys.user.0.ce_available=true (Credential Encrypted storage unlocked)";
  constexpr ssize_t kCeUnlockedLen = sizeof(kCeUnlocked) - 1;
  count = write(fd, kCeUnlocked, kCeUnlockedLen);
  if (count != kCeUnlockedLen) {
    abort();
  }

  count = read(fd, buf, 1);
  if (count < 0) {
    abort();
  }
  if (count != 1) {
    abort();
  }
  if (buf[0] != 1) {
    abort();
  }
}
