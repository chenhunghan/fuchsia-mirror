// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <cstdlib>
#include <format>
#include <iostream>
#include <string>

#include "rpmb.h"
#include "ufsutil.h"

int main(int argc, char** argv) {
  ufsutil::Initialize();

  if (argc < 3) {
    ufsutil::PrintUsage(std::cout);
    return EXIT_FAILURE;
  }

  std::string path = argv[1];
  if (ufsutil::IsRpmbCommand(argv[2])) {
    zx::result rpmb = ufsutil::OpenRpmbDevice(path.c_str());
    if (rpmb.is_error()) {
      std::cerr << std::format("Failed to open device: {}\n", rpmb.status_string());
      return EXIT_FAILURE;
    }
    zx::result<> status =
        ufsutil::RunRpmb(std::move(rpmb.value()), argc, argv, std::cout, std::cerr);
    return status.is_ok() ? EXIT_SUCCESS : EXIT_FAILURE;
  }

  zx::result dev = ufsutil::OpenDevice(path.c_str());
  if (dev.is_error()) {
    std::cerr << std::format("Failed to open device: {}\n", dev.status_string());
    return EXIT_FAILURE;
  }

  zx::result<> status =
      ufsutil::RunUfsUtils(fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>(std::move(dev.value())),
                           argc, argv, std::cout, std::cerr);
  return status.is_ok() ? EXIT_SUCCESS : EXIT_FAILURE;
}
