// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#include <fidl/fuchsia.hardware.spmi/cpp/test_base.h>
#include <fidl/fuchsia.io/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/namespace.h>

#include <iostream>
#include <sstream>

#include <zxtest/zxtest.h>

#include "lib/zx/result.h"
#include "spmi-ctl-impl.h"

// RAII helper to safely restore stream buffers on test completion or failure.
class ScopedStreamRedirector {
 public:
  ScopedStreamRedirector(std::ostream& stream, std::ostream& target)
      : stream_(stream), old_buf_(stream.rdbuf(target.rdbuf())) {}
  ~ScopedStreamRedirector() { stream_.rdbuf(old_buf_); }

  ScopedStreamRedirector(const ScopedStreamRedirector&) = delete;
  ScopedStreamRedirector& operator=(const ScopedStreamRedirector&) = delete;

 private:
  std::ostream& stream_;
  std::streambuf* old_buf_;
};

class FakeSpmi : public fidl::testing::TestBase<fuchsia_hardware_spmi::Device>,
                 public fidl::Server<fuchsia_hardware_spmi::Debug> {
 public:
  explicit FakeSpmi(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {}

  // FIDL natural C++ methods for fuchsia.hardware.spmi.
  void GetProperties(GetPropertiesCompleter::Sync& completer) override {
    fuchsia_hardware_spmi::DeviceGetPropertiesResponse response;
    response.sid(123);
    response.register_width_bytes(register_width_bytes_);
    completer.Reply(std::move(response));
  }
  void RegisterRead(RegisterReadRequest& request, RegisterReadCompleter::Sync& completer) override {
    read_size_ = request.size_bytes();
    read_addresses_.push_back(request.address());
    // Allow multi-register reads when requested addresses fall within the written range.
    const size_t reg_count = (data_.size() + register_width_bytes_ - 1) / register_width_bytes_;
    if (address_ && request.address() >= *address_ && request.address() < *address_ + reg_count) {
      const size_t offset = (request.address() - *address_) * register_width_bytes_;
      std::vector<uint8_t> data;
      for (size_t i = 0; i < request.size_bytes(); ++i) {
        if (offset + i < data_.size()) {
          data.push_back(data_[offset + i]);
        } else {
          data.push_back(0);
        }
      }
      return completer.Reply(zx::ok(data));
    }
    completer.Reply(zx::error(fuchsia_hardware_spmi::DriverError::kBadState));
  }
  void RegisterWrite(RegisterWriteRequest& request,
                     RegisterWriteCompleter::Sync& completer) override {
    address_.emplace(request.address());
    data_ = request.data();
    return completer.Reply(zx::ok());
  }
  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_spmi::Device> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}
  void NotImplemented_(const std::string& name, ::fidl::CompleterBase& completer) override {
    FAIL();
  }

  void ConnectTarget(ConnectTargetRequest& request,
                     ConnectTargetCompleter::Sync& completer) override {
    if (request.target_id() >= fuchsia_hardware_spmi::kMaxTargets) {
      completer.Reply(fit::error(fuchsia_hardware_spmi::DriverError::kInvalidArgs));
      return;
    }
    target_id_ = request.target_id();
    device_bindings_.AddBinding(dispatcher_, std::move(request.server()), this,
                                fidl::kIgnoreBindingClosure);
    completer.Reply(zx::ok());
  }
  void GetControllerProperties(GetControllerPropertiesCompleter::Sync& completer) override {
    completer.Reply({{"spmi-controller"}});
  }
  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_spmi::Debug> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

  uint8_t target_id() const { return target_id_; }
  std::vector<uint8_t>& data() { return data_; }
  uint16_t address() { return *address_; }
  size_t read_size() { return read_size_; }
  void set_register_width_bytes(uint32_t width) { register_width_bytes_ = width; }
  const std::vector<uint16_t>& read_addresses() const { return read_addresses_; }
  void clear_read_addresses() { read_addresses_.clear(); }

 private:
  async_dispatcher_t* const dispatcher_;
  uint8_t target_id_{fuchsia_hardware_spmi::kMaxTargets};
  std::optional<uint16_t> address_;
  std::vector<uint8_t> data_;
  size_t read_size_{0};
  // Default register width in bytes.
  uint32_t register_width_bytes_{1};
  std::vector<uint16_t> read_addresses_;
  fidl::ServerBindingGroup<fuchsia_hardware_spmi::Device> device_bindings_;
};

