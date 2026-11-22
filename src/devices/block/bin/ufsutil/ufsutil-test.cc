// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "ufsutil.h"

#include <endian.h>
#include <fidl/fuchsia.hardware.ufs/cpp/wire_test_base.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>

#include <algorithm>
#include <cstdint>
#include <cstring>
#include <format>
#include <initializer_list>
#include <map>
#include <optional>
#include <sstream>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include <safemath/safe_conversions.h>
#include <zxtest/zxtest.h>

#include "common.h"
#include "error-history.h"
#include "src/lib/files/file.h"
#include "src/lib/files/path.h"
#include "src/lib/files/scoped_temp_dir.h"

namespace ufsutil {
namespace {

// Returns the trimmed `[Value]` column from the first descriptor line matching `name`.
std::optional<std::string> FieldValue(std::string_view output, std::string_view name) {
  size_t pos = 0;
  while (pos < output.size()) {
    size_t end = output.find('\n', pos);
    if (end == std::string_view::npos) {
      end = output.size();
    }
    std::string_view line = output.substr(pos, end - pos);
    if (line.starts_with(name) && line.size() > name.size() && line[name.size()] == ' ') {
      size_t bar = line.rfind('|');
      if (bar == std::string_view::npos) {
        return std::nullopt;
      }
      std::string_view value = line.substr(bar + 1);
      size_t first = value.find_first_not_of(' ');
      if (first == std::string_view::npos) {
        return std::string();
      }
      size_t last = value.find_last_not_of(' ');
      return std::string(value.substr(first, last - first + 1));
    }
    pos = end + 1;
  }
  return std::nullopt;
}

// Returns the number of descriptor lines matching `name`.
size_t CountFields(std::string_view output, std::string_view name) {
  size_t count = 0;
  size_t pos = 0;
  while (pos < output.size()) {
    size_t end = output.find('\n', pos);
    if (end == std::string_view::npos) {
      end = output.size();
    }
    std::string_view line = output.substr(pos, end - pos);
    if (line.starts_with(name) && line.size() > name.size() && line[name.size()] == ' ') {
      ++count;
    }
    pos = end + 1;
  }
  return count;
}

// Fills `buf` with a position-dependent byte pattern.
void FillTestData(std::vector<uint8_t>& buf) {
  for (size_t i = 0; i < buf.size(); ++i) {
    buf[i] = static_cast<uint8_t>(((i * 31) + 7) & 0xFFu);
  }
}

// File name that error-history uses for the entry with `buffer_id`.
std::string EntryFileName(uint8_t buffer_id) {
  return std::format("ufsutil_error_entry_{:02X}.bin", buffer_id);
}

}  // namespace

using UfsClient = fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>;

class UfsUtilTest : public zxtest::Test,
                    public fidl::testing::WireTestBase<fuchsia_hardware_ufs::Ufs> {
 public:
  UfsUtilTest() : loop_(&kAsyncLoopConfigAttachToCurrentThread) {
    ufsutil::Initialize();
    data_.resize(fuchsia_hardware_ufs::wire::kMaxDescriptorSize, 0);
    data_[0] = static_cast<uint8_t>(fuchsia_hardware_ufs::wire::kMaxDescriptorSize);

    auto endpoints = fidl::Endpoints<fuchsia_hardware_ufs::Ufs>::Create();
    client_ = std::move(endpoints.client);
    fidl::BindServer(loop_.dispatcher(), std::move(endpoints.server), this);
    loop_.StartThread("ufs-util-test-loop");
  }

  ~UfsUtilTest() override { loop_.Shutdown(); }

  void ReadDescriptor(fuchsia_hardware_ufs::wire::UfsReadDescriptorRequest* request,
                      ReadDescriptorCompleter::Sync& completer) override {
    auto it = descriptors_.find(static_cast<uint8_t>(request->descriptor.type()));
    if (it != descriptors_.end()) {
      completer.ReplySuccess(fidl::VectorView<uint8_t>::FromExternal(it->second));
      return;
    }
    completer.ReplySuccess(fidl::VectorView<uint8_t>::FromExternal(data_));
  }

  void WriteDescriptor(fuchsia_hardware_ufs::wire::UfsWriteDescriptorRequest* request,
                       WriteDescriptorCompleter::Sync& completer) override {
    std::ranges::copy(request->data, data_.begin());
    completer.ReplySuccess();
  }

  void ReadFlag(fuchsia_hardware_ufs::wire::UfsReadFlagRequest* request,
                ReadFlagCompleter::Sync& completer) override {
    if (query_error_.has_value()) {
      completer.ReplyError(*query_error_);
      return;
    }
    completer.ReplySuccess(flag_value_);
  }

  void SetFlag(fuchsia_hardware_ufs::wire::UfsSetFlagRequest* request,
               SetFlagCompleter::Sync& completer) override {
    flag_value_ = 1;
    completer.ReplySuccess(flag_value_);
  }

  void ClearFlag(::fuchsia_hardware_ufs::wire::UfsClearFlagRequest* request,
                 ClearFlagCompleter::Sync& completer) override {
    flag_value_ = 0;
    completer.ReplySuccess(flag_value_);
  }

  void ToggleFlag(::fuchsia_hardware_ufs::wire::UfsToggleFlagRequest* request,
                  ToggleFlagCompleter::Sync& completer) override {
    flag_value_ ^= 1u;
    completer.ReplySuccess(flag_value_);
  }

  void ReadAttribute(::fuchsia_hardware_ufs::wire::UfsReadAttributeRequest* request,
                     ReadAttributeCompleter::Sync& completer) override {
    completer.ReplySuccess(attr_value_);
  }

  void WriteAttribute(::fuchsia_hardware_ufs::wire::UfsWriteAttributeRequest* request,
                      WriteAttributeCompleter::Sync& completer) override {
    attr_value_ = request->value;
    completer.ReplySuccess();
  }

  void ReadBuffer(::fuchsia_hardware_scsi::wire::ScsiReadBufferRequest* request,
                  ReadBufferCompleter::Sync& completer) override {
    ++read_buffer_count_;
    if (request->mode != fuchsia_hardware_scsi::wire::ReadBufferMode::kErrorHistory) {
      completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
      return;
    }
    // The driver rejects lengths that are not a multiple of 4.
    if (request->length % 4 != 0) {
      completer.ReplyError(ZX_ERR_INVALID_ARGS);
      return;
    }
    if (read_buffer_error_ != ZX_OK) {
      completer.ReplyError(read_buffer_error_);
      return;
    }

    if (request->buffer_id == 0) {
      auto checked_size = safemath::checked_cast<uint16_t>(error_history_entries_.size() *
                                                           sizeof(ErrorDirectoryEntry));
      uint16_t checked_length = forced_directory_length_.value_or(checked_size);
      std::vector<uint8_t> buffer(std::max(checked_length, checked_size) +
                                  sizeof(ErrorDirectoryHeader));
      ErrorDirectoryHeader* header = reinterpret_cast<ErrorDirectoryHeader*>(buffer.data());
      header->directory_length = htobe16(checked_length);
      std::memcpy(header->vendor_id.data(), "VENDOR01", 8);
      header->version = 1;

      ErrorDirectoryEntry* raw_entries =
          reinterpret_cast<ErrorDirectoryEntry*>(buffer.data() + sizeof(ErrorDirectoryHeader));
      for (size_t i = 0; i < error_history_entries_.size(); ++i) {
        raw_entries[i].buffer_id = error_history_entries_[i].buffer_id;
        raw_entries[i].reserved = error_history_entries_[i].reserved;
        raw_entries[i].length = htobe32(error_history_entries_[i].length);
      }

      if (request->data.write(buffer.data(), 0, buffer.size()) != ZX_OK) {
        completer.ReplyError(ZX_ERR_IO);
        return;
      }
      completer.ReplySuccess();
      return;
    }

    for (const auto& entry : error_history_entries_) {
      if (request->buffer_id == entry.buffer_id) {
        ++entry_read_count_;
        // The driver rejects transfers above kMaxTransferSize.
        if (request->length > kMaxTransferSize ||
            request->buffer_offset >= error_history_data_.size()) {
          completer.ReplyError(ZX_ERR_OUT_OF_RANGE);
          return;
        }
        entry_read_requests_.emplace_back(request->buffer_offset, request->length);
        // A read past the end underflows and returns only the available bytes.
        const size_t available =
            std::min<size_t>(request->length, error_history_data_.size() - request->buffer_offset);
        request->data.write(error_history_data_.data() + request->buffer_offset, 0, available);
        completer.ReplySuccess();
        return;
      }
    }

    completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
  }

  void SendUicCommand(::fuchsia_hardware_ufs::wire::UicCommand* request,
                      SendUicCommandCompleter::Sync& completer) override {
    last_uic_command_ = *request;
    ++uic_command_count_;
    if (uic_error_ != ZX_OK) {
      completer.ReplyError(uic_error_);
      return;
    }
    completer.ReplySuccess(uic_result_);
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override { FAIL(); }

  // Configures per-type descriptor bytes returned by `ReadDescriptor`. Types not configured here
  // fall back to `data_`.
  void SetDescriptor(fuchsia_hardware_ufs::wire::DescriptorType type, std::vector<uint8_t> data) {
    descriptors_[static_cast<uint8_t>(type)] = std::move(data);
  }

  // Adds an entry to the error history directory returned by `ReadBuffer`.
  void InjectErrorHistoryEntry(uint8_t buffer_id, uint32_t length) {
    error_history_entries_.push_back({.buffer_id = buffer_id, .reserved = {}, .length = length});
  }

  // Invokes `RunUfsUtils` with `args` and returns the captured standard output on success or the
  // error status on failure.
  zx::result<std::string> RunCommand(std::initializer_list<const char*> args) {
    std::ostringstream out;
    std::ostringstream err;
    std::vector<char*> argv;
    argv.reserve(args.size() + 1);
    for (const char* arg : args) {
      argv.push_back(const_cast<char*>(arg));
    }
    argv.push_back(nullptr);
    if (!client_.is_valid()) {
      auto endpoints = fidl::Endpoints<fuchsia_hardware_ufs::Ufs>::Create();
      client_ = std::move(endpoints.client);
      fidl::BindServer(loop_.dispatcher(), std::move(endpoints.server), this);
    }
    if (zx::result<> status = RunUfsUtils(UfsClient(std::move(client_)),
                                          static_cast<int>(args.size()), argv.data(), out, err);
        status.is_error()) {
      return status.take_error();
    }
    return zx::ok(out.str());
  }

  fidl::ClientEnd<fuchsia_hardware_ufs::Ufs> client_;
  std::vector<uint8_t> data_;
  std::map<uint8_t, std::vector<uint8_t>> descriptors_;
  std::optional<fuchsia_hardware_ufs::wire::QueryErrorCode> query_error_;
  uint8_t flag_value_ = 0;
  uint32_t attr_value_ = 0;
  uint32_t uic_result_ = 0;
  zx_status_t uic_error_ = ZX_OK;
  fuchsia_hardware_ufs::wire::UicCommand last_uic_command_{};
  size_t uic_command_count_ = 0;
  std::vector<ErrorDirectoryEntry> error_history_entries_;
  // Overrides the directory length reported in the error history directory header.
  std::optional<uint16_t> forced_directory_length_;
  // Contents of every error history entry buffer.
  std::vector<uint8_t> error_history_data_;
  zx_status_t read_buffer_error_ = ZX_OK;
  // Number of ReadBuffer requests, including the directory.
  size_t read_buffer_count_ = 0;
  // Number of ReadBuffer requests for error history entries (not the directory).
  size_t entry_read_count_ = 0;
  // (buffer_offset, length) of every ReadBuffer request for an error history entry.
  std::vector<std::pair<uint32_t, uint32_t>> entry_read_requests_;
  files::ScopedTempDir temp_dir_;
  async::Loop loop_;
};

TEST_F(UfsUtilTest, MissingCommand) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>"}), ZX_ERR_INVALID_ARGS);
}

TEST_F(UfsUtilTest, UnknownCommand) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "unknown"}), ZX_ERR_INVALID_ARGS);
}

