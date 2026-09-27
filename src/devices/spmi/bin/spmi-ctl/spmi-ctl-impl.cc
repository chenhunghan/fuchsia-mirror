// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "spmi-ctl-impl.h"

#include <fidl/fuchsia.hardware.spmi/cpp/fidl.h>
#include <fidl/fuchsia.hardware.spmi/cpp/natural_ostream.h>
#include <getopt.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/device-watcher/cpp/device-watcher.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/fdio.h>
#include <zircon/status.h>

#include <algorithm>
#include <cerrno>
#include <cstdlib>
#include <filesystem>
#include <format>
#include <iostream>
#include <limits>
#include <optional>
#include <sstream>
namespace {

enum class ParseResult {
  kSuccess,
  kInvalid,
  kOutOfRange,
};

// Parses a numerical string and verifies that the complete argument is consumed.
ParseResult ParseInteger(const char* str, int64_t* out_value) {
  if (str == nullptr || *str == '\0') {
    return ParseResult::kInvalid;
  }
  char* endptr = nullptr;
  errno = 0;
  long long val = std::strtoll(str, &endptr, 0);
  if (errno == ERANGE) {
    return ParseResult::kOutOfRange;
  }
  if (endptr == str || *endptr != '\0') {
    return ParseResult::kInvalid;
  }
  *out_value = val;
  return ParseResult::kSuccess;
}

constexpr char kSpmiDebugServiceDir[] = "/svc/fuchsia.hardware.spmi.DebugService";

constexpr char kUsageSummary[] = R"""(
SPMI driver control.

Usage:
  spmi-ctl [-c|--controller <controller>] [-i|--width <width>]
           -t|--target <id> -a|--address <address> -r|--read <registers_to_read>
  spmi-ctl [-c|--controller <controller>]
           -t|--target <id> -a|--address <address> -w|--write <byte_0> <byte_1>...
  spmi-ctl [-c|--controller <controller>] [-i|--width <width>]
           -t|--target <id> -a|--address <address> -d|--dump <bytes_to_dump>
  spmi-ctl [-c|--controller <controller>] -t|--target <id> -p|--properties
  spmi-ctl -l|--list
  spmi-ctl -h|--help
)""";

constexpr char kUsageDetails[] = R"""(
Options:
  -c, --controller  Controller device name. If specified, must be listed before other options.
                    If left unspecified, the first device in
                    /svc/fuchsia.hardware.spmi.DebugService is used.
  -t, --target      Target ID in [0, 15]. Must be listed before the following options.
  -a, --address     Address to read or write. Must be listed before --read or --write.
  -i, --width       Register width in bytes (e.g. 1, 2, 4). If left unspecified,
                    queries device properties (defaulting to 1 if unspecified by device).
  -r, --read        Reads <registers_to_read> registers from the device one by one,
                    displaying each register's address and value. Terminates on error.
  -w, --write       Writes <byte0>, <byte1>, etc to the device. Width is not required as
                    data is provided directly as individual bytes.
  -d, --dump        Dumps <dump_bytes> from the device into a 16-byte aligned table,
                    reading one register at a time. Truncated to a multiple of register
                    width if necessary. If there is an error, continues with the next
                    register and displays "--" for unreadable bytes.
  -p, --properties  Retrieves device properties.
  -l, --list        Lists all devices available.
  -h, --help        Show list of command-line options.

Examples:

Write 0x12 (one byte) to address 0x1234 using a Write SPMI command:
$ spmi-ctl -t 0 -a 0x1234 -w 0x12 0x34 0x56 0x78
Executing on device: /svc/fuchsia.hardware.spmi.DebugService/c3d9294786beb5a906e4dbd5fd3b596e/device

Read one register from address 0x1234 using a Read SPMI command:
$ spmi-ctl -t 0 -a 0x5678 -r 1
Executing on device: /svc/fuchsia.hardware.spmi.DebugService/c3d9294786beb5a906e4dbd5fd3b596e/device
fuchsia_hardware_spmi::DeviceRegisterReadResponse{ data = [ 219, ], }

Get properties for device named "pmic":
$ spmi-ctl -c pmic -t 0 -p
Executing on device: pmic
fuchsia_hardware_spmi::DeviceGetPropertiesResponse{ sid = 0, name = "pmic", }

List all devices:
$ spmi-ctl -l
Controllers found:
- name: pmic

)""";

template <typename T>
std::string ToString(const T& value) {
  std::ostringstream buf;
  buf << value;
  return buf.str();
}
template <typename T>
std::string FidlString(const T& value) {
  return ToString(fidl::ostream::Formatted<T>(value));
}

// Prints formatted register values according to the device's register width.
void PrintRegisters(uint16_t base_address, const std::vector<uint8_t>& data,
                    uint32_t register_width) {
  if (register_width == 0) {
    register_width = 1;
  }
  size_t offset = 0;
  // Current register address being formatted, incremented per register.
  uint16_t reg_addr = base_address;
  while (offset < data.size()) {
    const size_t bytes = std::min<size_t>(register_width, data.size() - offset);
    uint64_t val = 0;
    std::string hex_str;
    // Assemble hexadecimal string (MSB to LSB) and integer value for the register.
    for (size_t i = 0; i < bytes; ++i) {
      const uint8_t byte = data[offset + bytes - 1 - i];
      hex_str += std::format("{:02x}", byte);
      val = (val << 8) | byte;
    }
    std::cout << std::format("Register: 0x{:04x}  value: 0x{} ({})\n", reg_addr, hex_str, val);
    offset += bytes;
    reg_addr++;
  }
}

// Prints dumped byte data in a 16-byte aligned table format with header and row addresses.
// Displays raw bytes across 16 columns regardless of register width.
// Failed or unreadable bytes are displayed as "--".
void PrintDumpTable(uint16_t start_address,
                    const std::vector<std::optional<uint8_t>>& dumped_bytes) {
  if (dumped_bytes.empty()) {
    return;
  }
  constexpr size_t kBytesPerRow = 16;
  // Mask to compute the 16-byte aligned base address of each row.
  constexpr uint16_t kRowMask = 0xfff0;
  constexpr char kHeader[] = "       0  1  2  3  4  5  6  7  8  9  a  b  c  d  e  f\n";

  std::cout << kHeader;
  uint16_t current_address = start_address;
  size_t byte_idx = 0;
  while (byte_idx < dumped_bytes.size()) {
    const uint16_t row_base = static_cast<uint16_t>(current_address & kRowMask);
    std::string hex_col;
    for (size_t col = 0; col < kBytesPerRow; ++col) {
      const uint16_t addr = static_cast<uint16_t>(row_base + col);
      if (addr < current_address) {
        // Leading padding before the first dumped byte in this row.
        hex_col += "  ";
      } else if (byte_idx < dumped_bytes.size()) {
        const std::optional<uint8_t>& byte = dumped_bytes[byte_idx++];
        current_address++;
        if (byte.has_value()) {
          hex_col += std::format("{:02x}", *byte);
        } else {
          hex_col += "--";
        }
        if (byte_idx == dumped_bytes.size()) {
          break;
        }
      }
      if (col + 1 < kBytesPerRow) {
        hex_col += ' ';
      }
    }
    std::cout << std::format("{:04x}: {}\n", row_base, hex_col);
  }
}

void ShowUsage(bool show_details) {
  std::cout << kUsageSummary;
  if (!show_details) {
    std::cout << std::endl << "Use `spmi-ctl --help` to see full help text" << std::endl;
    return;
  }
  std::cout << kUsageDetails;
}

fidl::SyncClient<fuchsia_hardware_spmi::Debug> GetControllerByName(std::string_view name) {
  for (const auto& entry : std::filesystem::directory_iterator(kSpmiDebugServiceDir)) {
    std::string path = entry.path().string() + "/device";
    zx::result connector = component::Connect<fuchsia_hardware_spmi::Debug>(path);
    if (connector.is_error()) {
      continue;
    }
    auto client = fidl::SyncClient<fuchsia_hardware_spmi::Debug>(std::move(connector.value()));
    auto result = client->GetControllerProperties();
    if (result.is_ok() && result->name().has_value() && *result->name() == name) {
      return client;
    }
  }

  return {};
}

std::vector<fidl::Response<fuchsia_hardware_spmi::Debug::GetControllerProperties>>
GetAllControllerProperties(std::string_view directory) {
  std::vector<fidl::Response<fuchsia_hardware_spmi::Debug::GetControllerProperties>> responses;
  for (const auto& entry : std::filesystem::directory_iterator(directory)) {
    std::string path = entry.path().string() + "/device";
    zx::result connector = component::Connect<fuchsia_hardware_spmi::Debug>(path);
    if (connector.is_error()) {
      continue;
    }
    auto client = fidl::SyncClient<fuchsia_hardware_spmi::Debug>(std::move(connector.value()));
    if (auto result = client->GetControllerProperties(); result.is_ok()) {
      responses.push_back(*std::move(result));
    }
  }

  return responses;
}

void PrintControllerProperties(
    const fidl::Response<fuchsia_hardware_spmi::Debug::GetControllerProperties>& properties) {
  if (properties.name().has_value()) {
    std::cout << "- name: " << *properties.name();
  } else {
    std::cout << "- no name";
  }
  std::cout << std::endl;
}

// Queries device properties to determine the register width in bytes, defaulting
// to 1 byte if unavailable or invalid.
uint32_t GetRegisterWidthBytes(const fidl::SyncClient<fuchsia_hardware_spmi::Device>& client) {
  constexpr uint32_t kDefaultRegisterWidthBytes = 1;
  auto props = client->GetProperties();
  if (props.is_ok() && props->register_width_bytes().has_value() &&
      *props->register_width_bytes() > 0) {
    return *props->register_width_bytes();
  }
  return kDefaultRegisterWidthBytes;
}

}  // namespace

