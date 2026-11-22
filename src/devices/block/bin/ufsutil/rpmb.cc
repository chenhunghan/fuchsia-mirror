// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/block/bin/ufsutil/rpmb.h"

#include <endian.h>
#include <fcntl.h>
#include <getopt.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fit/defer.h>
#include <lib/zx/vmo.h>
#include <unistd.h>
#include <zircon/assert.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <format>
#include <iterator>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <vector>

#include <openssl/digest.h>
#include <openssl/hmac.h>
#include <openssl/mem.h>

#include "src/devices/block/bin/ufsutil/common.h"
#include "src/devices/block/bin/ufsutil/rpmb-frame.h"
#include "src/lib/files/directory.h"
#include "src/lib/files/file.h"
#include "src/lib/files/path.h"

namespace ufsutil {
namespace {

using RpmbClient = fidl::WireSyncClient<fuchsia_hardware_rpmb::Rpmb>;

constexpr uint64_t kFrameSize = fuchsia_hardware_rpmb::wire::kFrameSize;
// The UFS RPMB servers (C++ `rpmb.cc` and Rust `rpmb.rs`) accept at most 8 KiB in `tx_frames` and
// in `rx_frames`.
constexpr uint32_t kMaxFramesPerRequest = 16;
// The UFS RPMB servers send `tx_frames` in SECURITY PROTOCOL OUT commands of at most two frames
// (Rust: at most min(2, reliable_write_sector_count)). An authenticated data write must reach the
// device in one command.
constexpr uint32_t kMaxFramesPerWrite = 2;
constexpr size_t kBlockSize = sizeof(RpmbFrame::data);
// `EmmcDeviceInfo.rpmb_size` counts 128 KiB units.
constexpr uint64_t kRpmbSizeUnit = 128 * 1024;
// RPMB addresses are 16-bit block numbers.
constexpr uint32_t kRpmbAddressCount = 1u << 16;

constexpr char kRpmbServicePath[] = "/svc/fuchsia.hardware.rpmb.Service/<instance_id>/device";
// The top-level command (`argv[2]`) grouping the RPMB commands.
constexpr char kRpmbGroup[] = "rpmb";

struct RpmbArgs {
  std::optional<uint32_t> start;
  uint32_t count = 1;
  std::optional<std::string> file;
  std::optional<std::string> key;
};

using RpmbMac = std::array<uint8_t, kRpmbKeySize>;

// Holds an authentication key and clears it on destruction.
class RpmbKey {
 public:
  explicit RpmbKey(std::span<const uint8_t, kRpmbKeySize> bytes) {
    std::ranges::copy(bytes, bytes_.begin());
  }
  ~RpmbKey() { OPENSSL_cleanse(bytes_.data(), bytes_.size()); }
  RpmbKey(const RpmbKey&) = delete;
  RpmbKey& operator=(const RpmbKey&) = delete;
  // The deleted copy operations and the destructor already keep the move operations from being
  // declared implicitly. Delete them explicitly so that a later change to the declarations cannot
  // make the key movable, which would leave un-cleansed copies behind.
  RpmbKey(RpmbKey&&) = delete;
  RpmbKey& operator=(RpmbKey&&) = delete;

  // Loads a raw 32-byte key from `path`. Returns `std::nullopt` after reporting the reason to
  // `err` if the file cannot be read or does not hold exactly one key.
  static std::optional<RpmbKey> Load(const std::string& path, std::ostream& err) {
    // Read into a stack buffer that holds one byte more than a key, so that a longer file is
    // detected without keeping the key in heap memory, and cleanse it on every path.
    std::array<uint8_t, kRpmbKeySize + 1> buffer{};
    auto cleanse = fit::defer([&buffer] { OPENSSL_cleanse(buffer.data(), buffer.size()); });
    std::optional<size_t> size = ReadKeyFile(path, buffer, err);
    if (!size.has_value()) {
      return std::nullopt;
    }
    if (*size > kRpmbKeySize) {
      err << std::format("error: key file {} must hold {} bytes, but holds more\n", path,
                         kRpmbKeySize);
      return std::nullopt;
    }
    if (*size != kRpmbKeySize) {
      err << std::format("error: key file {} must hold {} bytes, not {}\n", path, kRpmbKeySize,
                         *size);
      return std::nullopt;
    }
    // The key cannot be moved, so construct it directly in the returned optional.
    return std::make_optional<RpmbKey>(std::span<const uint8_t>(buffer).first<kRpmbKeySize>());
  }