TEST_F(UfsUtilTest, CommandHelp) {
  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "read-desc", "--help"});
  ASSERT_OK(output);
  EXPECT_NE(std::string::npos, output->find("Usage: ufsutil <device> read-desc"));
}

TEST_F(UfsUtilTest, ReadDescriptor) {
  std::vector<uint8_t> device_desc(fuchsia_hardware_ufs::wire::kMaxDescriptorSize, 0);
  device_desc[0x00] = 0x59;  // bLength
  device_desc[0x01] = 0x00;  // bDescriptorIDN
  device_desc[0x10] = 0x03;  // wSpecVersion = 0x0310
  device_desc[0x11] = 0x10;
  SetDescriptor(fuchsia_hardware_ufs::wire::DescriptorType::kDevice, std::move(device_desc));

  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "read-desc", "--type", "0"});
  ASSERT_OK(output);
  EXPECT_EQ(std::optional<std::string>("0x59"), FieldValue(*output, "bLength"));
  EXPECT_EQ(std::optional<std::string>("0x0310"), FieldValue(*output, "wSpecVersion"));
}

TEST_F(UfsUtilTest, ReadConfigDescriptor) {
  data_[0x00] = 0xe6;  // bLength
  data_[0x1a] = 0x16;  // bUD0BaseOffset
  data_[0x1b] = 0x1a;  // bUDConfigPLength

  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "read-desc", "--type", "1"});
  ASSERT_OK(output);
  EXPECT_EQ(std::optional<std::string>("0xE6"), FieldValue(*output, "bLength"));
}