class SpmiCtlTest : public zxtest::Test {
 public:
  void SetUp() override {
    loop_ = std::make_unique<async::Loop>(&kAsyncLoopConfigAttachToCurrentThread);
    ASSERT_OK(loop_->StartThread("spmi-ctl-test-loop"));
    spmi_ = std::make_unique<FakeSpmi>(loop_->dispatcher());
  }

  void TearDown() override { loop_->Shutdown(); }

  int CallSpmiCtl(std::vector<std::string> args) {
    constexpr size_t kMaxArgs = 64;
    char* argv[kMaxArgs];
    ZX_ASSERT(args.size() <= kMaxArgs);
    for (size_t i = 0; i < args.size(); ++i) {
      argv[i] = const_cast<char*>(args[i].c_str());
    }
    fidl::ClientEnd<fuchsia_hardware_spmi::Debug> client;
    zx::result server = fidl::CreateEndpoints(&client);
    ZX_ASSERT(server.status_value() == ZX_OK);
    fidl::BindServer(loop_->dispatcher(), std::move(server.value()), spmi_.get());
    spmi_ctl_.emplace(SpmiCtl(std::move(client)));
    return spmi_ctl_->Execute(static_cast<int>(args.size()), argv);
  }

 protected:
  std::unique_ptr<async::Loop> loop_;
  std::unique_ptr<FakeSpmi> spmi_;
  std::optional<SpmiCtl> spmi_ctl_;
};

// Tests that invoking spmi-ctl with unknown command line flags returns an error.
TEST_F(SpmiCtlTest, UnknownCommands) {
  EXPECT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-b"}), -1);
  EXPECT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "--bad"}), -1);
}

// Tests that invalid or out-of-range target identifiers return an error.
TEST_F(SpmiCtlTest, InvalidTarget) {
  EXPECT_EQ(CallSpmiCtl({"spmi-ctl", "-a", "0x1234", "-r", "4"}), -1);
  EXPECT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "-a", "0x1234", "-r", "4"}), -1);
  EXPECT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "16", "-a", "0x1234", "-r", "4"}), -1);
  EXPECT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0x10", "-a", "0x1234", "-r", "4"}), -1);
  EXPECT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "100", "-a", "0x1234", "-r", "4"}), -1);
  EXPECT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "abc", "-a", "0x1234", "-r", "4"}), -1);
  EXPECT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "-1", "-a", "0x1234", "-r", "4"}), -1);
}

// Tests successful register write and read operations using both hex and decimal inputs.
TEST_F(SpmiCtlTest, ReadWriteSuccess) {
  std::vector<uint8_t> canned_data;
  constexpr uint32_t kWidth1Byte = 1;

  // Write then read 4 bytes using hex inputs with 0x prefix.
  ASSERT_EQ(
      CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1122", "-w", "0x11", "0x22", "0x33", "0x44"}),
      0);
  EXPECT_EQ(spmi_->target_id(), 0);
  canned_data = {0x11, 0x22, 0x33, 0x44};
  EXPECT_TRUE(spmi_->data() == canned_data);
  EXPECT_EQ(spmi_->address(), 0x1122);
  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1122", "-r", "0x4"}), 0);
  EXPECT_EQ(spmi_->target_id(), 0);
  EXPECT_EQ(spmi_->read_size(), kWidth1Byte);
  const std::vector<uint16_t> expected_read_addrs_hex = {0x1122, 0x1123, 0x1124, 0x1125};
  EXPECT_EQ(spmi_->read_addresses(), expected_read_addrs_hex);

  // Write then read 4 bytes using decimal inputs without 0x prefix.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "4386", "-w", "11", "22", "33", "44"}), 0);
  EXPECT_EQ(spmi_->target_id(), 0);
  canned_data = {11, 22, 33, 44};
  EXPECT_TRUE(spmi_->data() == canned_data);
  EXPECT_EQ(spmi_->address(), 4386);
  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "4386", "-r", "4"}), 0);
  EXPECT_EQ(spmi_->target_id(), 0);
  EXPECT_EQ(spmi_->read_size(), kWidth1Byte);
  const std::vector<uint16_t> expected_read_addrs_dec = {4386, 4387, 4388, 4389};
  EXPECT_EQ(spmi_->read_addresses(), expected_read_addrs_dec);

  // Write then read 9 bytes.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1111", "-w", "1", "2", "3", "4", "5", "6",
                         "7", "8", "9"}),
            0);
  EXPECT_EQ(spmi_->target_id(), 0);
  canned_data = {1, 2, 3, 4, 5, 6, 7, 8, 9};
  EXPECT_TRUE(spmi_->data() == canned_data);
  EXPECT_EQ(spmi_->address(), 0x1111);
  spmi_->clear_read_addresses();
  constexpr size_t kRead9Registers = 9;
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1111", "-r", "9"}), 0);
  EXPECT_EQ(spmi_->target_id(), 0);
  EXPECT_EQ(spmi_->read_size(), kWidth1Byte);
  EXPECT_EQ(spmi_->read_addresses().size(), kRead9Registers);
}

