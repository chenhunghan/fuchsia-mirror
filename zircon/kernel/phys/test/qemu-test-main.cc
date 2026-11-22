// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/boot-options/boot-options.h>
#include <lib/uart/qemu.h>
#include <stdio.h>

#include <ktl/utility.h>
#include <phys/boot-options.h>
#include <phys/output/stdout.h>
#include <phys/output/uart.h>
#include <phys/runtime/main.h>

#include "test-main.h"

#include <ktl/enforce.h>

void CppPhysMain(void* bootloader_data, arch::EarlyTicks ticks) {
  ArchSetUpCpu();

  UartDriver& uart = SetUartDriver(uart::qemu::kConfig);
  gPhysStdout.set_console(FILE{&uart});

  static BootOptions boot_opts;
  boot_opts.serial = uart::qemu::kConfig;
  InstallBootOptions(&boot_opts);

  // Early boot may have filled the screen with logs. Add a newline to
  // terminate any previous line, and another newline to leave a blank.
  printf("\n\n");

  // Run the test.
  int status = TestMain(bootloader_data, {}, ticks);
  if (status == 0) {
    printf("\n*** Test succeeded ***\n%s\n\n", BOOT_TEST_SUCCESS_STRING);
  } else {
    printf("\n*** Test FAILED: status %d ***\n\n", status);
  }

  ArchPanicReset();
}
