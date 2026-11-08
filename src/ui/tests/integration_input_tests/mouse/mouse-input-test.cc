// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.accessibility.semantics/cpp/fidl.h>
#include <fidl/fuchsia.ui.composition/cpp/fidl.h>
#include <fidl/fuchsia.ui.test.input/cpp/fidl.h>
#include <fuchsia/web/cpp/fidl.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>

#include <cstddef>
#include <cstdint>
#include <queue>
#include <string>
#include <utility>
#include <vector>

#include <gtest/gtest.h>

#include "src/lib/json_parser/json_parser.h"
#include "src/ui/testing/util/portable_ui_test.h"
#include "src/ui/testing/util/web_view_client.h"

namespace {

using component_testing::Protocol;

// Maximum pointer movement during a clickpad press for the gesture to
// be guaranteed to be interpreted as a click. For movement greater than
// this value, upper layers may, e.g., interpret the gesture as a drag.
//
// This value corresponds to the one used to instantiate the ClickDragHandler
// registered by Input Pipeline in Scene Manager.
constexpr int64_t kClickToDragThreshold = 16.0;

int ButtonsToInt(const std::vector<fuchsia_ui_test_input::MouseButton>& buttons) {
  int result = 0;
  for (const auto& button : buttons) {
    result |= (0x1 >> static_cast<uint32_t>(button));
  }

  return result;
}

std::vector<fuchsia_ui_test_input::MouseButton> GetPressedButtons(int buttons) {
  std::vector<fuchsia_ui_test_input::MouseButton> pressed_buttons;

  if (buttons & 0x1) {
    pressed_buttons.push_back(fuchsia_ui_test_input::MouseButton::kFirst);
  }
  if (buttons & 0x1 >> 1) {
    pressed_buttons.push_back(fuchsia_ui_test_input::MouseButton::kSecond);
  }
  if (buttons & 0x1 >> 2) {
    pressed_buttons.push_back(fuchsia_ui_test_input::MouseButton::kThird);
  }

  return pressed_buttons;
}

fuchsia_ui_test_input::MouseEventPhase GetPhase(const std::string& type) {
  if (type == "add") {
    return fuchsia_ui_test_input::MouseEventPhase::kAdd;
  }
  if (type == "hover") {
    return fuchsia_ui_test_input::MouseEventPhase::kHover;
  }
  if (type == "mousedown") {
    return fuchsia_ui_test_input::MouseEventPhase::kDown;
  }
  if (type == "mousemove") {
    return fuchsia_ui_test_input::MouseEventPhase::kMove;
  }
  if (type == "mouseup") {
    return fuchsia_ui_test_input::MouseEventPhase::kUp;
  }
  if (type == "wheel") {
    return fuchsia_ui_test_input::MouseEventPhase::kWheel;
  }
  FX_LOGS(FATAL) << "Invalid mouse event type: " << type;
  __builtin_unreachable();
}

struct Position {
  double x = 0.0;
  double y = 0.0;
};

class ChromiumInputTest : public ui_testing::PortableUITest {
 protected:
  std::string GetTestUIStackUrl() override { return "#meta/test-ui-stack.cm"; }

  // Use a DPR other than 1.0, so that logical and physical coordinate spaces
  // are different.
  float device_pixel_ratio() override { return 2.f; }

  void SetUp() override {
    ui_testing::PortableUITest::SetUp();

    RegisterMouse();

    LaunchClient();
    ASSERT_NE(web_client_, nullptr);
    ASSERT_TRUE(web_client_->is_started());

    web_client_->LoadUrl("about:blank");
    RunLoopUntil([this] { return web_client_->is_main_document_loaded(); });

    bool port_registered = false;
    web_client_->RegisterMessagePort([&port_registered] { port_registered = true; });
    RunLoopUntil(
        [this, &port_registered] { return port_registered && web_client_->is_window_resized(); });

    bool js_loaded = false;
    web_client_->ExecuteJavaScript(kAppCode, [&js_loaded] { js_loaded = true; });
    RunLoopUntil([&js_loaded] { return js_loaded; });

    ReceiveNextMouseEvent();
  }

  void TearDown() override {
    web_client_ = nullptr;
    ui_testing::PortableUITest::TearDown();

    // At the end of test, ensure event queue is empty.
    ASSERT_EQ(events_.size(), 0u);
  }

  size_t SizeOfEvents() const { return events_.size(); }

  fuchsia_ui_test_input::MouseInputListenerReportMouseInputRequest PopEvent() {
    auto e = std::move(events_.front());
    events_.pop();
    return e;
  }