TEST_F(UfsUtilTest, ReadUnitDescriptor) {
  data_[0x00] = 0x2d;  // bLength
  data_[0x01] = 0x02;  // bDescriptorIDN
  data_[0x03] = 0x01;  // bLUEnable
  data_[0x0a] = 0x0c;  // bLogicalBlockSize (4 KiB)

  zx::result<std::string> output =
      RunCommand({"ufsutil", "<device>", "read-desc", "--type", "2", "--index", "0"});
  ASSERT_OK(output);
  EXPECT_EQ(std::optional<std::string>("0x2D"), FieldValue(*output, "bLength"));
  EXPECT_EQ(std::optional<std::string>("0x01"), FieldValue(*output, "bLUEnable"));
  EXPECT_EQ(std::optional<std::string>("0x0C"), FieldValue(*output, "bLogicalBlockSize"));
}

TEST_F(UfsUtilTest, ReadInterconnectDescriptor) {
  data_[0x00] = 0x06;  // bLength
  data_[0x01] = 0x04;  // bDescriptorIDN
  data_[0x02] = 0x01;  // bcdUniproVersion = 0x0180
  data_[0x03] = 0x80;

  zx::result<std::string> output =
      RunCommand({"ufsutil", "<device>", "read-desc", "--type", "4", "-s", "0"});
  ASSERT_OK(output);
  EXPECT_EQ(std::optional<std::string>("0x0180"), FieldValue(*output, "bcdUniproVersion"));
}

TEST_F(UfsUtilTest, ReadGeometryDescriptor) {
  data_[0x00] = 0x57;  // bLength
  data_[0x01] = 0x07;  // bDescriptorIDN
  data_[0x0c] = 0x01;  // bMaxNumberLU (32 LUs)

  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "read-desc", "--type", "7"});
  ASSERT_OK(output);
  EXPECT_EQ(std::optional<std::string>("0x57"), FieldValue(*output, "bLength"));
  EXPECT_EQ(std::optional<std::string>("0x01"), FieldValue(*output, "bMaxNumberLU"));
}

TEST_F(UfsUtilTest, ReadPowerDescriptor) {
  data_[0x00] = 0x62;  // bLength
  data_[0x01] = 0x08;  // bDescriptorIDN
  data_[0x02] = 0x01;  // wActiveICCLevelsVCC[0] = 0x012C (300)
  data_[0x03] = 0x2c;

  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "read-desc", "--type", "8"});
  ASSERT_OK(output);
  EXPECT_EQ(std::optional<std::string>("0x012C (300)"),
            FieldValue(*output, "wActiveICCLevelsVCC[0]"));
}

TEST_F(UfsUtilTest, ReadStringDescriptor) {
  data_[0x00] = 0x08;  // bLength
  data_[0x01] = 0x05;  // bDescriptorIDN
  data_[0x02] = 0x00;
  data_[0x03] = 0x41;  // 'A'
  data_[0x04] = 0x00;
  data_[0x05] = 0x42;  // 'B'

  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "read-desc", "--type", "5"});
  ASSERT_OK(output);
  EXPECT_EQ(std::optional<std::string>("0x08"), FieldValue(*output, "bLength"));
  EXPECT_EQ(std::optional<std::string>("0x0041"), FieldValue(*output, "UC[0]"));
}

