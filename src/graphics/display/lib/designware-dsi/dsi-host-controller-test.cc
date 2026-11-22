// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/lib/designware-dsi/dsi-host-controller.h"

#include <lib/driver/mmio/cpp/mmio-buffer.h>
#include <lib/mipi-dsi/mipi-dsi.h>
#include <lib/mmio-ptr/fake.h>
#include <lib/zx/vmo.h>

#include <array>
#include <cstdint>

#include <gtest/gtest.h>

#include "src/graphics/display/lib/designware-dsi/dw-mipi-dsi-reg.h"

namespace designware_dsi {

namespace {

class DsiHostControllerTest : public ::testing::Test {
 protected:
  fdf::MmioBuffer CreateMmioBuffer() {
    return fdf::MmioBuffer(zx::vmo(), FakeMmioPtr(mmio_words_.data()), 0,
                           mmio_words_.size() * sizeof(uint32_t));
  }

 private:
  static constexpr size_t kMmioWordCount = 0x40;
  std::array<uint32_t, kMmioWordCount> mmio_words_{};
};

TEST_F(DsiHostControllerTest, IssueCommandsGenWriteShort) {
  DsiHostController controller(CreateMmioBuffer());

  static constexpr std::array<uint8_t, 2> kPayload = {0x12, 0x34};
  const mipi_dsi::DsiCommandAndResponse commands[] = {
      {
          .virtual_channel_id = 1,
          .data_type = kMipiDsiDtGenShortWrite2,
          .payload = kPayload,
          .response_payload = {},
      },
  };

  zx::result<> result = controller.IssueCommands(commands);
  EXPECT_TRUE(result.is_ok());

  fdf::MmioBuffer mmio = CreateMmioBuffer();
  DsiDwGenHdrReg header = DsiDwGenHdrReg::Get().ReadFrom(&mmio);
  EXPECT_EQ(header.gen_dt(), kMipiDsiDtGenShortWrite2);
  EXPECT_EQ(header.gen_vc(), 1u);
  EXPECT_EQ(header.gen_wc_lsbyte(), 0x12u);
  EXPECT_EQ(header.gen_wc_msbyte(), 0x34u);
}

TEST_F(DsiHostControllerTest, IssueCommandsDcsWriteShort) {
  DsiHostController controller(CreateMmioBuffer());

  static constexpr std::array<uint8_t, 2> kPayload = {0x56, 0x78};
  const mipi_dsi::DsiCommandAndResponse commands[] = {
      {
          .virtual_channel_id = 2,
          .data_type = kMipiDsiDtDcsShortWrite1,
          .payload = kPayload,
          .response_payload = {},
      },
  };

  zx::result<> result = controller.IssueCommands(commands);
  EXPECT_TRUE(result.is_ok());

  fdf::MmioBuffer mmio = CreateMmioBuffer();
  DsiDwGenHdrReg header = DsiDwGenHdrReg::Get().ReadFrom(&mmio);
  EXPECT_EQ(header.gen_dt(), kMipiDsiDtDcsShortWrite1);
  EXPECT_EQ(header.gen_vc(), 2u);
  EXPECT_EQ(header.gen_wc_lsbyte(), 0x56u);
  EXPECT_EQ(header.gen_wc_msbyte(), 0x78u);
}

TEST_F(DsiHostControllerTest, IssueCommandsGenWriteLong) {
  DsiHostController controller(CreateMmioBuffer());

  static constexpr std::array<uint8_t, 6> kPayload = {0x01, 0x02, 0x03, 0x04, 0x05, 0x06};
  const mipi_dsi::DsiCommandAndResponse commands[] = {
      {
          .virtual_channel_id = 3,
          .data_type = kMipiDsiDtGenLongWrite,
          .payload = kPayload,
          .response_payload = {},
      },
  };

  zx::result<> result = controller.IssueCommands(commands);
  EXPECT_TRUE(result.is_ok());

  fdf::MmioBuffer mmio = CreateMmioBuffer();
  DsiDwGenHdrReg header = DsiDwGenHdrReg::Get().ReadFrom(&mmio);
  EXPECT_EQ(header.gen_dt(), kMipiDsiDtGenLongWrite);
  EXPECT_EQ(header.gen_vc(), 3u);
  EXPECT_EQ(header.gen_wc_lsbyte(), kPayload.size());
  EXPECT_EQ(header.gen_wc_msbyte(), 0u);

  DsiDwGenPldDataReg last_payload = DsiDwGenPldDataReg::Get().ReadFrom(&mmio);
  EXPECT_EQ(last_payload.gen_pld_b1(), 0x05u);
  EXPECT_EQ(last_payload.gen_pld_b2(), 0x06u);
}

TEST_F(DsiHostControllerTest, IssueCommandsGenRead) {
  fdf::MmioBuffer setup_mmio = CreateMmioBuffer();
  DsiDwGenPldDataReg::Get().FromValue(0xddccbbaa).WriteTo(&setup_mmio);

  DsiHostController controller(CreateMmioBuffer());

  static constexpr std::array<uint8_t, 2> kPayload = {0x9a, 0xbc};
  std::array<uint8_t, 4> response = {};
  const mipi_dsi::DsiCommandAndResponse commands[] = {
      {
          .virtual_channel_id = 1,
          .data_type = kMipiDsiDtGenShortRead2,
          .payload = kPayload,
          .response_payload = response,
      },
  };

  zx::result<> result = controller.IssueCommands(commands);
  EXPECT_TRUE(result.is_ok());

  fdf::MmioBuffer mmio = CreateMmioBuffer();
  DsiDwGenHdrReg header = DsiDwGenHdrReg::Get().ReadFrom(&mmio);
  EXPECT_EQ(header.gen_dt(), kMipiDsiDtGenShortRead2);
  EXPECT_EQ(header.gen_vc(), 1u);
  EXPECT_EQ(header.gen_wc_lsbyte(), 0x9au);
  EXPECT_EQ(header.gen_wc_msbyte(), 0xbcu);

  EXPECT_EQ(response[0], 0xaau);
  EXPECT_EQ(response[1], 0xbbu);
  EXPECT_EQ(response[2], 0xccu);
  EXPECT_EQ(response[3], 0xddu);
}

}  // namespace

}  // namespace designware_dsi