  const fuchsia_ui_test_input::MouseInputListenerReportMouseInputRequest& LastEvent() const {
    return events_.back();
  }

  void ClearEvents() { events_ = {}; }

  static void VerifyEvent(
      fuchsia_ui_test_input::MouseInputListenerReportMouseInputRequest& pointer_data,
      double expected_x, double expected_y,
      const std::vector<fuchsia_ui_test_input::MouseButton>& expected_buttons,
      const fuchsia_ui_test_input::MouseEventPhase expected_phase) {
    FX_LOGS(INFO) << "Client received mouse change at (" << pointer_data.local_x().value() << ", "
                  << pointer_data.local_y().value() << ") with buttons "
                  << ButtonsToInt(pointer_data.buttons().value()) << ".";
    FX_LOGS(INFO) << "Expected mouse change is at approximately (" << expected_x << ", "
                  << expected_y << ") with buttons " << ButtonsToInt(expected_buttons) << ".";

    // Allow for minor rounding differences in coordinates.
    // Note: These approximations don't account for `PointerMotionDisplayScaleHandler`
    // or `PointerMotionSensorScaleHandler`. We will need to do so in order to validate
    // larger motion or different sized displays.
    EXPECT_NEAR(pointer_data.local_x().value(), expected_x, 1);
    EXPECT_NEAR(pointer_data.local_y().value(), expected_y, 1);
    EXPECT_EQ(pointer_data.buttons().value(), expected_buttons);
    EXPECT_EQ(pointer_data.phase().value(), expected_phase);
  }

  static void VerifyEventLocationOnTheRightOfExpectation(
      fuchsia_ui_test_input::MouseInputListenerReportMouseInputRequest& pointer_data,
      double expected_x_min, double expected_y,
      const std::vector<fuchsia_ui_test_input::MouseButton>& expected_buttons,
      const fuchsia_ui_test_input::MouseEventPhase expected_phase) {
    FX_LOGS(INFO) << "Client received mouse change at (" << pointer_data.local_x().value() << ", "
                  << pointer_data.local_y().value() << ") with buttons "
                  << ButtonsToInt(pointer_data.buttons().value()) << ".";
    FX_LOGS(INFO) << "Expected mouse change is at approximately (>" << expected_x_min << ", "
                  << expected_y << ") with buttons " << ButtonsToInt(expected_buttons) << ".";

    EXPECT_GT(pointer_data.local_x().value(), expected_x_min);
    EXPECT_NEAR(pointer_data.local_y().value(), expected_y, 1);
    EXPECT_EQ(pointer_data.buttons().value(), expected_buttons);
    EXPECT_EQ(pointer_data.phase().value(), expected_phase);
  }

  // TODO(https://fxbug.dev/42136267): EnsureMouseIsReadyAndGetPosition will send a mouse click
  // (down and up) and wait for response to ensure the mouse is ready to use. We will retry a
  // mouse click if we can not get the mouseup response in small timeout. This function returns
  // the cursor position in WebEngine coordinate system.
  Position EnsureMouseIsReadyAndGetPosition() {
    for (int retry = 0; retry < kMaxRetry; retry++) {
      // Mouse down and up.
      SimulateMouseEvent(/* pressed_buttons = */ {fuchsia_ui_test_input::MouseButton::kFirst},
                         /* movement_x = */ 0, /* movement_y = */ 0);
      SimulateMouseEvent(/* pressed_buttons = */ {}, /* movement_x = */ 0, /* movement_y = */ 0);

      auto wait_until_last_event_phase_or_timeout =
          [this](fuchsia_ui_test_input::MouseEventPhase event_phase) {
            return RunLoopWithTimeoutOrUntil(
                [this, event_phase] {
                  return SizeOfEvents() > 0 && LastEvent().phase() == event_phase;
                },
                kFirstEventRetryInterval);
          };

      bool got_mouse_up =
          wait_until_last_event_phase_or_timeout(fuchsia_ui_test_input::MouseEventPhase::kUp);

      if (got_mouse_up) {
        // There is an issue we found in retry that the mouse up we got may
        // come from previous retry loop, then EnsureMouseIsReadyAndGetPosition
        // exit and the down-up event caught by test body, and break the test
        // expectation. Here we inject 1 more wheel event, because wheel event
        // only injected once, when we receive the wheel event, we know all
        // events from EnsureMouseIsReadyAndGetPosition are processed.
        SimulateMouseScroll(/* pressed_buttons = */ {}, /* scroll_x = */ 0, /* scroll_y = */ 1);
        wait_until_last_event_phase_or_timeout(fuchsia_ui_test_input::MouseEventPhase::kWheel);
        Position p;
        p.x = LastEvent().local_x().value();
        p.y = LastEvent().local_y().value();
        ClearEvents();
        return p;
      }
    }

    FX_LOGS(FATAL) << "Can not get mouse click in max retries " << kMaxRetry;
    return Position{};
  }