// Tests error conditions during register write and read operations.
TEST_F(SpmiCtlTest, ReadWriteErrors) {
  // No address.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-w", "0x12", "0x34", "0x56"}), -1);
  // Unknown address.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x5678", "-r", "4"}), -1);
  // Address too big (hex).
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x10000", "-r", "4"}), -1);
  // Address too big (decimal).
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "65536", "-r", "4"}), -1);
  // Write no data.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-w"}), -1);
  // Read size 0 (decimal).
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-r", "0"}), -1);
  // Read size 0 (hex).
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-r", "0x0"}), -1);
  // Read size too big (hex).
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-r", "0x100000000"}), -1);
  // Read size too big (decimal).
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-r", "4294967296"}), -1);
  // Write byte > 255 (decimal).
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-w", "256"}), -1);
  // Write byte > 0xff (hex).
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-w", "0x100"}), -1);
  // Second write byte > 255 (decimal).
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-w", "0x10", "256"}), -1);
  // Second write byte > 0xff (hex).
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-w", "0x10", "0x100"}), -1);
  // Second write byte < 0.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-w", "0x10", "-1"}), -1);
  // Third write byte > 255 (decimal).
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-w", "0x10", "0x20", "256"}), -1);
  // Third write byte > 0xff (hex).
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-w", "0x10", "0x20", "0x100"}),
            -1);
  // Third write byte < 0.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-w", "0x10", "0x20", "-1"}), -1);
  // Target with invalid trailing characters.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0abc", "-a", "0x1234", "-w", "0x10"}), -1);
  // Address with invalid trailing characters.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234xyz", "-w", "0x10"}), -1);
  // Read size with invalid trailing characters.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-r", "4xyz"}), -1);
  // Write byte with invalid trailing characters.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-w", "10xyz"}), -1);
}

// Tests base-aware parsing where 0x or 0X prefix indicates hex and no prefix indicates decimal.
TEST_F(SpmiCtlTest, BaseAwareParsing) {
  // Test target parsing in hex (0xa == 10) and decimal (10).
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0xa", "-a", "0x1234", "-w", "0x1"}), 0);
  EXPECT_EQ(spmi_->target_id(), 10);
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "10", "-a", "0x1234", "-w", "0x1"}), 0);
  EXPECT_EQ(spmi_->target_id(), 10);

  // Test target parsing with uppercase 0X (0XF == 15).
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0XF", "-a", "0x1234", "-w", "0x1"}), 0);
  EXPECT_EQ(spmi_->target_id(), 15);

  // Test address parsing: 0x1234 in hex and 4660 in decimal.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-w", "0x10"}), 0);
  EXPECT_EQ(spmi_->address(), 0x1234);
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "4660", "-w", "16"}), 0);
  EXPECT_EQ(spmi_->address(), 4660);

  // Test write bytes: 0x10 is 16 in hex, 10 is 10 in decimal.
  std::vector<uint8_t> hex_data = {16};
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-w", "0x10"}), 0);
  EXPECT_TRUE(spmi_->data() == hex_data);
  std::vector<uint8_t> dec_data = {10};
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-w", "10"}), 0);
  EXPECT_TRUE(spmi_->data() == dec_data);

  // Test read size: 0x2 is 2 bytes, 2 is 2 bytes (with 2 bytes written in advance).
  constexpr size_t kRead2Registers = 2;
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-w", "0x1", "0x2"}), 0);
  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-r", "0x2"}), 0);
  EXPECT_EQ(spmi_->read_addresses().size(), kRead2Registers);
  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-r", "2"}), 0);
  EXPECT_EQ(spmi_->read_addresses().size(), kRead2Registers);
}

