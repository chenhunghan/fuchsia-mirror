// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_BIN_UFSUTIL_RPMB_H_
#define SRC_DEVICES_BLOCK_BIN_UFSUTIL_RPMB_H_

#include <fidl/fuchsia.hardware.rpmb/cpp/wire.h>
#include <lib/zx/result.h>

#include <ostream>
#include <string_view>

namespace ufsutil {

// Returns true if `command` is the `rpmb` command group, which runs against
// `fuchsia.hardware.rpmb.Service` instead of `fuchsia.hardware.ufs.Service`.
bool IsRpmbCommand(std::string_view command);

// Prints the RPMB commands for the top-level usage message.
void PrintRpmbUsage(std::ostream& out);

// Runs the RPMB subcommand in `argv[3]` of the `rpmb` group in `argv[2]`. `argv[1]` is the path
// that `client` was connected to and is only used in error messages.
zx::result<> RunRpmb(fidl::WireSyncClient<fuchsia_hardware_rpmb::Rpmb> client, int argc,
                     char** argv, std::ostream& out, std::ostream& err);

zx::result<fidl::WireSyncClient<fuchsia_hardware_rpmb::Rpmb>> OpenRpmbDevice(const char* path);

}  // namespace ufsutil

#endif  // SRC_DEVICES_BLOCK_BIN_UFSUTIL_RPMB_H_