 private:
  static constexpr auto kAppCode = R"JS(
    function reportMouseEvent(event) {
      console.assert(port != null);
      let response = JSON.stringify({
        type: event.type,
        epoch_msec: Date.now(),
        x: event.clientX,
        y: event.clientY,
        wheel_h: event.deltaX,
        wheel_v: event.deltaY,
        device_scale_factor: window.devicePixelRatio,
        buttons: event.buttons
      });
      console.info('Reporting mouse event ', response);
      port.postMessage(response);
    }
    document.body.onmousemove = reportMouseEvent;
    document.body.onmousedown = reportMouseEvent;
    document.body.onmouseup = reportMouseEvent;
    document.body.onwheel = reportMouseEvent;
  )JS";

  void ExtendRealm() override {
    ui_testing::WebViewClient::AddToRealm(
        realm_builder().root(), dispatcher(), &web_client_,
        fuchsia::web::ContextFeatureFlags::VULKAN | fuchsia::web::ContextFeatureFlags::NETWORK);
    realm_builder().AddRoute(
        {.capabilities =
             {
                 Protocol{fidl::DiscoverableProtocolName<
                     fuchsia_accessibility_semantics::SemanticsManager>},
                 Protocol{fidl::DiscoverableProtocolName<fuchsia_ui_composition::Allocator>},
                 Protocol{fidl::DiscoverableProtocolName<fuchsia_ui_composition::Flatland>},
             },
         .source = kTestUIStackRef,
         .targets = {ui_testing::WebViewClient::kChildRef}});
  }

  void ReceiveNextMouseEvent() {
    if (!web_client_) {
      return;
    }
    web_client_->ReceiveMessage([this](std::string message) {
      rapidjson::Document mouse_resp =
          json::JSONParser().ParseFromString(message, "web-mouse-response");
      FX_CHECK(!mouse_resp.HasParseError()) << mouse_resp.GetParseError();
      FX_CHECK(mouse_resp.HasMember("type") && mouse_resp["type"].IsString());
      FX_CHECK(mouse_resp.HasMember("epoch_msec") && mouse_resp["epoch_msec"].IsInt64());
      FX_CHECK(mouse_resp.HasMember("x") && mouse_resp["x"].IsNumber());
      FX_CHECK(mouse_resp.HasMember("y") && mouse_resp["y"].IsNumber());
      FX_CHECK(mouse_resp.HasMember("device_scale_factor") &&
               mouse_resp["device_scale_factor"].IsNumber());
      FX_CHECK(mouse_resp.HasMember("buttons") && mouse_resp["buttons"].IsNumber());

      fuchsia_ui_test_input::MouseInputListenerReportMouseInputRequest request({
          .local_x = mouse_resp["x"].GetDouble(),
          .local_y = mouse_resp["y"].GetDouble(),
          .time_received = mouse_resp["epoch_msec"].GetInt64() * 1000 * 1000,
          .buttons = GetPressedButtons(mouse_resp["buttons"].GetInt()),
          .phase = GetPhase(mouse_resp["type"].GetString()),
          .device_pixel_ratio = mouse_resp["device_scale_factor"].GetDouble(),
      });

      if (mouse_resp.HasMember("wheel_h")) {
        FX_CHECK(mouse_resp["wheel_h"].IsNumber());
        request.wheel_x_physical_pixel() = mouse_resp["wheel_h"].GetDouble();
      }
      if (mouse_resp.HasMember("wheel_v")) {
        FX_CHECK(mouse_resp["wheel_v"].IsNumber());
        request.wheel_y_physical_pixel() = mouse_resp["wheel_v"].GetDouble();
      }

      events_.push(std::move(request));
      ReceiveNextMouseEvent();
    });
  }

  static constexpr auto kFirstEventRetryInterval = zx::sec(1);
  static const int kMaxRetry = 10;

  ui_testing::WebViewClient* web_client_ = nullptr;
  std::queue<fuchsia_ui_test_input::MouseInputListenerReportMouseInputRequest> events_;
};