  // Returns the HMAC-SHA256 of `frames`, each from `data` to the end.
  RpmbMac Mac(std::span<const RpmbFrame> frames) const {
    bssl::ScopedHMAC_CTX ctx;
    ZX_ASSERT(HMAC_Init_ex(ctx.get(), bytes_.data(), bytes_.size(), EVP_sha256(), nullptr) == 1);
    for (const RpmbFrame& frame : frames) {
      ZX_ASSERT(HMAC_Update(ctx.get(), reinterpret_cast<const uint8_t*>(&frame) + kRpmbMacOffset,
                            sizeof(frame) - kRpmbMacOffset) == 1);
    }
    RpmbMac mac{};
    unsigned int mac_size = 0;
    ZX_ASSERT(HMAC_Final(ctx.get(), mac.data(), &mac_size) == 1);
    ZX_ASSERT(mac_size == mac.size());
    return mac;
  }

 private:
  // Reads at most `buffer.size()` bytes of `path` into `buffer`, or returns `std::nullopt`.
  static std::optional<size_t> ReadKeyFile(const std::string& path, std::span<uint8_t> buffer,
                                           std::ostream& err) {
    const int fd = open(path.c_str(), O_RDONLY);
    if (fd < 0) {
      err << std::format("error: failed to read key file {}\n", path);
      return std::nullopt;
    }
    size_t size = 0;
    bool failed = false;
    while (size < buffer.size()) {
      const ssize_t bytes = read(fd, buffer.data() + size, buffer.size() - size);
      if (bytes < 0) {
        if (errno == EINTR) {
          continue;
        }
        failed = true;
        break;
      }
      if (bytes == 0) {
        break;
      }
      size += static_cast<size_t>(bytes);
    }
    close(fd);
    if (failed) {
      err << std::format("error: failed to read key file {}\n", path);
      return std::nullopt;
    }
    return size;
  }