// Tests that dump requests with sizes greater than 255 bytes (e.g. 256 bytes) read one register
// at a time and do not wrap or truncate to 0.
TEST_F(SpmiCtlTest, DumpLargeSize) {
  constexpr uint32_t kLargeDumpSize = 256;
  constexpr uint32_t kWidth1Byte = 1;
  constexpr size_t kExpectedReads = 256;

  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-w", "0x1"}), 0);
  spmi_->data().resize(kLargeDumpSize);

  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-d", "256"}), 0);
  EXPECT_EQ(spmi_->read_size(), kWidth1Byte);
  EXPECT_EQ(spmi_->read_addresses().size(), kExpectedReads);

  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-d", "0x100"}), 0);
  EXPECT_EQ(spmi_->read_size(), kWidth1Byte);
  EXPECT_EQ(spmi_->read_addresses().size(), kExpectedReads);
}

// Verifies that spmi-ctl retrieves and displays device properties including register width.
TEST_F(SpmiCtlTest, GetProperties) {
  // Test with default 1-byte register width.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-p"}), 0);
  EXPECT_EQ(spmi_->target_id(), 0);

  // Test with 2-byte register width.
  constexpr uint32_t kWidth2Bytes = 2;
  spmi_->set_register_width_bytes(kWidth2Bytes);
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-p"}), 0);
  EXPECT_EQ(spmi_->target_id(), 0);
}

// Verifies reading registers one by one with different register widths (1-byte and 2-byte).
TEST_F(SpmiCtlTest, ReadWithRegisterWidth) {
  constexpr uint32_t kWidth1Byte = 1;
  constexpr uint32_t kWidth2Bytes = 2;

  // Read 4 registers with default 1-byte register width (4 reads of 1 byte each).
  ASSERT_EQ(
      CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-w", "0x11", "0x22", "0x33", "0x44"}),
      0);
  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-r", "4"}), 0);
  EXPECT_EQ(spmi_->read_size(), kWidth1Byte);
  const std::vector<uint16_t> expected_1byte_addrs = {0x1234, 0x1235, 0x1236, 0x1237};
  EXPECT_EQ(spmi_->read_addresses(), expected_1byte_addrs);

  // Read 2 registers with 2-byte register width (2 reads of 2 bytes each).
  spmi_->set_register_width_bytes(kWidth2Bytes);
  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-r", "2"}), 0);
  EXPECT_EQ(spmi_->read_size(), kWidth2Bytes);
  const std::vector<uint16_t> expected_2byte_addrs = {0x1234, 0x1235};
  EXPECT_EQ(spmi_->read_addresses(), expected_2byte_addrs);
}

// Verifies dumping registers reads one register at a time with device register width.
TEST_F(SpmiCtlTest, DumpWithRegisterWidth) {
  constexpr uint32_t kWidth1Byte = 1;
  constexpr uint32_t kWidth2Bytes = 2;

  // Dump 4 bytes with 1-byte register width issues 4 reads of 1 byte each.
  spmi_->set_register_width_bytes(kWidth1Byte);
  ASSERT_EQ(
      CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1000", "-w", "0x11", "0x22", "0x33", "0x44"}),
      0);
  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1000", "-d", "4"}), 0);
  const std::vector<uint16_t> expected_1byte_addrs = {0x1000, 0x1001, 0x1002, 0x1003};
  EXPECT_EQ(spmi_->read_addresses(), expected_1byte_addrs);
  EXPECT_EQ(spmi_->read_size(), kWidth1Byte);

  // Dump 4 bytes with 2-byte register width issues 2 reads of 2 bytes each.
  spmi_->set_register_width_bytes(kWidth2Bytes);
  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1000", "-d", "4"}), 0);
  const std::vector<uint16_t> expected_2byte_addrs = {0x1000, 0x1001};
  EXPECT_EQ(spmi_->read_addresses(), expected_2byte_addrs);
  EXPECT_EQ(spmi_->read_size(), kWidth2Bytes);
}