fidl::SyncClient<fuchsia_hardware_spmi::Device> SpmiCtl::GetSpmiClient(std::string controller,
                                                                       uint8_t target) {
  if (!test_client_ && !std::filesystem::exists(kSpmiDebugServiceDir)) {
    std::cerr << "Folder " << kSpmiDebugServiceDir << " not found" << std::endl;
    return {};
  }

  fidl::SyncClient<fuchsia_hardware_spmi::Debug> debug;
  if (test_client_) {
    debug = *std::move(test_client_);
  } else if (controller.empty()) {
    // If controller is not specified, use the first target entry in devfs.
    for (const auto& entry : std::filesystem::directory_iterator(kSpmiDebugServiceDir)) {
      controller = entry.path().string();
      zx::result connector =
          component::Connect<fuchsia_hardware_spmi::Debug>(controller + "/device");
      if (connector.is_error()) {
        continue;
      }
      auto client = fidl::SyncClient<fuchsia_hardware_spmi::Debug>(std::move(connector.value()));
      if (auto result = client->GetControllerProperties(); result.is_ok()) {
        debug = std::move(client);
        if (result->name().has_value()) {
          controller = *result->name();
        }
        break;
      }
    }
    if (!debug.is_valid()) {
      std::cerr << "no device found in: " << kSpmiDebugServiceDir << std::endl;
      return {};
    }
  } else if (debug = GetControllerByName(controller); !debug.is_valid()) {  // Try to find by name.
    std::cerr << "No controller found with name " << controller << std::endl;
    return {};
  }

  auto [device_client, device_server] = fidl::Endpoints<fuchsia_hardware_spmi::Device>::Create();
  if (auto result = debug->ConnectTarget({target, std::move(device_server)}); result.is_error()) {
    std::cerr << "could not connect to target " << static_cast<uint32_t>(target)
              << " on controller " << controller
              << " status:" << result.error_value().FormatDescription() << std::endl;
    return {};
  }

  std::cout << "Executing on controller: " << controller << std::endl;
  return fidl::SyncClient<fuchsia_hardware_spmi::Device>(std::move(device_client));
}

