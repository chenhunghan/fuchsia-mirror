// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/virtualization/bin/vmm/arch/arm64/pl011.h"

#include <zircon/types.h>

#include <utility>
#include <vector>

#include <gtest/gtest.h>

namespace {

using Data = std::vector<uint8_t>;

std::pair<zx_status_t, Data> ReadSocket(zx::socket& socket) {
  Data buf(16);
  size_t actual = 0;
  zx_status_t status = socket.read(0, buf.data(), buf.size(), &actual);
  buf.resize(actual);
  return {status, buf};
}

TEST(Pl011Test, ReadFlagRegisterReportsRxAndTxFifoEmpty) {
  zx::socket send, recv;
  ASSERT_EQ(ZX_OK, zx::socket::create(ZX_SOCKET_STREAM, &send, &recv));
  Pl011 uart(std::move(send));

  IoValue fr = IoValue::FromU16(0);
  ASSERT_EQ(ZX_OK, uart.Read(std::to_underlying(Pl011Register::FR), &fr));
  EXPECT_EQ(kPl011FlagRegisterRxFifoEmpty | kPl011FlagRegisterTxFifoEmpty, fr.u16);
}

TEST(Pl011Test, ReadDataRegisterReturnsZero) {
  zx::socket send, recv;
  ASSERT_EQ(ZX_OK, zx::socket::create(ZX_SOCKET_STREAM, &send, &recv));
  Pl011 uart(std::move(send));

  IoValue dr = IoValue::FromU16(0xffff);
  ASSERT_EQ(ZX_OK, uart.Read(std::to_underlying(Pl011Register::DR), &dr));
  EXPECT_EQ(0u, dr.u16);
}

TEST(Pl011Test, WriteDataRegister) {
  zx::socket send, recv;
  ASSERT_EQ(ZX_OK, zx::socket::create(ZX_SOCKET_STREAM, &send, &recv));
  Pl011 uart(std::move(send));

  ASSERT_EQ(ZX_OK, uart.Write(std::to_underlying(Pl011Register::DR), IoValue::FromU8('A')));
  ASSERT_EQ(ZX_OK, uart.Write(std::to_underlying(Pl011Register::DR), IoValue::FromU8('\r')));
  EXPECT_EQ(std::make_pair(ZX_OK, Data{'A', '\r'}), ReadSocket(recv));
}

}  // namespace