// Verifies reading registers one by one with explicit register width argument (-i and --width).
TEST_F(SpmiCtlTest, ReadWithRegisterWidthArg) {
  constexpr uint32_t kWidth2Bytes = 2;
  constexpr uint32_t kWidth4Bytes = 4;

  ASSERT_EQ(
      CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-w", "0x11", "0x22", "0x33", "0x44"}),
      0);

  // Read 2 registers specifying -i 2 (2 reads of 2 bytes each).
  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-i", "2", "-r", "2"}), 0);
  EXPECT_EQ(spmi_->read_size(), kWidth2Bytes);
  const std::vector<uint16_t> expected_2byte_addrs = {0x1234, 0x1235};
  EXPECT_EQ(spmi_->read_addresses(), expected_2byte_addrs);

  // Read 1 register specifying --width 4 (1 read of 4 bytes).
  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "--width", "4", "-r", "1"}), 0);
  EXPECT_EQ(spmi_->read_size(), kWidth4Bytes);
  const std::vector<uint16_t> expected_4byte_addrs = {0x1234};
  EXPECT_EQ(spmi_->read_addresses(), expected_4byte_addrs);
}

// Verifies dumping registers one by one with explicit register width argument (-i, --width).
TEST_F(SpmiCtlTest, DumpWithRegisterWidthArg) {
  constexpr uint32_t kWidth2Bytes = 2;
  constexpr uint32_t kWidth4Bytes = 4;
  ASSERT_EQ(
      CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1000", "-w", "0x11", "0x22", "0x33", "0x44"}),
      0);

  // Dump 4 bytes with -i 2 reads 2 registers of 2 bytes each.
  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1000", "-i", "2", "-d", "4"}), 0);
  const std::vector<uint16_t> expected_2byte_addrs = {0x1000, 0x1001};
  EXPECT_EQ(spmi_->read_addresses(), expected_2byte_addrs);
  EXPECT_EQ(spmi_->read_size(), kWidth2Bytes);

  // Dump 4 bytes with --width 4 reads 1 register of 4 bytes.
  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1000", "--width", "4", "-d", "4"}), 0);
  const std::vector<uint16_t> expected_4byte_addrs = {0x1000};
  EXPECT_EQ(spmi_->read_addresses(), expected_4byte_addrs);
  EXPECT_EQ(spmi_->read_size(), kWidth4Bytes);
}

// Verifies that dump continues with next register and displays "--" when reading registers fails.
TEST_F(SpmiCtlTest, DumpReadError) {
  // Capture stdout using RAII stream redirector.
  std::stringstream out_stream;
  ScopedStreamRedirector cout_redir(std::cout, out_stream);

  // Attempting to dump an uninitialized address causes reads to return kBadState error,
  // but dump continues and outputs "--" in the table.
  EXPECT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1000", "-d", "4"}), 0);
  std::string output = out_stream.str();
  EXPECT_TRUE(output.find("1000: -- -- -- --") != std::string::npos);
  EXPECT_EQ(spmi_->read_addresses().size(), 4);
}

// Verifies errors when dump size is less than register width,
// total read bytes overflow, or register width argument is invalid.
TEST_F(SpmiCtlTest, RegisterWidthValidationErrors) {
  constexpr uint32_t kWidth2Bytes = 2;
  spmi_->set_register_width_bytes(kWidth2Bytes);

  // Dump 1 byte on 2-byte width fails because it is less than one register width.
  EXPECT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-d", "1"}), -1);

  // Dump 3 bytes with -i 4 fails because it is less than one register width.
  EXPECT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-i", "4", "-d", "3"}), -1);

  // Read with total bytes overflow fails.
  EXPECT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-i", "2", "-r", "0x80000000"}),
            -1);

  // Invalid width arguments.
  EXPECT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-i", "0", "-r", "4"}), -1);
  EXPECT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-i", "-1", "-r", "4"}), -1);
  EXPECT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-i", "abc", "-r", "4"}), -1);
}

