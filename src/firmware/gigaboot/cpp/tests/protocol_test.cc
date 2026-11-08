// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "protocol.h"

#include <lib/efi/testing/stub_boot_services.h>
#include <lib/fit/defer.h>

#include <utility>

#include <efi/protocol/loaded-image.h>
#include <gtest/gtest.h>

#include "utils.h"

namespace {

using ::testing::_;
using ::testing::Return;

auto WithTestSystemTable(efi_system_table& table) {
  efi_system_table* prev = std::exchange(gEfiSystemTable, &table);
  EXPECT_EQ(prev, nullptr);
  return fit::defer([prev]() { gEfiSystemTable = prev; });
}

auto WithTestImageHandle(efi_handle handle) {
  EXPECT_TRUE(handle);
  efi_handle prev = std::exchange(gEfiImageHandle, handle);
  EXPECT_EQ(prev, nullptr);
  return fit::defer([prev]() { gEfiImageHandle = prev; });
}

TEST(EfiProtocolTest, EfiOpenProtocol) {
  efi::MockBootServices mock_boot_services;
  efi_system_table systab = {.BootServices = mock_boot_services.services()};
  auto use_systab = WithTestSystemTable(systab);

  // Normal success case.
  int handle = 1;
  efi_loaded_image_protocol protocol = {};
  mock_boot_services.ExpectOpenProtocol(&handle, EFI_LOADED_IMAGE_PROTOCOL_GUID, &protocol);
  {
    auto result = EfiOpenProtocol(&handle, LoadedImageProtocol);
    ASSERT_TRUE(result.is_ok()) << "EFI error " << result.error_value();
    auto* opened = static_cast<efi_loaded_image_protocol*>(result.value());
    EXPECT_EQ(opened, &protocol);
  }

  // Failure case. Also check the other arguments expected here, which
  // ExpectOpenProtocol doesn't check.
  int image_object = 2;
  auto use_image = WithTestImageHandle(&image_object);
  EXPECT_CALL(mock_boot_services,
              OpenProtocol(&handle, efi::MatchGuid(EFI_LOADED_IMAGE_PROTOCOL_GUID), _,
                           &image_object, nullptr, EFI_OPEN_PROTOCOL_BY_HANDLE_PROTOCOL))
      .WillOnce(Return(EFI_UNSUPPORTED));
  {
    auto result = EfiOpenProtocol(&handle, LoadedImageProtocol);
    ASSERT_TRUE(result.is_error()) << result.value();
    EXPECT_EQ(result.error_value(), EFI_UNSUPPORTED);
  }
}

TEST(EfiProtocolTest, EfiCloseProtocol) {
  efi::MockBootServices mock_boot_services;
  efi_system_table systab = {.BootServices = mock_boot_services.services()};
  auto use_systab = WithTestSystemTable(systab);

  int handle = 1;
  mock_boot_services.ExpectCloseProtocol(&handle, EFI_LOADED_IMAGE_PROTOCOL_GUID);
  EfiCloseProtocol(LoadedImageProtocol, &handle);
}

TEST(EfiProtocolTest, EfiProtocolPtr) {
  efi::MockBootServices mock_boot_services;
  efi_system_table systab = {.BootServices = mock_boot_services.services()};
  auto use_systab = WithTestSystemTable(systab);

  {
    EfiProtocolPtr<efi_loaded_image_protocol> image_ptr;
    EXPECT_FALSE(image_ptr);
    EXPECT_EQ(image_ptr.get(), nullptr);
  }

  int handle = 1;
  efi_loaded_image_protocol image = {};
  mock_boot_services.ExpectOpenProtocol(&handle, EFI_LOADED_IMAGE_PROTOCOL_GUID, &image);
  mock_boot_services.ExpectCloseProtocol(&image, EFI_LOADED_IMAGE_PROTOCOL_GUID);
  {
    auto result = EfiOpenProtocol<efi_loaded_image_protocol>(&handle);
    ASSERT_TRUE(result.is_ok()) << result.error_value();
    EfiProtocolPtr<efi_loaded_image_protocol> image_ptr = std::move(result).value();
    EXPECT_TRUE(image_ptr);
    EXPECT_EQ(image_ptr.get(), &image);
    // Destructor calls CloseProtocol.
  }
}

TEST(EfiProtocolTest, EfiHasProtocol) {
  efi::MockBootServices mock_boot_services;
  efi_system_table systab = {.BootServices = mock_boot_services.services()};
  auto use_systab = WithTestSystemTable(systab);

  int handle = 1;
  int image_object = 2;
  auto use_image = WithTestImageHandle(&image_object);

  EXPECT_CALL(mock_boot_services,
              OpenProtocol(&handle, efi::MatchGuid(EFI_LOADED_IMAGE_PROTOCOL_GUID), nullptr,
                           &image_object, nullptr, EFI_OPEN_PROTOCOL_TEST_PROTOCOL))
      .WillOnce(Return(EFI_SUCCESS))
      .WillOnce(Return(EFI_UNSUPPORTED));

  EXPECT_TRUE(EfiHasProtocol<efi_loaded_image_protocol>(&handle));
  EXPECT_FALSE(EfiHasProtocol<efi_loaded_image_protocol>(&handle));
}

}  // namespace