void SpmiCtl::ListDevices() {
  if (!std::filesystem::exists(kSpmiDebugServiceDir)) {
    std::cerr << "Folder " << kSpmiDebugServiceDir << " not found" << std::endl;
    return;
  }

  std::vector controller_properties = GetAllControllerProperties(kSpmiDebugServiceDir);

  if (controller_properties.empty()) {
    std::cerr << "No controllers found" << std::endl;
    return;
  }

  std::cout << "Controllers found: " << std::endl;

  for (const auto& properties : controller_properties) {
    PrintControllerProperties(properties);
  }
}

int SpmiCtl::Execute(int argc, char** argv) {
  std::string controller = {};
  std::optional<uint8_t> target;
  std::optional<uint16_t> address;
  std::optional<uint32_t> register_width_arg;
  optind = 0;

  while (true) {
    static struct option long_options[] = {
        {"help", no_argument, 0, 'h'},
        {"controller", required_argument, 0, 'c'},
        {"target", required_argument, 0, 't'},
        {"address", required_argument, 0, 'a'},
        {"read", required_argument, 0, 'r'},
        {"write", required_argument, 0, 'w'},
        {"dump", required_argument, 0, 'd'},
        {"properties", no_argument, 0, 'p'},
        {"list", no_argument, 0, 'l'},
        {"width", required_argument, 0, 'i'},
        {0, 0, 0, 0},
    };

    int c = getopt_long(argc, argv, "hc:t:a:r:w:d:pli:", long_options, 0);
    if (c == -1)
      break;

    switch (c) {
      case 'h':
        ShowUsage(true);
        return 0;

      case 'c':
        controller = optarg;
        break;

      case 'i': {
        // Register width in bytes; must be greater than 0.
        int64_t width = 0;
        const auto res = ParseInteger(optarg, &width);
        if (res == ParseResult::kInvalid) {
          ShowUsage(false);
          return -1;
        }
        if (res == ParseResult::kOutOfRange || width < 1 ||
            width > std::numeric_limits<uint32_t>::max()) {
          std::cerr << "Width failed: must be between 1 and 0xffffffff inclusive" << std::endl;
          return -1;
        }
        register_width_arg = static_cast<uint32_t>(width);
        break;
      }

      case 't': {
        // Target ID must be within [0, 15].
        int64_t target_id = 0;
        const auto res = ParseInteger(optarg, &target_id);
        if (res == ParseResult::kInvalid) {
          ShowUsage(false);
          return -1;
        }
        if (res == ParseResult::kOutOfRange || target_id < 0 ||
            target_id >= static_cast<int64_t>(fuchsia_hardware_spmi::kMaxTargets)) {
          std::cerr << "target must be between 0 and 15 inclusive" << std::endl;
          return -1;
        }
        target = static_cast<uint8_t>(target_id);
        break;
      }

      case 'a': {
        // Register address must fit within 16 bits [0, 0xffff].
        int64_t local_address = 0;
        const auto res = ParseInteger(optarg, &local_address);
        if (res == ParseResult::kInvalid) {
          ShowUsage(false);
          return -1;
        }
        if (res == ParseResult::kOutOfRange || local_address < 0 || local_address > 0xffff) {
          std::cerr << "Address failed: must be between 0 and 0xffff inclusive" << std::endl;
          return -1;
        }
        address.emplace(static_cast<uint16_t>(local_address));
      } break;

      case 'r': {
        if (!target || !address) {
          break;
        }

        // Parse number of registers to read; must be within [1, 0xffffffff].
        int64_t read_registers = 0;
        const auto res = ParseInteger(optarg, &read_registers);
        if (res == ParseResult::kInvalid) {
          ShowUsage(false);
          return -1;
        }
        if (res == ParseResult::kOutOfRange || read_registers < 1 ||
            read_registers > std::numeric_limits<uint32_t>::max()) {
          std::cerr << "Read failed: must be between 1 and 0xffffffff inclusive" << std::endl;
          return -1;
        }

        auto client = GetSpmiClient(controller, *target);
        if (!client.is_valid()) {
          return -1;
        }

        // Determine register width from command-line argument or device properties.
        const uint32_t reg_width = register_width_arg.value_or(GetRegisterWidthBytes(client));

        // Read registers one by one.
        for (size_t reg_idx = 0; reg_idx < static_cast<size_t>(read_registers); ++reg_idx) {
          if (static_cast<size_t>(*address) + reg_idx > std::numeric_limits<uint16_t>::max()) {
            std::cerr << "Read terminated: address out of 16 bits range" << std::endl;
            return -1;
          }
          const uint16_t local_address = static_cast<uint16_t>(*address + reg_idx);
          fuchsia_hardware_spmi::DeviceRegisterReadRequest request;
          request.address(local_address);
          request.size_bytes(reg_width);

          auto result = client->RegisterRead(std::move(request));
          if (result.is_error()) {
            std::cerr << "Read failed: " << result.error_value().FormatDescription() << std::endl;
            return -1;
          }
          PrintRegisters(local_address, result->data(), reg_width);
        }
        return 0;
      } break;

      case 'd': {
        if (!target || !address) {
          break;
        }

        // Parse number of bytes to dump; must be within [1, 0xffffffff].
        int64_t dump_bytes = 0;
        const auto res = ParseInteger(optarg, &dump_bytes);
        if (res == ParseResult::kInvalid) {
          ShowUsage(false);
          return -1;
        }
        if (res == ParseResult::kOutOfRange || dump_bytes < 1 ||
            dump_bytes > std::numeric_limits<uint32_t>::max()) {
          std::cerr << "Dump failed: must be between 1 and 0xffffffff inclusive" << std::endl;
          return -1;
        }

        auto client = GetSpmiClient(controller, *target);
        if (!client.is_valid()) {
          return -1;
        }

        // Determine register width from device properties and optional argument.
        const uint32_t driver_reg_width = GetRegisterWidthBytes(client);
        const uint32_t reg_width = register_width_arg.value_or(driver_reg_width);

        // Dump size must be at least one register width.
        if (dump_bytes < reg_width) {
          std::cerr << "Dump failed: bytes to dump (" << dump_bytes
                    << ") must be at least register width (" << reg_width << ")\n";
          return -1;
        }

        // Truncate dump size to a multiple of the register width if needed.
        if (dump_bytes % reg_width != 0) {
          dump_bytes -= dump_bytes % reg_width;
          std::cerr << std::format("WARNING: Truncating dump size to register width ({} bytes)\n",
                                   dump_bytes);
        }

        // Warn if the register width of the SPMI target is not 1 byte, as table
        // column offsets and row addresses reflect byte offsets rather than register indices.
        constexpr uint32_t kSingleByteRegisterWidth = 1;
        if (driver_reg_width != kSingleByteRegisterWidth) {
          std::cerr << std::format(
              "WARNING: Register width in the SPMI target is {} (not 1),\n"
              "         offsets are not the same as reading registers.\n",
              driver_reg_width);
        }

        // Dumped byte values from reads (std::nullopt indicates a read error).
        std::vector<std::optional<uint8_t>> dumped_bytes;

        // Maximum 16-bit address limit.
        constexpr size_t kMaxAddress16 = 0xffff;
        const size_t total_registers = static_cast<size_t>(dump_bytes) / reg_width;

        // Bound reservation to maximum addressable space to prevent excessive allocation.
        const size_t max_registers = kMaxAddress16 + 1 - *address;
        dumped_bytes.reserve(std::min(total_registers, max_registers) * reg_width);

        // Read registers one by one. If there is an error, continue with the next register.
        for (size_t reg_idx = 0; reg_idx < total_registers; ++reg_idx) {
          if (static_cast<size_t>(*address) + reg_idx > kMaxAddress16) {
            std::cerr << "Dump terminated: address out of 16 bits range" << std::endl;
            break;
          }
          const uint16_t local_address = static_cast<uint16_t>(*address + reg_idx);
          fuchsia_hardware_spmi::DeviceRegisterReadRequest request;
          request.address(local_address);
          request.size_bytes(reg_width);
          auto result = client->RegisterRead(std::move(request));
          if (result.is_error()) {
            if (result.error_value().is_framework_error()) {
              std::cerr << "Dump terminated: transport error ("
                        << result.error_value().FormatDescription() << ")" << std::endl;
              break;
            }
            for (size_t b = 0; b < reg_width; ++b) {
              dumped_bytes.push_back(std::nullopt);
            }
            continue;
          }
          for (uint8_t byte : result->data()) {
            dumped_bytes.push_back(byte);
          }
        }

        PrintDumpTable(*address, dumped_bytes);
        return 0;
      }

      case 'w': {
        if (!target || !address) {
          break;
        }

        // Writes accept explicit raw bytes, so register width is not needed.
        std::vector<uint8_t> write_bytes;
        int64_t write_byte = 0;
        // Parse first byte; must be a valid 8-bit integer in [0, 0xff].
        auto res = ParseInteger(optarg, &write_byte);
        if (res == ParseResult::kInvalid) {
          std::cerr << "Write failed: at least one byte must be provided" << std::endl;
          return -1;
        }
        if (res == ParseResult::kOutOfRange || write_byte < 0 || write_byte > 0xff) {
          std::cerr << "Write failed: byte must be between 0 and 0xff inclusive" << std::endl;
          return -1;
        }
        write_bytes.push_back(static_cast<uint8_t>(write_byte));
        // Add any additional bytes provided.
        while (optind < argc) {
          res = ParseInteger(argv[optind], &write_byte);
          if (res == ParseResult::kInvalid) {
            break;
          }
          if (res == ParseResult::kOutOfRange || write_byte < 0 || write_byte > 0xff) {
            std::cerr << "Write failed: byte must be between 0 and 0xff inclusive" << std::endl;
            return -1;
          }
          optind++;
          write_bytes.push_back(static_cast<uint8_t>(write_byte));
        }
        fuchsia_hardware_spmi::DeviceRegisterWriteRequest request;
        request.address(std::move(*address));
        request.data(std::move(write_bytes));
        auto client = GetSpmiClient(controller, *target);
        if (!client.is_valid()) {
          return -1;
        }
        auto result = client->RegisterWrite(std::move(request));
        if (result.is_error()) {
          std::cerr << "Write failed: " << result.error_value().FormatDescription() << std::endl;
          return -1;
        }
        std::cout << "Write success" << std::endl;

        return 0;
      } break;

      case 'p': {
        if (!target) {
          break;
        }

        auto client = GetSpmiClient(controller, *target);
        if (!client.is_valid()) {
          return -1;
        }
        auto result = client->GetProperties();
        if (result.is_error()) {
          std::cerr << "Get properties failed: " << result.error_value().FormatDescription()
                    << std::endl;
          return -1;
        }
        std::cout << FidlString(*result) << std::endl;
        return 0;
      }

      case 'l':
        ListDevices();
        return 0;

      default:
        ShowUsage(false);
        return -1;
    }
  }

  ShowUsage(false);
  return -1;
}
