// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "ufsutil.h"

#include <getopt.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <zircon/errors.h>

#include <cstdlib>
#include <format>
#include <functional>
#include <map>
#include <optional>
#include <ostream>
#include <string>
#include <unordered_map>
#include <utility>

#include "error-history.h"
#include "query.h"
#include "rpmb.h"
#include "uic.h"

namespace ufsutil {
namespace {
#define QUERY_REQUEST_COMMAND_USAGE_MESSAGE(command, extra_options)     \
  "Usage: ufsutil <device> " command                                    \
  " [options]\n"                                                        \
  "options:\n"                                                          \
  "  [--type=<NUM>, -t <NUM>]        Descriptor type idn. (required)\n" \
  "  [--index=<NUM>, -i <NUM>]       (default = 0)\n"                   \
  "  [--selector=<NUM>, -s <NUM>]    (default = 0)\n" extra_options

constexpr char kReadDescUsageMessage[] = QUERY_REQUEST_COMMAND_USAGE_MESSAGE("read-desc", "");
constexpr char kSetFlagUsageMessage[] = QUERY_REQUEST_COMMAND_USAGE_MESSAGE("set-flag", "");
constexpr char kReadFlagUsageMessage[] = QUERY_REQUEST_COMMAND_USAGE_MESSAGE("read-flag", "");
constexpr char kToggleFlagUsageMessage[] = QUERY_REQUEST_COMMAND_USAGE_MESSAGE("toggle-flag", "");
constexpr char kClearFlagUsageMessage[] = QUERY_REQUEST_COMMAND_USAGE_MESSAGE("clear-flag", "");
constexpr char kReadAttributeUsageMessage[] = QUERY_REQUEST_COMMAND_USAGE_MESSAGE("read-attr", "");
constexpr char kWriteDescUsageMessage[] = QUERY_REQUEST_COMMAND_USAGE_MESSAGE(
    "write-desc",
    "  [--value=<NUM>, -v <NUM>]\n"
    "  [--file=<FILE_PATH>, -f <FILE_PATH>]  (required by the configuration descriptor)\n");
constexpr char kWriteAttributeUsageMessage[] =
    QUERY_REQUEST_COMMAND_USAGE_MESSAGE("write-attr", "  [--value=<NUM>, -v <NUM>]\n");
constexpr char kErrorHistoryUsageMessage[] =
    "Usage: ufsutil <device> error-history [options]\n"
    "options:\n"
    "  [--out-dir=<OUTPUT_DIR>, -o <OUTPUT_DIR>]    (required)\n"
    "\n"
    "  Retrieves error history data from the UFS device and saves it to files\n"
    "  in the specified output directory.\n"
    "\n"
    "  Each error history entry will be saved as:\n"
    "    ufsutil_error_entry_<buffer_id>.bin (buffer_id in hex, e.g. 10)\n";
constexpr char kDmeGetUsageMessage[] =
    "Usage: ufsutil <device> dme-get [options]\n"
    "options:\n"
    "  [--id=<NUM>, -i <NUM>]          ID of the attribute (required)\n"
    "  [--selector=<NUM>, -s <NUM>]    GenSelectorIndex (default = 0)\n"
    "  [--peer, -p]                    Access to peer UniPort (default: local)\n";
constexpr char kDmeSetUsageMessage[] =
    "Usage: ufsutil <device> dme-set [options]\n"
    "options:\n"
    "  [--id=<NUM>, -i <NUM>]          ID of the attribute (required)\n"
    "  [--selector=<NUM>, -s <NUM>]    GenSelectorIndex (default = 0)\n"
    "  [--peer, -p]                    Access to peer UniPort (default: local)\n"
    "  [--value=<NUM>, -v <NUM>]       (required)\n"
    "\n"
    "Only volatile values of the attributes allowed by the driver (e.g. PA_TActivate,\n"
    "PA_Granularity) can be set.\n";

using CommandHandler = std::function<zx::result<>(
    const fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>&,
    const std::unordered_map<uint32_t, OptionValue>&, std::ostream&, std::ostream&)>;

const char* kQueryRequestShortOptions = "ht:i:s:";
const struct option kQueryRequestOptions[] = {
    {.name = "help", .has_arg = no_argument, .flag = nullptr, .val = 'h'},
    {.name = "type", .has_arg = required_argument, .flag = nullptr, .val = 't'},
    {.name = "index", .has_arg = required_argument, .flag = nullptr, .val = 'i'},
    {.name = "selector", .has_arg = required_argument, .flag = nullptr, .val = 's'},
    {.name = nullptr, .has_arg = 0, .flag = nullptr, .val = 0},
};

const char* kWriteAttributeShortOptions = "ht:i:s:v:";
const struct option kWriteAttributeOptions[] = {
    {.name = "help", .has_arg = no_argument, .flag = nullptr, .val = 'h'},
    {.name = "type", .has_arg = required_argument, .flag = nullptr, .val = 't'},
    {.name = "index", .has_arg = required_argument, .flag = nullptr, .val = 'i'},
    {.name = "selector", .has_arg = required_argument, .flag = nullptr, .val = 's'},
    {.name = "value", .has_arg = required_argument, .flag = nullptr, .val = 'v'},
    {.name = nullptr, .has_arg = 0, .flag = nullptr, .val = 0},
};

const char* kWriteDescriptorShortOptions = "ht:i:s:v:f:";
const struct option kWriteDescriptorOptions[] = {
    {.name = "help", .has_arg = no_argument, .flag = nullptr, .val = 'h'},
    {.name = "type", .has_arg = required_argument, .flag = nullptr, .val = 't'},
    {.name = "index", .has_arg = required_argument, .flag = nullptr, .val = 'i'},
    {.name = "selector", .has_arg = required_argument, .flag = nullptr, .val = 's'},
    {.name = "value", .has_arg = required_argument, .flag = nullptr, .val = 'v'},
    {.name = "file", .has_arg = required_argument, .flag = nullptr, .val = 'f'},
    {.name = nullptr, .has_arg = 0, .flag = nullptr, .val = 0},
};

const char* kErrorHistoryShortOptions = "ho:";
const struct option kErrorHistoryLongOptions[] = {
    {.name = "help", .has_arg = no_argument, .flag = nullptr, .val = 'h'},
    {.name = "out-dir", .has_arg = required_argument, .flag = nullptr, .val = 'o'},
    {.name = nullptr, .has_arg = 0, .flag = nullptr, .val = 0},
};

const char* kDmeGetShortOptions = "hi:s:p";
const struct option kDmeGetLongOptions[] = {
    {.name = "help", .has_arg = no_argument, .flag = nullptr, .val = 'h'},
    {.name = "id", .has_arg = required_argument, .flag = nullptr, .val = 'i'},
    {.name = "selector", .has_arg = required_argument, .flag = nullptr, .val = 's'},
    {.name = "peer", .has_arg = no_argument, .flag = nullptr, .val = 'p'},
    {.name = nullptr, .has_arg = 0, .flag = nullptr, .val = 0},
};

const char* kDmeSetShortOptions = "hi:s:v:p";
const struct option kDmeSetLongOptions[] = {
    {.name = "help", .has_arg = no_argument, .flag = nullptr, .val = 'h'},
    {.name = "id", .has_arg = required_argument, .flag = nullptr, .val = 'i'},
    {.name = "selector", .has_arg = required_argument, .flag = nullptr, .val = 's'},
    {.name = "value", .has_arg = required_argument, .flag = nullptr, .val = 'v'},
    {.name = "peer", .has_arg = no_argument, .flag = nullptr, .val = 'p'},
    {.name = nullptr, .has_arg = 0, .flag = nullptr, .val = 0},
};

enum class UfsCommand : uint8_t {
  READ_DESC,
  WRITE_DESC,
  READ_FLAG,
  SET_FLAG,
  CLEAR_FLAG,
  TOGGLE_FLAG,
  READ_ATTR,
  WRITE_ATTR,
  ERROR_HISTORY,
  DME_GET,
  DME_SET,
};

struct CommandDefinition {
  UfsCommand command_type;
  const char* description;
  const struct option* long_opts;
  const char* short_opts;
  CommandHandler handler;
  const char* helpMessage;
};

struct ParsedCommand {
  std::string name;
  std::unordered_map<uint32_t, OptionValue> options;
  CommandHandler handler;
};

std::map<std::string, CommandDefinition> commandRegistry;

void registerCommand(const std::string& cmd_name, const CommandDefinition& cmd) {
  commandRegistry[cmd_name] = cmd;
}

zx::result<std::optional<ParsedCommand>> ParseCommand(int argc, char** argv, std::ostream& out,
                                                      std::ostream& err) {
  if (argc < 3) {
    PrintUsage(out);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  std::string cmd_name = argv[2];
  auto cmd = commandRegistry.find(cmd_name);
  if (cmd == commandRegistry.end()) {
    err << std::format("error: Invalid command '{}'\n", cmd_name);
    PrintUsage(out);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  ParsedCommand result{
      .name = cmd_name,
      .options = {},
      .handler = cmd->second.handler,
  };

  opterr = 0;
  optind = 3;
  int opt;
  while ((opt = getopt_long(argc, argv, cmd->second.short_opts, cmd->second.long_opts, nullptr)) !=
         -1) {
    switch (opt) {
      case '?':
        err << std::format("error: invalid or incomplete option for '{}'\n", cmd_name);
        out << cmd->second.description << "\n" << cmd->second.helpMessage;
        return zx::error(ZX_ERR_INVALID_ARGS);
      case 'h':
        out << cmd->second.description << "\n" << cmd->second.helpMessage;
        return zx::ok(std::nullopt);
      case 'f':
      case 'o':
        result.options[opt] = optarg;
        break;
      case 't':
      case 'i':
      case 's': {
        auto val = ParseStrToUint32(optarg);
        if (!val) {
          err << std::format("error: invalid argument for -{:c}\n", static_cast<char>(opt));
          return zx::error(ZX_ERR_INVALID_ARGS);
        }
        result.options[opt] = *val;
      } break;
      case 'v': {
        if (cmd->second.command_type == UfsCommand::WRITE_DESC) {
          result.options[opt] = optarg;
        } else if (cmd->second.command_type == UfsCommand::WRITE_ATTR ||
                   cmd->second.command_type == UfsCommand::DME_SET) {
          auto val = ParseStrToUint32(optarg);
          if (!val) {
            err << std::format("error: invalid argument for -{:c}\n", static_cast<char>(opt));
            return zx::error(ZX_ERR_INVALID_ARGS);
          }
          result.options[opt] = *val;
        }
      } break;
      case 'p':
        // Flag option: presence is what matters.
        result.options[opt] = 0u;
        break;
      default:
        break;
    }
  }

  return zx::ok(std::move(result));
}

zx::result<> ExecuteCommand(fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>& client,
                            const ParsedCommand& cmd, std::ostream& out, std::ostream& err) {
  return cmd.handler(client, cmd.options, out, err);
}

}  // namespace

void Initialize() {
  registerCommand("read-desc",
                  {.command_type = UfsCommand::READ_DESC,
                   .description = "Retrieve the characteristics and functions of the device.\n",
                   .long_opts = kQueryRequestOptions,
                   .short_opts = kQueryRequestShortOptions,
                   .handler = HandleReadDescriptor,
                   .helpMessage = kReadDescUsageMessage});
  registerCommand("write-desc",
                  {.command_type = UfsCommand::WRITE_DESC,
                   .description = "Configure the characteristics and functions of the device.\n",
                   .long_opts = kWriteDescriptorOptions,
                   .short_opts = kWriteDescriptorShortOptions,
                   .handler = HandleWriteDescriptor,
                   .helpMessage = kWriteDescUsageMessage});
  registerCommand("set-flag",
                  {.command_type = UfsCommand::SET_FLAG,
                   .description = "Enable flags to activate device features or modes.\n",
                   .long_opts = kQueryRequestOptions,
                   .short_opts = kQueryRequestShortOptions,
                   .handler = HandleSetFlag,
                   .helpMessage = kSetFlagUsageMessage});
  registerCommand("read-flag",
                  {.command_type = UfsCommand::READ_FLAG,
                   .description = "Retrieve flags to check which features or modes are enabled.\n",
                   .long_opts = kQueryRequestOptions,
                   .short_opts = kQueryRequestShortOptions,
                   .handler = HandleReadFlag,
                   .helpMessage = kReadFlagUsageMessage});
  registerCommand("toggle-flag",
                  {.command_type = UfsCommand::TOGGLE_FLAG,
                   .description = "Toggle flags to control device features or modes.\n",
                   .long_opts = kQueryRequestOptions,
                   .short_opts = kQueryRequestShortOptions,
                   .handler = HandleToggleFlag,
                   .helpMessage = kToggleFlagUsageMessage});
  registerCommand("clear-flag",
                  {.command_type = UfsCommand::CLEAR_FLAG,
                   .description = "Clear flags to disable device functions or modes. \n",
                   .long_opts = kQueryRequestOptions,
                   .short_opts = kQueryRequestShortOptions,
                   .handler = HandleClearFlag,
                   .helpMessage = kClearFlagUsageMessage});
  registerCommand("read-attr",
                  {.command_type = UfsCommand::READ_ATTR,
                   .description = "Retrieve attributes that represent numeric ranges.\n",
                   .long_opts = kQueryRequestOptions,
                   .short_opts = kQueryRequestShortOptions,
                   .handler = HandleReadAttribute,
                   .helpMessage = kReadAttributeUsageMessage});
  registerCommand("write-attr",
                  {.command_type = UfsCommand::WRITE_ATTR,
                   .description = "Configure attributes that represent numeric ranges.\n",
                   .long_opts = kWriteAttributeOptions,
                   .short_opts = kWriteAttributeShortOptions,
                   .handler = HandleWriteAttribute,
                   .helpMessage = kWriteAttributeUsageMessage});
  registerCommand(
      "error-history",
      {.command_type = UfsCommand::ERROR_HISTORY,
       .description = "Retrieve the error history directory and data of the UFS device.\n",
       .long_opts = kErrorHistoryLongOptions,
       .short_opts = kErrorHistoryShortOptions,
       .handler = HandleErrorHistory,
       .helpMessage = kErrorHistoryUsageMessage});
  registerCommand(
      "dme-get",
      {.command_type = UfsCommand::DME_GET,
       .description = "Retrieve UniPro and M-PHY attributes of the local or peer UniPort.\n",
       .long_opts = kDmeGetLongOptions,
       .short_opts = kDmeGetShortOptions,
       .handler = HandleDmeGet,
       .helpMessage = kDmeGetUsageMessage});
  registerCommand(
      "dme-set",
      {.command_type = UfsCommand::DME_SET,
       .description = "Configure UniPro and M-PHY attributes of the local or peer UniPort.\n",
       .long_opts = kDmeSetLongOptions,
       .short_opts = kDmeSetShortOptions,
       .handler = HandleDmeSet,
       .helpMessage = kDmeSetUsageMessage});
}

void PrintUsage(std::ostream& out) {
  out << "Usage: ufsutil <device> <command> [options]\n"
         "   <device>       Path to the UFS device service endpoint, e.g., "
         "/svc/fuchsia.hardware.ufs.Service/<instance_id>/device\n\n"
         "Available commands:\n";
  for (const auto& cmd : commandRegistry) {
    out << std::format("  {:<40}  {}", cmd.first, cmd.second.description);
  }
  PrintRpmbUsage(out);
  out << "\nType 'ufsutil <device> <command> -h' for help on a specific command.\n"
         "\nSupported Specifiation: JEDEC UFS 3.1 (JESD220E)\n";
}

zx::result<> RunUfsUtils(fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs> client, int argc,
                         char** argv, std::ostream& out, std::ostream& err) {
  zx::result<std::optional<ParsedCommand>> cmd = ParseCommand(argc, argv, out, err);
  if (cmd.is_error()) {
    return cmd.take_error();
  }
  const std::optional<ParsedCommand>& parsed = cmd.value();
  if (!parsed.has_value()) {
    return zx::ok();
  }

  return ExecuteCommand(client, *parsed, out, err);
}

zx::result<fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>> OpenDevice(const char* dev) {
  zx::result result = component::Connect<fuchsia_hardware_ufs::Ufs>(dev);
  if (result.is_error()) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  return zx::ok(fidl::WireSyncClient(std::move(result.value())));
}

}  // namespace ufsutil
