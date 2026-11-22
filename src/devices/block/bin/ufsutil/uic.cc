// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "uic.h"

#include <fidl/fuchsia.hardware.ufs/cpp/wire_types.h>
#include <zircon/status.h>

#include <format>

#include "common.h"

namespace ufsutil {
namespace {
constexpr uint32_t UicArgMibSel(uint16_t attr, uint16_t sel) {
  return (static_cast<uint32_t>(attr) << 16u) | static_cast<uint32_t>(sel);
}

// Sends `opcode` with `args` and returns the result, or reports the FIDL or driver error to `err`.
zx::result<uint32_t> SendUicCommand(
    const fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>& client,
    fuchsia_hardware_ufs::wire::UicCommandOpcode opcode,
    const fidl::Array<uint32_t, fuchsia_hardware_ufs::wire::kUicCommandArgumentCount>& args,
    const char* command_name, std::ostream& err) {
  const fidl::WireResult result = client->SendUicCommand(opcode, args);
  if (!result.ok()) {
    err << std::format("error: failed to send {} command: {}\n", command_name,
                       result.status_string());
    return zx::error(result.status());
  }
  if (result->is_error()) {
    err << std::format("error: failed to send {} command: {}\n", command_name,
                       zx_status_get_string(result->error_value()));
    return zx::error(result->error_value());
  }
  return zx::ok(result->value()->result);
}
}  // namespace

zx::result<> HandleDmeGet(const fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>& client,
                          const std::unordered_map<uint32_t, OptionValue>& options,
                          std::ostream& out, std::ostream& err) {
  if (!CheckRequiredOption(options, 'i', "id", err)) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  uint16_t attr_id = 0;
  if (!GetOptionAs<uint16_t>(options, 'i', attr_id, err)) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  // Get GenSelectorIdx (if provided)
  uint16_t sel = 0;
  if (!GetOptionAs<uint16_t>(options, 's', sel, err)) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  fidl::Array<uint32_t, fuchsia_hardware_ufs::wire::kUicCommandArgumentCount> args{};
  args[0] = UicArgMibSel(attr_id, sel);

  fuchsia_hardware_ufs::wire::UicCommandOpcode opcode =
      options.contains('p') ? fuchsia_hardware_ufs::wire::UicCommandOpcode::kDmePeerGet
                            : fuchsia_hardware_ufs::wire::UicCommandOpcode::kDmeGet;

  zx::result<uint32_t> value = SendUicCommand(client, opcode, args, "dme get", err);
  if (value.is_error()) {
    return value.take_error();
  }
  out << std::format("0x{:x} : 0x{:x}\n", attr_id, *value);

  return zx::ok();
}

zx::result<> HandleDmeSet(const fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>& client,
                          const std::unordered_map<uint32_t, OptionValue>& options,
                          std::ostream& out, std::ostream& err) {
  if (!CheckRequiredOption(options, 'i', "id", err)) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  uint16_t attr_id = 0;
  if (!GetOptionAs<uint16_t>(options, 'i', attr_id, err)) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  if (!CheckRequiredOption(options, 'v', "value", err)) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  uint32_t value = std::get<uint32_t>(options.at('v'));

  // Get GenSelectorIdx (if provided)
  uint16_t sel = 0;
  if (!GetOptionAs<uint16_t>(options, 's', sel, err)) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  // Prepare UIC command arguments
  fidl::Array<uint32_t, fuchsia_hardware_ufs::wire::kUicCommandArgumentCount> args{};
  args[0] = UicArgMibSel(attr_id, sel);
  // AttrSetType 0 (volatile). The driver rejects static (non-volatile) DME_SET.
  args[1] = 0;
  args[2] = value;

  fuchsia_hardware_ufs::wire::UicCommandOpcode opcode =
      options.contains('p') ? fuchsia_hardware_ufs::wire::UicCommandOpcode::kDmePeerSet
                            : fuchsia_hardware_ufs::wire::UicCommandOpcode::kDmeSet;

  if (zx::result<uint32_t> result = SendUicCommand(client, opcode, args, "dme set", err);
      result.is_error()) {
    return result.take_error();
  }
  return zx::ok();
}

}  // namespace ufsutil
