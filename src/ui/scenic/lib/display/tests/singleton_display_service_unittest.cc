// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/display/singleton_display_service.h"

#include <fidl/fuchsia.hardware.display.types/cpp/fidl.h>
#include <fidl/fuchsia.images2/cpp/fidl.h>
#include <fidl/fuchsia.ui.input.internal/cpp/fidl.h>
#include <fidl/fuchsia.ui.views/cpp/fidl.h>
#include <lib/zx/event.h>
#include <lib/zx/eventpair.h>

#include <cstdint>
#include <memory>
#include <optional>
#include <vector>

#include <gtest/gtest.h>

#include "src/lib/fsl/handles/object_info.h"

namespace display::test {

constexpr uint32_t kMaxDisplayLayersCount = 2;

TEST(SingletonDisplayService, GetMetrics) {
  static constexpr uint32_t kWidthInPx = 777;
  static constexpr uint32_t kHeightInPx = 555;
  static constexpr uint32_t kWidthInMm = 77;
  static constexpr uint32_t kHeightInMm = 55;
  static constexpr uint32_t kRefreshRate = 44000;
  auto display = std::make_shared<Display>(
      display::WireDisplayId{.value = 1},
      WireDisplayMode{.active_area = {.width = kWidthInPx, .height = kHeightInPx},
                      .refresh_rate_millihertz = kRefreshRate},
      kWidthInMm, kHeightInMm, kMaxDisplayLayersCount,
      std::vector{fuchsia_images2::wire::PixelFormat::kB8G8R8A8});
  auto singleton = std::make_unique<SingletonDisplayService>(display);

  uint32_t width_in_px = 0;
  uint32_t height_in_px = 0;
  uint32_t width_in_mm = 0;
  uint32_t height_in_mm = 0;
  float dpr_x = 0.f;
  float dpr_y = 0.f;
  uint32_t refresh_rate = 0;

  singleton->GetMetrics([&](auto response) {
    auto& info = response.info();
    ASSERT_TRUE(info.extent_in_px().has_value());
    width_in_px = info.extent_in_px()->width();
    height_in_px = info.extent_in_px()->height();
    ASSERT_TRUE(info.extent_in_mm().has_value());
    width_in_mm = info.extent_in_mm()->width();
    height_in_mm = info.extent_in_mm()->height();
    ASSERT_TRUE(info.recommended_device_pixel_ratio().has_value());
    dpr_x = info.recommended_device_pixel_ratio()->x();
    dpr_y = info.recommended_device_pixel_ratio()->y();
    ASSERT_TRUE(info.maximum_refresh_rate_in_millihertz().has_value());
    refresh_rate = info.maximum_refresh_rate_in_millihertz().value();
  });

  EXPECT_EQ(width_in_px, kWidthInPx);
  EXPECT_EQ(height_in_px, kHeightInPx);
  EXPECT_EQ(width_in_mm, kWidthInMm);
  EXPECT_EQ(height_in_mm, kHeightInMm);
  EXPECT_EQ(dpr_x, 1.f);
  EXPECT_EQ(dpr_y, 1.f);
  EXPECT_EQ(refresh_rate, kRefreshRate);
}

TEST(SingletonDisplayService, DevicePixelRatioChange) {
  auto display =
      std::make_shared<Display>(display::WireDisplayId{.value = 1},
                                WireDisplayMode{.active_area = {.width = 777, .height = 555},
                                                .refresh_rate_millihertz = 4400},
                                /*width_in_mm=*/77, /*height_in_mm=*/55, kMaxDisplayLayersCount,
                                std::vector{fuchsia_images2::PixelFormat::kB8G8R8A8});
  auto singleton = std::make_unique<SingletonDisplayService>(display);

  const float kDPRx = 1.25f;
  const float kDPRy = 1.25f;
  display->set_device_pixel_ratio({kDPRx, kDPRy});

  float dpr_x = 0.f;
  float dpr_y = 0.f;
  singleton->GetMetrics([&](auto response) {
    auto& dpr = response.info().recommended_device_pixel_ratio();
    dpr_x = dpr->x();
    dpr_y = dpr->y();
  });

  EXPECT_EQ(dpr_x, kDPRx);
  EXPECT_EQ(dpr_y, kDPRy);
}

class SingletonDisplayServiceTest : public ::testing::Test {
 protected:
  void SetUp() override {
    display_ = std::make_shared<Display>(display::WireDisplayId{.value = 1},
                                         /*width_in_px=*/777, /*height_in_px=*/555,
                                         kMaxDisplayLayersCount);
    singleton_ = std::make_unique<SingletonDisplayService>(display_);
  }

  struct GetEventResult {
    zx_status_t status = ZX_OK;
    std::optional<fit::result<fuchsia_ui_input_internal::InputOwnershipError,
                              fuchsia_ui_input_internal::InputOwnershipGetEventResponse>>
        result;
  };