TEST_F(UfsUtilTest, ReadDeviceHealthDescriptor) {
  data_[0x00] = 0x2d;  // bLength
  data_[0x01] = 0x09;  // bDescriptorIDN
  data_[0x02] = 0x01;  // bPreEOLInfo (Normal)

  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "read-desc", "--type", "9"});
  ASSERT_OK(output);
  EXPECT_EQ(std::optional<std::string>("0x01"), FieldValue(*output, "bPreEOLInfo"));
}

TEST_F(UfsUtilTest, ReadDescriptorUnsupportedType) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "read-desc", "--type", "3"}),
                ZX_ERR_NOT_SUPPORTED);
}

TEST_F(UfsUtilTest, ReadDescriptorInvalidArgument) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "read-desc", "-t", "abc"}), ZX_ERR_INVALID_ARGS);
}

TEST_F(UfsUtilTest, ReadDescriptorMissingRequiredArgs) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "read-desc"}), ZX_ERR_INVALID_ARGS);
}

TEST_F(UfsUtilTest, WriteDescriptor) {
  zx::result<std::string> output =
      RunCommand({"ufsutil", "<device>", "write-desc", "-t", "5", "-v", "AB"});
  ASSERT_OK(output);
  EXPECT_TRUE(output->empty());
  EXPECT_EQ(0x06, data_[0]);
  EXPECT_EQ(static_cast<uint8_t>(fuchsia_hardware_ufs::wire::DescriptorType::kString), data_[1]);
  EXPECT_EQ(0x00, data_[2]);
  EXPECT_EQ('A', data_[3]);
  EXPECT_EQ(0x00, data_[4]);
  EXPECT_EQ('B', data_[5]);
}

TEST_F(UfsUtilTest, WriteDescriptorMissingOption) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "write-desc", "-t", "5"}), ZX_ERR_INVALID_ARGS);
}

TEST_F(UfsUtilTest, WriteDescriptorUnsupportedType) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "write-desc", "--type", "3"}),
                ZX_ERR_NOT_SUPPORTED);
}

TEST_F(UfsUtilTest, WriteDescriptorMissingRequiredArgs) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "write-desc", "-t"}), ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "write-desc", "--type"}), ZX_ERR_INVALID_ARGS);
}

TEST_F(UfsUtilTest, WriteDescriptorNotWritable) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "write-desc", "-t", "0"}), ZX_ERR_ACCESS_DENIED);
}

TEST_F(UfsUtilTest, SetFlag) {
  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "set-flag", "-t", "1"});
  ASSERT_OK(output);
  EXPECT_EQ("fDeviceInit : 1\n", *output);
}

TEST_F(UfsUtilTest, SetFlagReadOnly) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "set-flag", "-t", "9"}), ZX_ERR_ACCESS_DENIED);
}

TEST_F(UfsUtilTest, SetFlagUnsupportedType) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "set-flag", "-t", "99"}), ZX_ERR_NOT_SUPPORTED);
}

TEST_F(UfsUtilTest, ReadFlag) {
  flag_value_ = 1;
  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "read-flag", "-t", "1"});
  ASSERT_OK(output);
  EXPECT_EQ("fDeviceInit : 1\n", *output);
}

TEST_F(UfsUtilTest, ReadFlagWriteOnly) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "read-flag", "-t", "6"}), ZX_ERR_ACCESS_DENIED);
}

TEST_F(UfsUtilTest, ToggleFlag) {
  flag_value_ = 0;
  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "toggle-flag", "-t", "1"});
  ASSERT_OK(output);
  EXPECT_EQ("fDeviceInit : 1\n", *output);
}

TEST_F(UfsUtilTest, ClearFlag) {
  flag_value_ = 1;
  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "clear-flag", "-t", "1"});
  ASSERT_OK(output);
  EXPECT_EQ("fDeviceInit : 0\n", *output);
}

TEST_F(UfsUtilTest, ReadAttribute) {
  attr_value_ = 0x0102;
  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "read-attr", "-t", "3"});
  ASSERT_OK(output);
  EXPECT_EQ("value : 0x0102\n", *output);
}

TEST_F(UfsUtilTest, ReadAttributeWriteOnly) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "read-attr", "-t", "0xf"}),
                ZX_ERR_ACCESS_DENIED);
}

TEST_F(UfsUtilTest, ReadAttributeUnsupportedType) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "read-attr", "-t", "99"}), ZX_ERR_NOT_SUPPORTED);
}

TEST_F(UfsUtilTest, WriteAttribute) {
  zx::result<std::string> output =
      RunCommand({"ufsutil", "<device>", "write-attr", "-t", "0", "-v", "0x01"});
  ASSERT_OK(output);
  EXPECT_TRUE(output->empty());
  EXPECT_EQ(0x01u, attr_value_);
}

TEST_F(UfsUtilTest, WriteAttributeReadOnly) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "write-attr", "-t", "5", "-v", "0x01"}),
                ZX_ERR_ACCESS_DENIED);
}

TEST_F(UfsUtilTest, WriteAttributeMissingValue) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "write-attr", "-t", "0"}), ZX_ERR_INVALID_ARGS);
}