// Verifies that dump requests with sizes not aligned to register width
// are truncated to the nearest multiple with a warning.
TEST_F(SpmiCtlTest, DumpTruncateToRegisterWidth) {
  constexpr uint32_t kWidth2Bytes = 2;
  constexpr uint32_t kWidth4Bytes = 4;
  constexpr size_t kExpected1Register = 1;
  ASSERT_EQ(
      CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1000", "-w", "0x11", "0x22", "0x33", "0x44"}),
      0);

  // Capture stderr using RAII stream redirector.
  std::stringstream err_stream;
  ScopedStreamRedirector cerr_redir(std::cerr, err_stream);

  // Dump 3 bytes on 2-byte register width truncates to 2 bytes (1 register).
  spmi_->set_register_width_bytes(kWidth2Bytes);
  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1000", "-d", "3"}), 0);
  EXPECT_TRUE(err_stream.str().find("WARNING: Truncating dump size to register width (2 bytes)") !=
              std::string::npos);
  EXPECT_EQ(spmi_->read_addresses().size(), kExpected1Register);
  EXPECT_EQ(spmi_->read_size(), kWidth2Bytes);

  // Dump 5 bytes with -i 4 truncates to 4 bytes (1 register).
  err_stream.str("");
  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1000", "-i", "4", "-d", "5"}), 0);
  EXPECT_TRUE(err_stream.str().find("WARNING: Truncating dump size to register width (4 bytes)") !=
              std::string::npos);
  EXPECT_EQ(spmi_->read_addresses().size(), kExpected1Register);
  EXPECT_EQ(spmi_->read_size(), kWidth4Bytes);
}

// Tests read operation when requested address range exceeds 16-bit address limit.
TEST_F(SpmiCtlTest, ReadAddressOutOfRange) {
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0xffff", "-w", "0xaa"}), 0);
  // Reading 2 registers from 0xffff attempts to read 0xffff and 0x10000, which terminates.
  EXPECT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0xffff", "-r", "2"}), -1);
}

// Tests successful register dump operations using hex and decimal counts.
TEST_F(SpmiCtlTest, DumpSuccess) {
  constexpr size_t kDump4Bytes = 4;
  constexpr uint32_t kDump1Byte = 1;
  constexpr size_t kDump32Bytes = 32;

  // Write 4 bytes, then dump 4 bytes in hex.
  ASSERT_EQ(
      CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1122", "-w", "0x11", "0x22", "0x33", "0x44"}),
      0);
  EXPECT_EQ(spmi_->target_id(), 0);
  EXPECT_EQ(spmi_->address(), 0x1122);
  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1122", "-d", "0x4"}), 0);
  EXPECT_EQ(spmi_->target_id(), 0);
  EXPECT_EQ(spmi_->read_size(), kDump1Byte);
  EXPECT_EQ(spmi_->read_addresses().size(), kDump4Bytes);

  // Dump 4 bytes in decimal.
  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1122", "-d", "4"}), 0);
  EXPECT_EQ(spmi_->target_id(), 0);
  EXPECT_EQ(spmi_->read_size(), kDump1Byte);
  EXPECT_EQ(spmi_->read_addresses().size(), kDump4Bytes);

  // Dump 1 byte.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1122", "-d", "1"}), 0);
  EXPECT_EQ(spmi_->target_id(), 0);
  EXPECT_EQ(spmi_->read_size(), kDump1Byte);

  // Write and dump 32 bytes to test 16-byte table row formatting.
  constexpr uint32_t kWidth1Byte = 1;
  std::vector<std::string> write_32bytes = {"spmi-ctl", "-t", "0", "-a", "0x1000", "-w"};
  for (size_t i = 0; i < kDump32Bytes; ++i) {
    write_32bytes.push_back(std::format("0x{:02x}", i));
  }
  ASSERT_EQ(CallSpmiCtl(write_32bytes), 0);
  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1000", "-d", "32"}), 0);
  EXPECT_EQ(spmi_->read_size(), kWidth1Byte);
  EXPECT_EQ(spmi_->read_addresses().size(), kDump32Bytes);
}