  GetEventResult GetInputOwnershipEvent(
      fuchsia_ui_input_internal::InputOwnershipGetEventRequest request) {
    GetEventResult out;
    singleton_->GetEvent(std::move(request), [&](zx_status_t status, auto res) {
      out.status = status;
      out.result = std::move(res);
    });
    return out;
  }

  std::shared_ptr<Display> display_;
  std::unique_ptr<SingletonDisplayService> singleton_;
};

TEST_F(SingletonDisplayServiceTest, GetOwnershipEvent) {
  std::optional<zx::event> event;
  singleton_->GetEvent(
      [&](fuchsia_ui_composition_internal::DisplayOwnershipGetEventResponse response) {
        event = std::move(response.ownership_event());
      });
  ASSERT_TRUE(event.has_value());
  EXPECT_EQ(fsl::GetKoid(event->get()), fsl::GetKoid(display_->ownership_event().get()));
}

TEST_F(SingletonDisplayServiceTest, InputOwnershipGetEventVirtcon) {
  auto request = fuchsia_ui_input_internal::InputOwnershipGetEventRequest(
      fuchsia_ui_input_internal::InputOwnershipTarget::WithVirtcon({}));

  auto res = GetInputOwnershipEvent(std::move(request));
  EXPECT_EQ(res.status, ZX_OK);
  ASSERT_TRUE(res.result.has_value());
  ASSERT_TRUE(res.result->is_ok());
  const zx::event& event = res.result->value().ownership_event();
  EXPECT_EQ(fsl::GetKoid(event.get()), fsl::GetKoid(display_->ownership_event().get()));

  zx_info_handle_basic_t info;
  ASSERT_EQ(event.get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr), ZX_OK);
  EXPECT_EQ(info.rights & ZX_RIGHT_SIGNAL, 0u);
  EXPECT_NE(info.rights & ZX_RIGHT_WAIT, 0u);
  EXPECT_NE(info.rights & ZX_RIGHT_TRANSFER, 0u);
  EXPECT_NE(info.rights & ZX_RIGHT_DUPLICATE, 0u);
  EXPECT_NE(info.rights & ZX_RIGHT_INSPECT, 0u);
}

TEST_F(SingletonDisplayServiceTest, InputOwnershipGetEventPlatform) {
  auto request = fuchsia_ui_input_internal::InputOwnershipGetEventRequest(
      fuchsia_ui_input_internal::InputOwnershipTarget::WithPlatform({}));

  auto res = GetInputOwnershipEvent(std::move(request));
  EXPECT_EQ(res.status, ZX_OK);
  ASSERT_TRUE(res.result.has_value());
  ASSERT_TRUE(res.result->is_ok());
  const zx::event& event = res.result->value().ownership_event();
  EXPECT_EQ(fsl::GetKoid(event.get()), fsl::GetKoid(display_->ownership_event().get()));

  zx_info_handle_basic_t info;
  ASSERT_EQ(event.get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr), ZX_OK);
  EXPECT_EQ(info.rights & ZX_RIGHT_SIGNAL, 0u);
}

TEST_F(SingletonDisplayServiceTest, InputOwnershipGetEventValidViewRef) {
  zx::eventpair ep1, ep2;
  ASSERT_EQ(zx::eventpair::create(0, &ep1, &ep2), ZX_OK);
  const zx_koid_t expected_koid = fsl::GetKoid(ep1.get());

  fuchsia_ui_views::ViewRef view_ref;
  view_ref.reference(std::move(ep1));

  std::optional<zx_koid_t> registered_koid;
  singleton_->SetOnViewRefRegisteredCallback([&](zx_koid_t koid) { registered_koid = koid; });

  auto request = fuchsia_ui_input_internal::InputOwnershipGetEventRequest(
      fuchsia_ui_input_internal::InputOwnershipTarget::WithViewRef(std::move(view_ref)));

  auto res = GetInputOwnershipEvent(std::move(request));
  EXPECT_EQ(res.status, ZX_OK);
  ASSERT_TRUE(res.result.has_value());
  ASSERT_TRUE(res.result->is_ok());
  EXPECT_EQ(registered_koid, expected_koid);

  const zx::event& event = res.result->value().ownership_event();
  EXPECT_EQ(fsl::GetKoid(event.get()), fsl::GetKoid(display_->ownership_event().get()));

  zx_info_handle_basic_t info;
  ASSERT_EQ(event.get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr), ZX_OK);
  EXPECT_EQ(info.rights & ZX_RIGHT_SIGNAL, 0u);
}