TEST_F(UfsUtilTest, QueryErrors) {
  using fuchsia_hardware_ufs::wire::QueryErrorCode;

  query_error_ = QueryErrorCode::kParameterNotReadable;
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "read-flag", "-t", "1"}), ZX_ERR_ACCESS_DENIED);

  query_error_ = QueryErrorCode::kInvalidIndex;
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "read-flag", "-t", "1"}), ZX_ERR_INVALID_ARGS);

  query_error_ = QueryErrorCode::kInvalidOpcode;
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "read-flag", "-t", "1"}), ZX_ERR_NOT_SUPPORTED);

  query_error_ = QueryErrorCode::kGeneralFailure;
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "read-flag", "-t", "1"}), ZX_ERR_IO);

  auto endpoints = fidl::Endpoints<fuchsia_hardware_ufs::Ufs>::Create();
  client_ = std::move(endpoints.client);
  endpoints.server.reset();
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "read-flag", "-t", "1"}), ZX_ERR_PEER_CLOSED);
}

TEST_F(UfsUtilTest, ReadConfigDescriptorPrintsConfigurationData) {
  using fuchsia_hardware_ufs::wire::DescriptorType;
  constexpr size_t kSize = fuchsia_hardware_ufs::wire::kMaxDescriptorSize;
  std::vector<uint8_t> device(kSize, 0);
  device[0x00] = 0x59;  // bLength
  device[0x01] = 0x00;  // bDescriptorIDN
  device[0x1a] = 0x16;  // bUD0BaseOffset
  device[0x1b] = 0x1a;  // bUDConfigPLength
  std::vector<uint8_t> config(kSize, 0);
  config[0x00] = 0xe6;  // bLength
  config[0x01] = 0x01;  // bDescriptorIDN
  SetDescriptor(DescriptorType::kDevice, std::move(device));
  SetDescriptor(DescriptorType::kConfiguration, std::move(config));

  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "read-desc", "--type", "1"});
  ASSERT_OK(output);
  EXPECT_EQ(std::optional<std::string>("0xE6"), FieldValue(*output, "bLength"));
  EXPECT_EQ(std::optional<std::string>("0x01"), FieldValue(*output, "bDescriptorIDN"));
}

TEST_F(UfsUtilTest, ReadStringDescriptorPrintsLastCharacter) {
  // bLength 0x0A: a 2-byte header followed by four UTF-16BE characters "0000".
  SetDescriptor(fuchsia_hardware_ufs::wire::DescriptorType::kString,
                {0x0a, 0x05, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30, 0x00, 0x30});

  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "read-desc", "--type", "5"});
  ASSERT_OK(output);
  EXPECT_EQ(std::optional<std::string>("0x0030"), FieldValue(*output, "UC[3]"));
  EXPECT_EQ(std::nullopt, FieldValue(*output, "UC[4]"));
}

TEST_F(UfsUtilTest, ReadStringDescriptorShorterThanHeader) {
  SetDescriptor(fuchsia_hardware_ufs::wire::DescriptorType::kString, {0x0a});

  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "read-desc", "--type", "5"});
  ASSERT_OK(output);
  EXPECT_EQ("", *output);
}

TEST_F(UfsUtilTest, ReadStringDescriptorLengthExceedsBuffer) {
  // bLength claims four characters, but the response carries only one.
  SetDescriptor(fuchsia_hardware_ufs::wire::DescriptorType::kString, {0x0a, 0x05, 0x00, 0x41});

  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "read-desc", "--type", "5"});
  ASSERT_OK(output);
  EXPECT_EQ(std::optional<std::string>("0x0041"), FieldValue(*output, "UC[0]"));
  EXPECT_EQ(std::nullopt, FieldValue(*output, "UC[1]"));
}

TEST_F(UfsUtilTest, ReadDescriptorShorterThanFields) {
  // The response carries only the 2-byte header of the device descriptor.
  SetDescriptor(fuchsia_hardware_ufs::wire::DescriptorType::kDevice, {0x59, 0x00});

  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "read-desc", "--type", "0"});
  ASSERT_OK(output);
  EXPECT_EQ(std::optional<std::string>("0x59"), FieldValue(*output, "bLength"));
  EXPECT_EQ(std::optional<std::string>("0x00"), FieldValue(*output, "bDescriptorIDN"));
  EXPECT_EQ(std::nullopt, FieldValue(*output, "bDevice"));
}

TEST_F(UfsUtilTest, ReadPowerDescriptorShorterThanFields) {
  // bLength claims the full descriptor, but the response carries only two VCC levels.
  SetDescriptor(fuchsia_hardware_ufs::wire::DescriptorType::kPower,
                {0x62, 0x08, 0x01, 0x2c, 0x00, 0x64});

  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "read-desc", "--type", "8"});
  ASSERT_OK(output);
  EXPECT_EQ(std::optional<std::string>("0x012C (300)"),
            FieldValue(*output, "wActiveICCLevelsVCC[0]"));
  EXPECT_EQ(std::optional<std::string>("0x0064 (100)"),
            FieldValue(*output, "wActiveICCLevelsVCC[1]"));
  EXPECT_EQ(std::nullopt, FieldValue(*output, "wActiveICCLevelsVCC[2]"));
  EXPECT_EQ(std::nullopt, FieldValue(*output, "wActiveICCLevelsVCCQ[0]"));
}

TEST_F(UfsUtilTest, ReadPowerDescriptorShorterThanHeader) {
  SetDescriptor(fuchsia_hardware_ufs::wire::DescriptorType::kPower, {0x62});

  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "read-desc", "--type", "8"});
  ASSERT_OK(output);
  EXPECT_EQ("", *output);
}

TEST_F(UfsUtilTest, ReadConfigDescriptorDeviceDescriptorTooShort) {
  // bUD0BaseOffset (0x1A) and bUDConfigPLength (0x1B) are missing from the response.
  SetDescriptor(fuchsia_hardware_ufs::wire::DescriptorType::kDevice, {0x59, 0x00});

  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "read-desc", "--type", "1"}), ZX_ERR_IO);
}