// Tests error conditions during dump operations.
TEST_F(SpmiCtlTest, DumpErrors) {
  // No address.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-d", "4"}), -1);
  // Dump size 0 (decimal).
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-d", "0"}), -1);
  // Dump size 0 (hex).
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-d", "0x0"}), -1);
  // Dump size negative.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-d", "-1"}), -1);
  // Dump size with invalid trailing characters.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1234", "-d", "4xyz"}), -1);
  // Missing target.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-a", "0x1234", "-d", "4"}), -1);
}

// Tests dump operation when requested address range exceeds 16-bit address limit.
TEST_F(SpmiCtlTest, DumpAddressOutOfRange) {
  constexpr uint32_t kWidth1Byte = 1;
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0xffff", "-w", "0xaa"}), 0);
  // Dumping 2 bytes from 0xffff attempts to read 0xffff (ok) and 0x10000 (exceeds 16-bit).
  spmi_->clear_read_addresses();
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0xffff", "-d", "2"}), 0);
  const std::vector<uint16_t> expected_addrs = {0xffff};
  EXPECT_EQ(spmi_->read_addresses(), expected_addrs);
  EXPECT_EQ(spmi_->read_size(), kWidth1Byte);
}

// Verifies dumping an unaligned range that crosses a 16-byte table row boundary.
TEST_F(SpmiCtlTest, DumpUnalignedMultiRow) {
  constexpr uint32_t kWidth1Byte = 1;
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x100e", "-w", "0x11", "0x22", "0x33",
                         "0x44", "0x55", "0x66"}),
            0);
  spmi_->clear_read_addresses();

  // Capture stdout to verify table layout across row boundaries.
  std::stringstream out_stream;
  ScopedStreamRedirector cout_redir(std::cout, out_stream);

  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x100e", "-d", "6"}), 0);
  const std::vector<uint16_t> expected_addrs = {0x100e, 0x100f, 0x1010, 0x1011, 0x1012, 0x1013};
  EXPECT_EQ(spmi_->read_addresses(), expected_addrs);
  EXPECT_EQ(spmi_->read_size(), kWidth1Byte);

  const std::string output = out_stream.str();
  EXPECT_TRUE(output.find("1000:") != std::string::npos);
  EXPECT_TRUE(output.find("11 22") != std::string::npos);
  EXPECT_TRUE(output.find("1010: 33 44 55 66") != std::string::npos);
}

// Verifies table output formatting and width warnings.
TEST_F(SpmiCtlTest, DumpTableOutputAndWarning) {
  // Write 3 bytes at 0x1000.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1000", "-w", "0x41", "0x20", "0x42"}), 0);

  // Capture stdout and stderr using RAII stream redirectors.
  std::stringstream out_stream;
  std::stringstream err_stream;
  ScopedStreamRedirector cout_redir(std::cout, out_stream);
  ScopedStreamRedirector cerr_redir(std::cerr, err_stream);

  // Dump 3 bytes with default 1-byte width: no width warning on stderr.
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1000", "-d", "3"}), 0);
  std::string output = out_stream.str();
  std::string errors = err_stream.str();

  EXPECT_TRUE(output.find("0  1  2  3  4  5  6  7  8  9  a  b  c  d  e  f") != std::string::npos);
  EXPECT_TRUE(output.find("1000: 41 20 42") != std::string::npos);
  EXPECT_TRUE(errors.empty());

  // Test width warning on stderr when target register width is 2.
  constexpr uint32_t kWidth2Bytes = 2;
  spmi_->set_register_width_bytes(kWidth2Bytes);
  err_stream.str("");
  ASSERT_EQ(CallSpmiCtl({"spmi-ctl", "-t", "0", "-a", "0x1000", "-d", "2"}), 0);
  EXPECT_TRUE(err_stream.str().find("WARNING: Register width in the SPMI target is 2") !=
              std::string::npos);
}