  std::array<uint8_t, kRpmbKeySize> bytes_{};
};

using RpmbHandler = zx::result<> (*)(const RpmbClient& client, const RpmbArgs& args,
                                     std::ostream& out, std::ostream& err);

struct RpmbCommand {
  std::string_view name;
  std::string_view description;
  std::string_view usage;
  const char* short_options;
  const struct option* long_options;
  RpmbHandler handler;
};

constexpr struct option kHelpOnlyOptions[] = {
    {.name = "help", .has_arg = no_argument, .flag = nullptr, .val = 'h'},
    {.name = nullptr, .has_arg = 0, .flag = nullptr, .val = 0},
};

constexpr struct option kCounterOptions[] = {
    {.name = "help", .has_arg = no_argument, .flag = nullptr, .val = 'h'},
    {.name = "key", .has_arg = required_argument, .flag = nullptr, .val = 'k'},
    {.name = nullptr, .has_arg = 0, .flag = nullptr, .val = 0},
};

constexpr struct option kReadOptions[] = {
    {.name = "help", .has_arg = no_argument, .flag = nullptr, .val = 'h'},
    {.name = "start", .has_arg = required_argument, .flag = nullptr, .val = 's'},
    {.name = "count", .has_arg = required_argument, .flag = nullptr, .val = 'n'},
    {.name = "file", .has_arg = required_argument, .flag = nullptr, .val = 'f'},
    {.name = "key", .has_arg = required_argument, .flag = nullptr, .val = 'k'},
    {.name = nullptr, .has_arg = 0, .flag = nullptr, .val = 0},
};

constexpr struct option kWriteOptions[] = {
    {.name = "help", .has_arg = no_argument, .flag = nullptr, .val = 'h'},
    {.name = "start", .has_arg = required_argument, .flag = nullptr, .val = 's'},
    {.name = "file", .has_arg = required_argument, .flag = nullptr, .val = 'f'},
    {.name = "key", .has_arg = required_argument, .flag = nullptr, .val = 'k'},
    {.name = nullptr, .has_arg = 0, .flag = nullptr, .val = 0},
};

// Builds a request frame of `type` with a fresh nonce.
RpmbFrame NewRequest(uint16_t type) {
  RpmbFrame frame{};
  frame.req_resp = htobe16(type);
  zx_cprng_draw(frame.nonce, sizeof(frame.nonce));
  return frame;
}

// Sends the frames of one RPMB operation and receives `response_count` frames in a single
// `Request` call. With a `response_count` of 0, the request has no `rx_frames`.
//
// A request carries the frames of one RPMB operation only, because the UFS drivers can send
// frames of different operations in one SECURITY PROTOCOL OUT command.
zx::result<std::vector<RpmbFrame>> Transact(const RpmbClient& client,
                                            std::span<const RpmbFrame> request,
                                            uint32_t response_count, std::ostream& err) {
  const uint64_t tx_size = request.size() * kFrameSize;
  zx::vmo tx_vmo;
  if (zx_status_t status = zx::vmo::create(tx_size, 0, &tx_vmo); status != ZX_OK) {
    err << std::format("error: failed to create VMO: {}\n", zx_status_get_string(status));
    return zx::error(status);
  }
  if (zx_status_t status = tx_vmo.write(request.data(), 0, tx_size); status != ZX_OK) {
    err << std::format("error: failed to write VMO: {}\n", zx_status_get_string(status));
    return zx::error(status);
  }

  fuchsia_hardware_rpmb::wire::Request rpmb_request = {};
  rpmb_request.tx_frames.vmo = std::move(tx_vmo);
  rpmb_request.tx_frames.offset = 0;
  rpmb_request.tx_frames.size = tx_size;

  const uint64_t rx_size = response_count * kFrameSize;
  zx::vmo rx_vmo;
  fuchsia_mem::wire::Range rx_range{.offset = 0, .size = rx_size};
  if (response_count > 0) {
    if (zx_status_t status = zx::vmo::create(rx_size, 0, &rx_vmo); status != ZX_OK) {
      err << std::format("error: failed to create VMO: {}\n", zx_status_get_string(status));
      return zx::error(status);
    }
    if (zx_status_t status = rx_vmo.duplicate(ZX_RIGHT_SAME_RIGHTS, &rx_range.vmo);
        status != ZX_OK) {
      err << std::format("error: failed to duplicate VMO: {}\n", zx_status_get_string(status));
      return zx::error(status);
    }
    rpmb_request.rx_frames = fidl::ObjectView<fuchsia_mem::wire::Range>::FromExternal(&rx_range);
  }

  const fidl::WireResult result = client->Request(std::move(rpmb_request));
  if (!result.ok()) {
    err << std::format("error: RPMB request failed: {}\n", result.status_string());
    return zx::error(result.status());
  }
  if (result->is_error()) {
    err << std::format("error: RPMB request failed: {}\n",
                       zx_status_get_string(result->error_value()));
    return zx::error(result->error_value());
  }

  std::vector<RpmbFrame> response(response_count);
  if (response_count == 0) {
    return zx::ok(std::move(response));
  }
  if (zx_status_t status = rx_vmo.read(response.data(), 0, rx_size); status != ZX_OK) {
    err << std::format("error: failed to read VMO: {}\n", zx_status_get_string(status));
    return zx::error(status);
  }
  return zx::ok(std::move(response));
}

// Describes a host-order `RpmbFrame::result` value, including the write counter status bit.
std::string DescribeResult(uint16_t result) {
  return std::format("{}{} (result 0x{:04X})", RpmbResultString(RpmbOperationResult(result)),
                     result & kRpmbResultWriteCounterExpired ? ", write counter expired" : "",
                     result);
}

zx::result<> CheckResponse(const RpmbFrame& request, const RpmbFrame& response,
                           uint16_t expected_type, std::string_view operation, std::ostream& err) {
  const uint16_t type = be16toh(response.req_resp);
  if (type != expected_type) {
    err << std::format("error: unexpected RPMB response type 0x{:04X} for {} (expected 0x{:04X})\n",
                       type, operation, expected_type);
    return zx::error(ZX_ERR_IO_DATA_INTEGRITY);
  }
  const uint16_t result = be16toh(response.result);
  if (RpmbOperationResult(result) != static_cast<uint16_t>(RpmbResult::kOk)) {
    err << std::format("error: RPMB {} failed: {}\n", operation, DescribeResult(result));
    return zx::error(ZX_ERR_IO);
  }
  if (!std::ranges::equal(request.nonce, response.nonce)) {
    err << std::format("error: RPMB {} response nonce does not match the request\n", operation);
    return zx::error(ZX_ERR_IO_DATA_INTEGRITY);
  }
  return zx::ok();
}

// Checks the frames of an authenticated data read response.
//
// Every frame of a successful response carries the nonce, the address and the block count of the
// request, and the last frame carries the result. After a response shorter than requested, the
// rest of `response` can hold frames of earlier requests, so a failed result is reported only from
// a frame that carries the request nonce.
zx::result<> CheckReadResponse(const RpmbFrame& request, std::span<const RpmbFrame> response,
                               std::ostream& err) {
  constexpr std::string_view kOperation = "authenticated data read";
  const auto from_request = [&request](const RpmbFrame& frame) {
    return std::ranges::equal(request.nonce, frame.nonce);
  };
  const auto operation_result = [](const RpmbFrame& frame) {
    return RpmbOperationResult(be16toh(frame.result));
  };
  constexpr uint16_t kOk = static_cast<uint16_t>(RpmbResult::kOk);

  const bool nonces_match = std::ranges::all_of(response, from_request);
  const uint16_t type = be16toh(response.back().req_resp);
  if (nonces_match && type == kRpmbAuthenticatedDataReadResponse &&
      operation_result(response.back()) == kOk) {
    auto other = std::ranges::find_if(response, [&request](const RpmbFrame& frame) {
      return frame.address != request.address || frame.block_count != request.block_count;
    });
    if (other == response.end()) {
      return zx::ok();
    }
    err << std::format(
        "error: RPMB {} response is for {} blocks at address 0x{:04X} (requested {} at 0x{:04X})\n",
        kOperation, be16toh(other->block_count), be16toh(other->address),
        be16toh(request.block_count), be16toh(request.address));
    return zx::error(ZX_ERR_IO_DATA_INTEGRITY);
  }

  auto failed = std::ranges::find_if(response, [&](const RpmbFrame& frame) {
    return from_request(frame) && operation_result(frame) != kOk;
  });
  if (failed != response.end()) {
    err << std::format("error: RPMB {} failed: {}\n", kOperation,
                       DescribeResult(be16toh(failed->result)));
    return zx::error(ZX_ERR_IO);
  }
  if (!nonces_match) {
    err << std::format("error: RPMB {} response nonce does not match the request\n", kOperation);
    return zx::error(ZX_ERR_IO_DATA_INTEGRITY);
  }
  err << std::format("error: unexpected RPMB response type 0x{:04X} for {} (expected 0x{:04X})\n",
                     type, kOperation, kRpmbAuthenticatedDataReadResponse);
  return zx::error(ZX_ERR_IO_DATA_INTEGRITY);
}

// Checks the MAC in the last of `response`, which covers all of `response`.
zx::result<> CheckMac(const RpmbKey& key, std::span<const RpmbFrame> response,
                      std::string_view operation, std::ostream& err) {
  const RpmbMac mac = key.Mac(response);
  if (CRYPTO_memcmp(mac.data(), response.back().key_mac, mac.size()) != 0) {
    err << std::format(
        "error: RPMB {} response MAC does not match; the key is wrong or the response was "
        "altered\n",
        operation);
    return zx::error(ZX_ERR_IO_DATA_INTEGRITY);
  }
  return zx::ok();
}

// Checks the result of an authenticated data write of the blocks at `address` with
// `write_counter`.
//
// The device keeps one result for all clients, and every request other than a result read
// replaces it. A successful result belongs to this write if it carries `address` and
// `write_counter + 1`. A failed result with `address` and `write_counter` shows that the blocks
// were not written. Any other result leaves the outcome unknown: a failed result with another
// write counter is either a counter failure of this write or the failure of a later write by
// another client.
zx::result<> CheckWriteResult(const RpmbKey& key, const RpmbFrame& response, uint16_t address,
                              uint32_t write_counter, std::ostream& err) {
  constexpr std::string_view kOperation = "authenticated data write";
  const uint16_t type = be16toh(response.req_resp);
  if (type != kRpmbAuthenticatedDataWriteResponse) {
    err << std::format("error: unexpected RPMB response type 0x{:04X} for {} (expected 0x{:04X})\n",
                       type, kOperation, kRpmbAuthenticatedDataWriteResponse);
    return zx::error(ZX_ERR_IO_DATA_INTEGRITY);
  }
  if (zx::result<> status = CheckMac(key, std::span(&response, 1), kOperation, err);
      status.is_error()) {
    return status;
  }
  const uint16_t result = be16toh(response.result);
  const bool succeeded = RpmbOperationResult(result) == static_cast<uint16_t>(RpmbResult::kOk);
  const uint16_t result_address = be16toh(response.address);
  const uint32_t result_counter = be32toh(response.write_counter);
  const uint32_t expected_counter = succeeded ? write_counter + 1 : write_counter;
  if (result_address != address || result_counter != expected_counter) {
    err << std::format(
        "error: could not confirm the RPMB write to address 0x{:04X}: the result ({}) is for "
        "address 0x{:04X} with write counter {} (sent {})\n",
        address, DescribeResult(result), result_address, result_counter, write_counter);
    return zx::error(ZX_ERR_IO_DATA_INTEGRITY);
  }
  if (!succeeded) {
    err << std::format("error: RPMB {} failed: {}\n", kOperation, DescribeResult(result));
    return zx::error(ZX_ERR_IO);
  }
  return zx::ok();
}

zx::result<fuchsia_hardware_rpmb::wire::EmmcDeviceInfo> GetEmmcInfo(const RpmbClient& client,
                                                                    std::ostream& err) {
  const fidl::WireResult result = client->GetDeviceInfo();
  if (!result.ok()) {
    err << std::format("error: failed to get RPMB device info: {}\n", result.status_string());
    return zx::error(result.status());
  }
  // The UFS drivers report the RPMB unit in the eMMC variant.
  const fuchsia_hardware_rpmb::wire::DeviceInfo& info = result.value().info;
  if (!info.is_emmc_info()) {
    err << "error: unsupported RPMB device info\n";
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  return zx::ok(info.emmc_info());
}

// Returns the number of frames to request per authenticated data read.
uint32_t FramesPerRead(const fuchsia_hardware_rpmb::wire::EmmcDeviceInfo& info) {
  return std::clamp<uint32_t>(info.reliable_write_sector_count, 1, kMaxFramesPerRequest);
}

// Returns the number of frames to send per authenticated data write.
uint32_t FramesPerWrite(const fuchsia_hardware_rpmb::wire::EmmcDeviceInfo& info) {
  return std::clamp<uint32_t>(info.reliable_write_sector_count, 1, kMaxFramesPerWrite);
}

// Checks the MAC of the response if `key` is set.
zx::result<RpmbFrame> ReadWriteCounter(const RpmbClient& client, const RpmbKey* key,
                                       std::ostream& err) {
  constexpr std::string_view kOperation = "write counter read";
  const RpmbFrame request = NewRequest(kRpmbWriteCounterReadRequest);
  zx::result<std::vector<RpmbFrame>> response = Transact(client, std::span(&request, 1), 1, err);
  if (response.is_error()) {
    return response.take_error();
  }
  if (zx::result<> status =
          CheckResponse(request, response->front(), kRpmbWriteCounterReadResponse, kOperation, err);
      status.is_error()) {
    return status.take_error();
  }
  if (key != nullptr) {
    if (zx::result<> status = CheckMac(*key, *response, kOperation, err); status.is_error()) {
      return status.take_error();
    }
  }
  return zx::ok(response->front());
}

// Checks the MAC of each response if `key` is set.
zx::result<std::vector<uint8_t>> ReadData(const RpmbClient& client, const RpmbKey* key,
                                          uint16_t address, uint32_t block_count,
                                          uint32_t frames_per_read, std::ostream& err) {
  std::vector<uint8_t> data;
  data.reserve(block_count * sizeof(RpmbFrame::data));
  for (uint32_t done = 0; done < block_count;) {
    const uint32_t count = std::min(block_count - done, frames_per_read);
    RpmbFrame request = NewRequest(kRpmbAuthenticatedDataReadRequest);
    request.address = htobe16(static_cast<uint16_t>(address + done));
    request.block_count = htobe16(static_cast<uint16_t>(count));

    zx::result<std::vector<RpmbFrame>> response =
        Transact(client, std::span(&request, 1), count, err);
    if (response.is_error()) {
      return response.take_error();
    }
    if (zx::result<> status = CheckReadResponse(request, *response, err); status.is_error()) {
      return status.take_error();
    }
    if (key != nullptr) {
      if (zx::result<> status = CheckMac(*key, *response, "authenticated data read", err);
          status.is_error()) {
        return status.take_error();
      }
    }
    for (const RpmbFrame& frame : *response) {
      data.insert(data.end(), std::begin(frame.data), std::end(frame.data));
    }
    done += count;
  }
  return zx::ok(std::move(data));
}

// Writes `data` to the blocks from `address`. Each write sends at most `frames_per_write` frames
// with the write counter read just before it, and stops at the first failure.
zx::result<> WriteData(const RpmbClient& client, const RpmbKey& key, uint16_t address,
                       std::span<const uint8_t> data, uint32_t frames_per_write,
                       std::ostream& err) {
  const uint32_t block_count = static_cast<uint32_t>(data.size() / kBlockSize);
  for (uint32_t done = 0; done < block_count;) {
    const uint32_t count = std::min(block_count - done, frames_per_write);
    const uint16_t block_address = static_cast<uint16_t>(address + done);

    // RPMB writes cannot be undone, so tell the user how far the write got.
    const auto report_progress = [&](bool chunk_unknown) {
      err << std::format("note: {} of {} blocks were written before the failure\n", done,
                         block_count);
      if (chunk_unknown) {
        err << std::format("note: blocks 0x{:04X}..0x{:04X} may or may not have been written\n",
                           block_address, block_address + count - 1);
      }
    };

    zx::result<RpmbFrame> counter = ReadWriteCounter(client, &key, err);
    if (counter.is_error()) {
      report_progress(false);
      return counter.take_error();
    }
    if (be16toh(counter->result) & kRpmbResultWriteCounterExpired) {
      err << "error: the RPMB write counter has expired; the device rejects authenticated writes\n";
      report_progress(false);
      return zx::error(ZX_ERR_BAD_STATE);
    }
    const uint32_t write_counter = be32toh(counter->write_counter);

    std::vector<RpmbFrame> request(count);
    for (uint32_t i = 0; i < count; ++i) {
      RpmbFrame& frame = request[i];
      frame.req_resp = htobe16(kRpmbAuthenticatedDataWriteRequest);
      frame.address = htobe16(block_address);
      frame.block_count = htobe16(static_cast<uint16_t>(count));
      frame.write_counter = htobe32(write_counter);
      std::ranges::copy(data.subspan((done + i) * kBlockSize, kBlockSize), frame.data);
    }
    std::ranges::copy(key.Mac(request), request.back().key_mac);

    // The write and the result read go in separate requests. A driver could otherwise send a
    // one-frame write and the result read in one SECURITY PROTOCOL OUT command.
    if (zx::result<std::vector<RpmbFrame>> status = Transact(client, request, 0, err);
        status.is_error()) {
      report_progress(true);
      return status.take_error();
    }
    RpmbFrame result_read{};
    result_read.req_resp = htobe16(kRpmbResultReadRequest);
    zx::result<std::vector<RpmbFrame>> response =
        Transact(client, std::span(&result_read, 1), 1, err);
    if (response.is_error()) {
      report_progress(true);
      return response.take_error();
    }
    if (zx::result<> status =
            CheckWriteResult(key, response->front(), block_address, write_counter, err);
        status.is_error()) {
      // ZX_ERR_IO means the result shows that the device did not write these blocks.
      report_progress(status.status_value() != ZX_ERR_IO);
      return status;
    }
    done += count;
  }
  return zx::ok();
}

// Checks that `count` blocks from `start` lie within the RPMB address range.
zx::result<> CheckBlockRange(uint32_t start, uint64_t count, std::ostream& err) {
  if (start >= kRpmbAddressCount || count > kRpmbAddressCount - start) {
    err << std::format("error: blocks {}..{} are outside the RPMB address range\n", start,
                       uint64_t{start} + count - 1);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  return zx::ok();
}

// Checks that `count` blocks from `start` lie within the RPMB unit that `info` describes.
//
// `EmmcDeviceInfo.rpmb_size` is the size of the whole RPMB unit, which the drivers derive from the
// RPMB Unit Descriptor's logical block count and size rather than from `bRPMBRegion0Size`. Region 0
// spans the whole unit in the shipping configuration, so the check matches region 0 there; if other
// regions are enabled it is a permissive bound. When the drivers cannot read the descriptor they
// report 512 KiB, so the check can also reject a write that the device would accept. Rejecting
// early is still preferable for a write, because a write that has already committed earlier chunks
// cannot be undone.
zx::result<> CheckDeviceRange(uint32_t start, uint64_t count,
                              const fuchsia_hardware_rpmb::wire::EmmcDeviceInfo& info,
                              std::ostream& err) {
  if (info.rpmb_size == 0) {
    return zx::ok();
  }
  const uint64_t block_capacity = uint64_t{info.rpmb_size} * kRpmbSizeUnit / kBlockSize;
  if (uint64_t{start} + count > block_capacity) {
    err << std::format("error: blocks {}..{} are outside the {} blocks that the device reports\n",
                       start, uint64_t{start} + count - 1, block_capacity);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  return zx::ok();
}

std::string_view MacStatus(bool verified) {
  return verified ? "MAC verified\n" : "MAC not verified\n";
}

zx::result<> HandleRpmbInfo(const RpmbClient& client, const RpmbArgs& args, std::ostream& out,
                            std::ostream& err) {
  zx::result<fuchsia_hardware_rpmb::wire::EmmcDeviceInfo> info = GetEmmcInfo(client, err);
  if (info.is_error()) {
    return info.take_error();
  }
  out << std::format("RPMB size                  : {} KiB\n",
                     info->rpmb_size * kRpmbSizeUnit / 1024);
  out << std::format("Reliable write sector count: {}\n", info->reliable_write_sector_count);
  out << std::format("Frames per read request    : {}\n", FramesPerRead(*info));
  out << std::format("Frames per write request   : {}\n", FramesPerWrite(*info));
  out << "CID                        : ";
  if (std::ranges::all_of(info->cid, [](uint8_t byte) { return byte == 0; })) {
    out << "not reported";
  } else {
    for (uint8_t byte : info->cid) {
      out << std::format("{:02X}", byte);
    }
  }
  out << "\n";
  return zx::ok();
}

zx::result<> HandleRpmbCounter(const RpmbClient& client, const RpmbArgs& args, std::ostream& out,
                               std::ostream& err) {
  const std::optional<RpmbKey> key = args.key ? RpmbKey::Load(*args.key, err) : std::nullopt;
  if (args.key.has_value() && !key.has_value()) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  zx::result<RpmbFrame> response = ReadWriteCounter(client, key ? &*key : nullptr, err);
  if (response.is_error()) {
    return response.take_error();
  }
  out << std::format("Write counter: {}\n", be32toh(response->write_counter));
  if (be16toh(response->result) & kRpmbResultWriteCounterExpired) {
    out << "The write counter has expired; the device rejects further authenticated writes.\n";
  }
  out << MacStatus(key.has_value());
  return zx::ok();
}

zx::result<> HandleRpmbRead(const RpmbClient& client, const RpmbArgs& args, std::ostream& out,
                            std::ostream& err) {
  if (!args.file.has_value()) {
    err << "error: missing required option -f/--file\n";
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  if (args.count == 0) {
    err << "error: --count must be at least 1\n";
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  const uint32_t start = args.start.value_or(0);
  if (zx::result<> status = CheckBlockRange(start, args.count, err); status.is_error()) {
    return status;
  }
  // Fail before reading anything from the device.
  const std::string dir = files::GetDirectoryName(files::AbsolutePath(*args.file));
  if (!files::IsDirectory(dir)) {
    err << std::format("error: output directory {} does not exist\n", dir);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  const std::optional<RpmbKey> key = args.key ? RpmbKey::Load(*args.key, err) : std::nullopt;
  if (args.key.has_value() && !key.has_value()) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  zx::result<fuchsia_hardware_rpmb::wire::EmmcDeviceInfo> info = GetEmmcInfo(client, err);
  if (info.is_error()) {
    return info.take_error();
  }
  if (zx::result<> status = CheckDeviceRange(start, args.count, *info, err); status.is_error()) {
    return status;
  }
  zx::result<std::vector<uint8_t>> data =
      ReadData(client, key ? &*key : nullptr, static_cast<uint16_t>(start), args.count,
               FramesPerRead(*info), err);
  if (data.is_error()) {
    return data.take_error();
  }
  if (!files::WriteFile(*args.file, reinterpret_cast<const char*>(data->data()),
                        static_cast<ssize_t>(data->size()))) {
    err << std::format("error: failed to write {}\n", *args.file);
    return zx::error(ZX_ERR_IO);
  }
  out << std::format("Read {} blocks ({} bytes) from address 0x{:04X} to {}\n", args.count,
                     data->size(), start, *args.file);
  out << MacStatus(key.has_value());
  return zx::ok();
}

zx::result<> HandleRpmbWrite(const RpmbClient& client, const RpmbArgs& args, std::ostream& out,
                             std::ostream& err) {
  if (!args.file.has_value()) {
    err << "error: missing required option -f/--file\n";
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  if (!args.key.has_value()) {
    err << "error: missing required option -k/--key\n";
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  // A write cannot be undone, so it has no default address.
  if (!args.start.has_value()) {
    err << "error: missing required option -s/--start\n";
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  const uint32_t start = *args.start;
  std::vector<uint8_t> data;
  if (!files::ReadFileToVector(*args.file, &data)) {
    err << std::format("error: failed to read {}\n", *args.file);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  if (data.empty() || data.size() % kBlockSize != 0) {
    err << std::format("error: {} holds {} bytes; the size must be a nonzero multiple of {}\n",
                       *args.file, data.size(), kBlockSize);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  const uint64_t block_count = data.size() / kBlockSize;
  if (zx::result<> status = CheckBlockRange(start, block_count, err); status.is_error()) {
    return status;
  }
  const std::optional<RpmbKey> key = RpmbKey::Load(*args.key, err);
  if (!key.has_value()) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  zx::result<fuchsia_hardware_rpmb::wire::EmmcDeviceInfo> info = GetEmmcInfo(client, err);
  if (info.is_error()) {
    return info.take_error();
  }
  if (zx::result<> status = CheckDeviceRange(start, block_count, *info, err); status.is_error()) {
    return status;
  }
  if (zx::result<> status =
          WriteData(client, *key, static_cast<uint16_t>(start), data, FramesPerWrite(*info), err);
      status.is_error()) {
    return status;
  }
  out << std::format("Wrote {} blocks ({} bytes) from {} to address 0x{:04X}\n", block_count,
                     data.size(), *args.file, start);
  return zx::ok();
}

constexpr RpmbCommand kRpmbCommands[] = {
    {
        .name = "info",
        .description = "Show the RPMB size and transfer limits reported by the driver.\n",
        .usage = "Usage: ufsutil <rpmb-device> rpmb info\n",
        .short_options = "h",
        .long_options = kHelpOnlyOptions,
        .handler = HandleRpmbInfo,
    },
    {
        .name = "counter",
        .description = "Read the RPMB write counter.\n",
        .usage = "Usage: ufsutil <rpmb-device> rpmb counter [options]\n"
                 "options:\n"
                 "  [--key=<FILE_PATH>, -k <FILE_PATH>]    32-byte key to verify the MAC with\n",
        .short_options = "hk:",
        .long_options = kCounterOptions,
        .handler = HandleRpmbCounter,
    },
    {
        .name = "read",
        .description = "Read authenticated data blocks of RPMB region 0 into a file.\n",
        .usage = "Usage: ufsutil <rpmb-device> rpmb read [options]\n"
                 "options:\n"
                 "  [--start=<ADDR>, -s <ADDR>]            First 256-byte block (default = 0)\n"
                 "  [--count=<NUM>, -n <NUM>]              Number of blocks (default = 1)\n"
                 "  [--file=<FILE_PATH>, -f <FILE_PATH>]   Output file (required)\n"
                 "  [--key=<FILE_PATH>, -k <FILE_PATH>]    32-byte key to verify the MAC with\n",
        .short_options = "hs:n:f:k:",
        .long_options = kReadOptions,
        .handler = HandleRpmbRead,
    },
    {
        .name = "write",
        .description =
            "Write a file permanently to authenticated data blocks of RPMB region 0, which is "
            "shared with the TEE.\n",
        .usage = "Usage: ufsutil <rpmb-device> rpmb write [options]\n"
                 "options:\n"
                 "  [--start=<ADDR>, -s <ADDR>]            First 256-byte block (required)\n"
                 "  [--file=<FILE_PATH>, -f <FILE_PATH>]   Input file, a multiple of 256 bytes "
                 "(required)\n"
                 "  [--key=<FILE_PATH>, -k <FILE_PATH>]    32-byte authentication key (required)\n"
                 "WARNING: The write is permanent. RPMB region 0 is shared with the TEE secure\n"
                 "         storage client, so overwriting its blocks corrupts the data that "
                 "the\n"
                 "         client stores.\n",
        .short_options = "hs:f:k:",
        .long_options = kWriteOptions,
        .handler = HandleRpmbWrite,
    },
};

const RpmbCommand* FindRpmbCommand(std::string_view name) {
  auto it = std::ranges::find(kRpmbCommands, name, &RpmbCommand::name);
  return it == std::end(kRpmbCommands) ? nullptr : &*it;
}

// Returns the parsed arguments, or `std::nullopt` if only the help message was requested.
zx::result<std::optional<RpmbArgs>> ParseRpmbArgs(const RpmbCommand& command, int argc, char** argv,
                                                  std::ostream& out, std::ostream& err) {
  RpmbArgs args;
  // Parse the arguments of the subcommand as a command line of their own. getopt_long resets its
  // short-option cursor only when optind is 0 (zircon musl getopt.c), so assigning an absolute
  // index would inherit a cursor that an earlier call in this process left mid-cluster.
  const int sub_argc = argc - 3;
  char** const sub_argv = argv + 3;
  opterr = 0;
  optind = 0;
  int opt;
  while ((opt = getopt_long(sub_argc, sub_argv, command.short_options, command.long_options,
                            nullptr)) != -1) {
    switch (opt) {
      case 'h':
        out << command.description << "\n" << command.usage;
        return zx::ok(std::nullopt);
      case 's':
      case 'n': {
        std::optional<uint32_t> value = ParseStrToUint32(optarg);
        if (!value) {
          err << std::format("error: invalid argument for -{:c}\n", static_cast<char>(opt));
          return zx::error(ZX_ERR_INVALID_ARGS);
        }
        if (opt == 's') {
          args.start = *value;
        } else {
          args.count = *value;
        }
        break;
      }
      case 'f':
        args.file = optarg;
        break;
      case 'k':
        args.key = optarg;
        break;
      default:
        err << std::format("error: invalid or incomplete option for '{}'\n", command.name);
        out << command.description << "\n" << command.usage;
        return zx::error(ZX_ERR_INVALID_ARGS);
    }
  }
  // getopt_long permutes the arguments, so options can leave the non-option arguments that came
  // before them behind in the slice. RPMB commands take no positional arguments.
  if (optind < sub_argc) {
    err << std::format("error: unexpected argument '{}'\n", sub_argv[optind]);
    out << command.description << "\n" << command.usage;
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  return zx::ok(std::move(args));
}

}  // namespace

bool IsRpmbCommand(std::string_view command) { return command == kRpmbGroup; }

void PrintRpmbUsage(std::ostream& out) {
  out << std::format("\nRPMB commands (<device> is an RPMB service endpoint, e.g., {}):\n",
                     kRpmbServicePath);
  for (const RpmbCommand& command : kRpmbCommands) {
    const std::string usage = std::format("ufsutil <rpmb-device> rpmb {}", command.name);
    out << std::format("  {:<40}  {}", usage, command.description);
  }
}

zx::result<> RunRpmb(fidl::WireSyncClient<fuchsia_hardware_rpmb::Rpmb> client, int argc,
                     char** argv, std::ostream& out, std::ostream& err) {
  const RpmbCommand* command = nullptr;
  if (argc >= 4 && std::string_view(argv[2]) == kRpmbGroup) {
    command = FindRpmbCommand(argv[3]);
  }
  if (command == nullptr) {
    err << "error: missing or unknown RPMB command\n";
    PrintRpmbUsage(out);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  zx::result<std::optional<RpmbArgs>> args = ParseRpmbArgs(*command, argc, argv, out, err);
  if (args.is_error()) {
    return args.take_error();
  }
  const std::optional<RpmbArgs>& parsed = args.value();
  if (!parsed.has_value()) {
    return zx::ok();
  }

  zx::result<> status = command->handler(client, *parsed, out, err);
  if (status.status_value() == ZX_ERR_PEER_CLOSED) {
    err << std::format("hint: RPMB commands take an RPMB service endpoint, e.g., {}\n",
                       kRpmbServicePath);
  }
  return status;
}

zx::result<fidl::WireSyncClient<fuchsia_hardware_rpmb::Rpmb>> OpenRpmbDevice(const char* path) {
  zx::result result = component::Connect<fuchsia_hardware_rpmb::Rpmb>(path);
  if (result.is_error()) {
    return result.take_error();
  }
  return zx::ok(fidl::WireSyncClient(std::move(result.value())));
}

}  // namespace ufsutil