TEST_F(UfsUtilTest, ReadConfigDescriptorUnitsExceedBuffer) {
  using fuchsia_hardware_ufs::wire::DescriptorType;
  std::vector<uint8_t> device(fuchsia_hardware_ufs::wire::kMaxDescriptorSize, 0);
  device[0x1a] = 0x16;  // bUD0BaseOffset
  device[0x1b] = 0x1a;  // bUDConfigPLength
  // The header and two of the eight unit descriptor configurable parameters.
  std::vector<uint8_t> config(0x16 + (2 * 0x1a), 0);
  config[0x00] = 0xe6;  // bLength
  config[0x01] = 0x01;  // bDescriptorIDN
  SetDescriptor(DescriptorType::kDevice, std::move(device));
  SetDescriptor(DescriptorType::kConfiguration, std::move(config));

  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "read-desc", "--type", "1"});
  ASSERT_OK(output);
  EXPECT_EQ(std::optional<std::string>("0xE6"), FieldValue(*output, "bLength"));
  EXPECT_EQ(2u, CountFields(*output, "bLUEnable"));
}

TEST_F(UfsUtilTest, ReadFlagPrintsPermanentlyDisableFwUpdateName) {
  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "read-flag", "-t", "0x0b"});
  ASSERT_OK(output);
  EXPECT_EQ("fPermanentlyDisableFwUpdate : 0\n", *output);
}

TEST_F(UfsUtilTest, DmeGet) {
  // PA_MaxRxHSGear (0x1587) is defined in MIPI UniPro Specification v2.0
  uic_result_ = 0x4;
  zx::result<std::string> output = RunCommand({"ufsutil", "<device>", "dme-get", "-i", "0x1587"});
  ASSERT_OK(output);
  EXPECT_EQ("0x1587 : 0x4\n", *output);
  ASSERT_EQ(1u, uic_command_count_);
  EXPECT_EQ(fuchsia_hardware_ufs::wire::UicCommandOpcode::kDmeGet, last_uic_command_.opcode);
  EXPECT_EQ(0x15870000u, last_uic_command_.argument[0]);
  EXPECT_EQ(0u, last_uic_command_.argument[1]);
  EXPECT_EQ(0u, last_uic_command_.argument[2]);
}

TEST_F(UfsUtilTest, DmeGetSelectorLongOption) {
  // PA_MaxRxHSGear (0x1587) is defined in MIPI UniPro Specification v2.0
  ASSERT_OK(RunCommand({"ufsutil", "<device>", "dme-get", "--id", "0x1587", "--selector", "1"}));
  ASSERT_EQ(1u, uic_command_count_);
  EXPECT_EQ(fuchsia_hardware_ufs::wire::UicCommandOpcode::kDmeGet, last_uic_command_.opcode);
  EXPECT_EQ(0x15870001u, last_uic_command_.argument[0]);
}

TEST_F(UfsUtilTest, DmeGetMissingID) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "dme-get"}), ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(0u, uic_command_count_);
}

TEST_F(UfsUtilTest, DmeGetDriverError) {
  uic_error_ = ZX_ERR_NOT_SUPPORTED;
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "dme-get", "-i", "0x1587"}),
                ZX_ERR_NOT_SUPPORTED);
}

TEST_F(UfsUtilTest, DmePeerGet) {
  // PA_Peer_TX_LCC_Enable (0x155F) is defined in MIPI UniPro Specification v2.0
  ASSERT_OK(RunCommand({"ufsutil", "<device>", "dme-get", "-i", "0x155F", "--peer"}));
  ASSERT_EQ(1u, uic_command_count_);
  EXPECT_EQ(fuchsia_hardware_ufs::wire::UicCommandOpcode::kDmePeerGet, last_uic_command_.opcode);
  EXPECT_EQ(0x155F0000u, last_uic_command_.argument[0]);
}

TEST_F(UfsUtilTest, DmeSet) {
  // PA_TActivate (0x15A8) is defined in MIPI UniPro Specification v2.0. The driver only accepts
  // volatile DME_SET of allowlisted attributes such as this one.
  zx::result<std::string> output =
      RunCommand({"ufsutil", "<device>", "dme-set", "-i", "0x15A8", "-v", "0x3"});
  ASSERT_OK(output);
  EXPECT_TRUE(output->empty());
  ASSERT_EQ(1u, uic_command_count_);
  EXPECT_EQ(fuchsia_hardware_ufs::wire::UicCommandOpcode::kDmeSet, last_uic_command_.opcode);
  EXPECT_EQ(0x15A80000u, last_uic_command_.argument[0]);
  EXPECT_EQ(0u, last_uic_command_.argument[1]);  // AttrSetType: volatile
  EXPECT_EQ(0x3u, last_uic_command_.argument[2]);
}

TEST_F(UfsUtilTest, DmePeerSet) {
  // PA_TActivate (0x15A8) is defined in MIPI UniPro Specification v2.0
  ASSERT_OK(RunCommand({"ufsutil", "<device>", "dme-set", "-i", "0x15A8", "--peer", "-v", "0"}));
  ASSERT_EQ(1u, uic_command_count_);
  EXPECT_EQ(fuchsia_hardware_ufs::wire::UicCommandOpcode::kDmePeerSet, last_uic_command_.opcode);
  EXPECT_EQ(0x15A80000u, last_uic_command_.argument[0]);
  EXPECT_EQ(0u, last_uic_command_.argument[1]);  // AttrSetType: volatile
  EXPECT_EQ(0u, last_uic_command_.argument[2]);
}