TEST_F(SingletonDisplayServiceTest, InputOwnershipGetEventValidViewRefNoCallback) {
  zx::eventpair ep1, ep2;
  ASSERT_EQ(zx::eventpair::create(0, &ep1, &ep2), ZX_OK);

  fuchsia_ui_views::ViewRef view_ref;
  view_ref.reference(std::move(ep1));

  // No callback registered via SetOnViewRefRegisteredCallback().
  auto request = fuchsia_ui_input_internal::InputOwnershipGetEventRequest(
      fuchsia_ui_input_internal::InputOwnershipTarget::WithViewRef(std::move(view_ref)));

  auto res = GetInputOwnershipEvent(std::move(request));
  EXPECT_EQ(res.status, ZX_OK);
  ASSERT_TRUE(res.result.has_value());
  ASSERT_TRUE(res.result->is_ok());

  const zx::event& event = res.result->value().ownership_event();
  EXPECT_EQ(fsl::GetKoid(event.get()), fsl::GetKoid(display_->ownership_event().get()));
}

TEST_F(SingletonDisplayServiceTest, InputOwnershipGetEventPeerClosedViewRef) {
  zx::eventpair ep1, ep2;
  ASSERT_EQ(zx::eventpair::create(0, &ep1, &ep2), ZX_OK);
  ep2.reset();  // Close peer so ZX_EVENTPAIR_PEER_CLOSED is asserted.

  fuchsia_ui_views::ViewRef view_ref;
  view_ref.reference(std::move(ep1));

  bool callback_invoked = false;
  singleton_->SetOnViewRefRegisteredCallback([&](zx_koid_t) { callback_invoked = true; });

  auto request = fuchsia_ui_input_internal::InputOwnershipGetEventRequest(
      fuchsia_ui_input_internal::InputOwnershipTarget::WithViewRef(std::move(view_ref)));

  auto res = GetInputOwnershipEvent(std::move(request));
  EXPECT_EQ(res.status, ZX_OK);
  ASSERT_TRUE(res.result.has_value());
  ASSERT_TRUE(res.result->is_error());
  EXPECT_EQ(res.result->error_value(),
            fuchsia_ui_input_internal::InputOwnershipError::kUnknownView);
  EXPECT_FALSE(callback_invoked);
}

TEST_F(SingletonDisplayServiceTest, InputOwnershipGetEventKoidInvalidViewRef) {
  // An uninitialized ViewRef has an invalid handle (ZX_HANDLE_INVALID), causing fsl::GetKoid to
  // return ZX_KOID_INVALID.
  fuchsia_ui_views::ViewRef view_ref;

  bool callback_invoked = false;
  singleton_->SetOnViewRefRegisteredCallback([&](zx_koid_t) { callback_invoked = true; });

  auto request = fuchsia_ui_input_internal::InputOwnershipGetEventRequest(
      fuchsia_ui_input_internal::InputOwnershipTarget::WithViewRef(std::move(view_ref)));

  auto res = GetInputOwnershipEvent(std::move(request));
  EXPECT_EQ(res.status, ZX_OK);
  ASSERT_TRUE(res.result.has_value());
  ASSERT_TRUE(res.result->is_error());
  EXPECT_EQ(res.result->error_value(),
            fuchsia_ui_input_internal::InputOwnershipError::kUnknownView);
  EXPECT_FALSE(callback_invoked);
}

TEST_F(SingletonDisplayServiceTest, InputOwnershipGetEventUnknownTarget) {
  fuchsia_ui_input_internal::InputOwnershipTarget target(
      fidl::internal::DefaultConstructPossiblyInvalidObjectTag{});
  auto request = fuchsia_ui_input_internal::InputOwnershipGetEventRequest(std::move(target));

  auto res = GetInputOwnershipEvent(std::move(request));
  EXPECT_EQ(res.status, ZX_OK);
  ASSERT_TRUE(res.result.has_value());
  ASSERT_TRUE(res.result->is_error());
  EXPECT_EQ(res.result->error_value(),
            fuchsia_ui_input_internal::InputOwnershipError::kInvalidTarget);
}

TEST_F(SingletonDisplayServiceTest, InputOwnershipGetEventDuplicationFailure) {
  // Replace the display's ownership event with an invalid handle so duplicate() fails.
  display_->set_ownership_event_for_testing(zx::event());

  zx::eventpair ep1, ep2;
  ASSERT_EQ(zx::eventpair::create(0, &ep1, &ep2), ZX_OK);

  fuchsia_ui_views::ViewRef view_ref;
  view_ref.reference(std::move(ep1));

  bool callback_invoked = false;
  singleton_->SetOnViewRefRegisteredCallback([&](zx_koid_t) { callback_invoked = true; });

  auto request = fuchsia_ui_input_internal::InputOwnershipGetEventRequest(
      fuchsia_ui_input_internal::InputOwnershipTarget::WithViewRef(std::move(view_ref)));

  auto res = GetInputOwnershipEvent(std::move(request));
  EXPECT_NE(res.status, ZX_OK);
  EXPECT_FALSE(res.result.has_value());
  EXPECT_FALSE(callback_invoked);
}

}  // namespace display::test