TEST_F(ChromiumInputTest, ChromiumMouseMove) {
  auto initial_position = EnsureMouseIsReadyAndGetPosition();

  double initial_x = initial_position.x;
  double initial_y = initial_position.y;

  SimulateMouseEvent(/* pressed_buttons = */ {},
                     /* movement_x = */ 5, /* movement_y = */ 0);
  RunLoopUntil([this] { return SizeOfEvents() == 1; });

  auto event_move = PopEvent();

  VerifyEventLocationOnTheRightOfExpectation(
      event_move,
      /*expected_x_min=*/initial_x,
      /*expected_y=*/initial_y,
      /*expected_buttons=*/{},
      /*expected_phase=*/fuchsia_ui_test_input::MouseEventPhase::kMove);
}

TEST_F(ChromiumInputTest, DISABLED_ChromiumMouseDownMoveUp) {
  auto initial_position = EnsureMouseIsReadyAndGetPosition();

  double initial_x = initial_position.x;
  double initial_y = initial_position.y;

  SimulateMouseEvent(/* pressed_buttons = */ {fuchsia_ui_test_input::MouseButton::kFirst},
                     /* movement_x = */ 0, /* movement_y = */ 0);
  SimulateMouseEvent(/* pressed_buttons = */ {fuchsia_ui_test_input::MouseButton::kFirst},
                     /* movement_x = */ kClickToDragThreshold, /* movement_y = */ 0);
  SimulateMouseEvent(/* pressed_buttons = */ {}, /* movement_x = */ 0, /* movement_y = */ 0);

  RunLoopUntil([this] { return SizeOfEvents() == 3; });

  auto event_down = PopEvent();
  auto event_move = PopEvent();
  auto event_up = PopEvent();

  VerifyEvent(event_down,
              /*expected_x=*/initial_x,
              /*expected_y=*/initial_y,
              /*expected_buttons=*/{fuchsia_ui_test_input::MouseButton::kFirst},
              /*expected_phase=*/fuchsia_ui_test_input::MouseEventPhase::kDown);
  VerifyEventLocationOnTheRightOfExpectation(
      event_move,
      /*expected_x_min=*/initial_x,
      /*expected_y=*/initial_y,
      /*expected_buttons=*/{fuchsia_ui_test_input::MouseButton::kFirst},
      /*expected_phase=*/fuchsia_ui_test_input::MouseEventPhase::kMove);
  VerifyEvent(event_up,
              /*expected_x=*/event_move.local_x().value(),
              /*expected_y=*/initial_y,
              /*expected_buttons=*/{},
              /*expected_phase=*/fuchsia_ui_test_input::MouseEventPhase::kUp);
}

TEST_F(ChromiumInputTest, DISABLED_ChromiumMouseWheel) {
  auto initial_position = EnsureMouseIsReadyAndGetPosition();

  double initial_x = initial_position.x;
  double initial_y = initial_position.y;

  SimulateMouseScroll(/* pressed_buttons = */ {}, /* scroll_x = */ 1, /* scroll_y = */ 0);
  RunLoopUntil([this] { return SizeOfEvents() == 1; });

  auto event_wheel_h = PopEvent();

  VerifyEvent(event_wheel_h,
              /*expected_x=*/initial_x,
              /*expected_y=*/initial_y,
              /*expected_buttons=*/{},
              /*expected_phase=*/fuchsia_ui_test_input::MouseEventPhase::kWheel);
  // Chromium will scale the count of ticks to pixel.
  // Positive delta in Fuchsia means scroll left, and scroll left in JS is negative delta.
  EXPECT_LT(event_wheel_h.wheel_x_physical_pixel(), 0);
  EXPECT_EQ(event_wheel_h.wheel_y_physical_pixel(), 0);

  SimulateMouseScroll(/* pressed_buttons = */ {}, /* scroll_x = */ 0, /* scroll_y = */ 1);
  RunLoopUntil([this] { return SizeOfEvents() == 1; });

  auto event_wheel_v = PopEvent();

  VerifyEvent(event_wheel_v,
              /*expected_x=*/initial_x,
              /*expected_y=*/initial_y,
              /*expected_buttons=*/{},
              /*expected_phase=*/fuchsia_ui_test_input::MouseEventPhase::kWheel);
  // Chromium will scale the count of ticks to pixel.
  // Positive delta in Fuchsia means scroll up, and scroll up in JS is negative delta.
  EXPECT_LT(event_wheel_v.wheel_y_physical_pixel(), 0);
  EXPECT_EQ(event_wheel_v.wheel_x_physical_pixel(), 0);
}

}  // namespace