TEST_F(UfsUtilTest, DmeSetDriverError) {
  uic_error_ = ZX_ERR_ACCESS_DENIED;
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "dme-set", "-i", "0x1587", "-v", "0"}),
                ZX_ERR_ACCESS_DENIED);
}

TEST_F(UfsUtilTest, DmeSetStaticOptionRejected) {
  // Static (non-volatile) DME_SET is not supported by the driver, so the option is not offered.
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "dme-set", "-i", "0x15A8", "-S", "-v", "0"}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(0u, uic_command_count_);
}

TEST_F(UfsUtilTest, DmeSetMissingValue) {
  // PA_MaxRxHSGear (0x1587) is defined in MIPI UniPro Specification v2.0
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "dme-set", "-i", "0x1587"}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(0u, uic_command_count_);
}

TEST_F(UfsUtilTest, DmeSetOutOfRangeAttrId) {
  // 65536 is out of range for uint16_t.
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "dme-set", "-i", "65536", "-v", "0"}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(0u, uic_command_count_);
}

TEST_F(UfsUtilTest, DmeSetOutOfRangeSelector) {
  // 65536 is out of range for uint16_t.
  EXPECT_STATUS(
      RunCommand({"ufsutil", "<device>", "dme-set", "-i", "0x1587", "-s", "65536", "-v", "0"}),
      ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(0u, uic_command_count_);
}

TEST_F(UfsUtilTest, ErrorHistory) {
  constexpr uint32_t kEntryBufferSize = 128;
  error_history_data_.resize(kEntryBufferSize);
  FillTestData(error_history_data_);
  InjectErrorHistoryEntry(0x10, kEntryBufferSize);
  InjectErrorHistoryEntry(0x11, kEntryBufferSize);

  zx::result<std::string> output =
      RunCommand({"ufsutil", "<device>", "error-history", "-o", temp_dir_.path().c_str()});
  ASSERT_OK(output);
  EXPECT_NE(std::string::npos, output->find("Vendor ID  : VENDOR01"));
  EXPECT_NE(std::string::npos, output->find("Entry #0 | Buffer ID: 0x10 | Length: 128 bytes"));
  EXPECT_NE(std::string::npos, output->find("Entry #1 | Buffer ID: 0x11 | Length: 128 bytes"));
  for (uint8_t buffer_id : {uint8_t{0x10}, uint8_t{0x11}}) {
    std::vector<uint8_t> written;
    ASSERT_TRUE(files::ReadFileToVector(files::JoinPath(temp_dir_.path(), EntryFileName(buffer_id)),
                                        &written));
    EXPECT_EQ(error_history_data_, written);
  }
}

TEST_F(UfsUtilTest, ErrorHistoryData) {
  // An entry larger than kMaxTransferSize is read in kMaxTransferSize chunks.
  constexpr uint8_t kBufferId = 0x10;
  constexpr uint32_t kEntryLength = kMaxTransferSize * 2;
  error_history_data_.resize(kEntryLength);
  FillTestData(error_history_data_);
  InjectErrorHistoryEntry(kBufferId, kEntryLength);

  ASSERT_OK(RunCommand({"ufsutil", "<device>", "error-history", "-o", temp_dir_.path().c_str()}));
  EXPECT_EQ(2u, entry_read_count_);
  std::vector<uint8_t> written;
  ASSERT_TRUE(files::ReadFileToVector(files::JoinPath(temp_dir_.path(), EntryFileName(kBufferId)),
                                      &written));
  EXPECT_EQ(error_history_data_, written);
}

TEST_F(UfsUtilTest, ErrorHistoryUnalignedEntryLength) {
  // An unaligned entry is read with the length rounded up to a multiple of 4.
  constexpr uint8_t kBufferId = 0x10;
  constexpr uint32_t kEntryLength = 130;
  error_history_data_.resize(kEntryLength);
  FillTestData(error_history_data_);
  InjectErrorHistoryEntry(kBufferId, kEntryLength);

  ASSERT_OK(RunCommand({"ufsutil", "<device>", "error-history", "-o", temp_dir_.path().c_str()}));

  ASSERT_EQ(1u, entry_read_requests_.size());
  EXPECT_EQ(0u, entry_read_requests_[0].first);
  EXPECT_EQ(132u, entry_read_requests_[0].second);

  std::vector<uint8_t> written;
  ASSERT_TRUE(files::ReadFileToVector(files::JoinPath(temp_dir_.path(), EntryFileName(kBufferId)),
                                      &written));
  EXPECT_EQ(error_history_data_, written);
}

TEST_F(UfsUtilTest, ErrorHistoryUnalignedEntryLengthMultipleChunks) {
  // Only the last chunk is rounded up.
  constexpr uint8_t kBufferId = 0x10;
  constexpr uint32_t kEntryLength = kMaxTransferSize + 130;
  error_history_data_.resize(kEntryLength);
  FillTestData(error_history_data_);
  InjectErrorHistoryEntry(kBufferId, kEntryLength);

  ASSERT_OK(RunCommand({"ufsutil", "<device>", "error-history", "-o", temp_dir_.path().c_str()}));

  ASSERT_EQ(2u, entry_read_requests_.size());
  EXPECT_EQ(0u, entry_read_requests_[0].first);
  EXPECT_EQ(kMaxTransferSize, entry_read_requests_[0].second);
  EXPECT_EQ(kMaxTransferSize, entry_read_requests_[1].first);
  EXPECT_EQ(132u, entry_read_requests_[1].second);

  std::vector<uint8_t> written;
  ASSERT_TRUE(files::ReadFileToVector(files::JoinPath(temp_dir_.path(), EntryFileName(kBufferId)),
                                      &written));
  EXPECT_EQ(error_history_data_, written);
}

TEST_F(UfsUtilTest, ErrorHistoryOutputDirMissing) {
  InjectErrorHistoryEntry(0x10, 128);
  std::string missing_dir = files::JoinPath(temp_dir_.path(), "missing");
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "error-history", "-o", missing_dir.c_str()}),
                ZX_ERR_INVALID_ARGS);
  // The directory is checked before anything is read from the device.
  EXPECT_EQ(0u, read_buffer_count_);
}

TEST_F(UfsUtilTest, ErrorHistoryReadBufferError) {
  read_buffer_error_ = ZX_ERR_NOT_SUPPORTED;
  EXPECT_STATUS(
      RunCommand({"ufsutil", "<device>", "error-history", "-o", temp_dir_.path().c_str()}),
      ZX_ERR_NOT_SUPPORTED);
}

TEST_F(UfsUtilTest, ErrorHistoryEntryTooLarge) {
  // READ BUFFER carries the buffer offset in 24 bits, so larger entries cannot be read.
  InjectErrorHistoryEntry(0x10, (1u << 24u) + 1);
  EXPECT_STATUS(
      RunCommand({"ufsutil", "<device>", "error-history", "-o", temp_dir_.path().c_str()}),
      ZX_ERR_IO);
  EXPECT_EQ(0u, entry_read_count_);
}

TEST_F(UfsUtilTest, ErrorHistoryMissingRequiredArgs) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "error-history"}), ZX_ERR_INVALID_ARGS);
}

TEST_F(UfsUtilTest, ErrorHistoryNoArgument) {
  EXPECT_STATUS(RunCommand({"ufsutil", "<device>", "error-history", "-o"}), ZX_ERR_INVALID_ARGS);
}

TEST_F(UfsUtilTest, ErrorHistoryDirectoryZeroLength) {
  forced_directory_length_ = 0;
  EXPECT_STATUS(
      RunCommand({"ufsutil", "<device>", "error-history", "-o", temp_dir_.path().c_str()}),
      ZX_ERR_IO);
}

TEST_F(UfsUtilTest, ErrorHistoryDirectoryShorterThanEntries) {
  // The directory reports one entry although two are present; only the first one is read.
  error_history_data_.resize(128);
  FillTestData(error_history_data_);
  InjectErrorHistoryEntry(0x10, 128);
  InjectErrorHistoryEntry(0x11, 128);
  forced_directory_length_ = sizeof(ErrorDirectoryEntry);

  ASSERT_OK(RunCommand({"ufsutil", "<device>", "error-history", "-o", temp_dir_.path().c_str()}));
  EXPECT_EQ(1u, entry_read_count_);
  EXPECT_TRUE(files::IsFile(files::JoinPath(temp_dir_.path(), EntryFileName(0x10))));
  EXPECT_FALSE(files::IsFile(files::JoinPath(temp_dir_.path(), EntryFileName(0x11))));
}

TEST_F(UfsUtilTest, ErrorHistoryEntryZeroLength) {
  InjectErrorHistoryEntry(0x10, 0);
  EXPECT_STATUS(
      RunCommand({"ufsutil", "<device>", "error-history", "-o", temp_dir_.path().c_str()}),
      ZX_ERR_IO);
}

TEST_F(UfsUtilTest, ErrorHistoryEntryReadError) {
  // The directory is read, but the fake rejects the entry read because `error_history_data_` is
  // empty.
  InjectErrorHistoryEntry(0x10, 128);
  EXPECT_STATUS(
      RunCommand({"ufsutil", "<device>", "error-history", "-o", temp_dir_.path().c_str()}),
      ZX_ERR_OUT_OF_RANGE);
  EXPECT_EQ(1u, entry_read_count_);
  EXPECT_FALSE(files::IsFile(files::JoinPath(temp_dir_.path(), EntryFileName(0x10))));
}

TEST_F(UfsUtilTest, ErrorHistoryEntryBufferIdOutOfRange) {
  // Buffer IDs outside 10h - EFh are rejected before reading the entry.
  for (uint8_t buffer_id : {uint8_t{0x0F}, uint8_t{0xF0}}) {
    error_history_entries_.clear();
    InjectErrorHistoryEntry(buffer_id, 128);
    EXPECT_STATUS(
        RunCommand({"ufsutil", "<device>", "error-history", "-o", temp_dir_.path().c_str()}),
        ZX_ERR_IO);
  }
  EXPECT_EQ(0u, entry_read_count_);
}

TEST_F(UfsUtilTest, ErrorHistoryUnalignedLength) {
  InjectErrorHistoryEntry(0x10, 128);
  forced_directory_length_ = sizeof(ErrorDirectoryEntry) + 1;
  EXPECT_STATUS(
      RunCommand({"ufsutil", "<device>", "error-history", "-o", temp_dir_.path().c_str()}),
      ZX_ERR_IO);
}

TEST_F(UfsUtilTest, ErrorHistoryExceedMaxEntryCount) {
  // Buffer ID range as defined in the UFS spec (10h - EFh)
  constexpr size_t kMaxErrorHistoryEntries = 224;
  forced_directory_length_ = sizeof(ErrorDirectoryEntry) * (kMaxErrorHistoryEntries + 1);
  EXPECT_STATUS(
      RunCommand({"ufsutil", "<device>", "error-history", "-o", temp_dir_.path().c_str()}),
      ZX_ERR_IO);
}

TEST_F(UfsUtilTest, ErrorHistoryNoEntries) {
  EXPECT_STATUS(
      RunCommand({"ufsutil", "<device>", "error-history", "-o", temp_dir_.path().c_str()}),
      ZX_ERR_IO);
}

}  // namespace ufsutil
