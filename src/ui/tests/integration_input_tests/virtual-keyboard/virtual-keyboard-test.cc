// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.accessibility.semantics/cpp/fidl.h>
#include <fidl/fuchsia.input.injection/cpp/fidl.h>
#include <fidl/fuchsia.input.virtualkeyboard/cpp/fidl.h>
#include <fidl/fuchsia.ui.composition/cpp/fidl.h>
#include <fidl/fuchsia.ui.input/cpp/fidl.h>
#include <fidl/fuchsia.ui.input3/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/fidl/cpp/channel.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/errors.h>
#include <zircon/types.h>
#include <zircon/utc.h>

#include <iostream>

#include <gtest/gtest.h>
#include <src/lib/testing/loop_fixture/real_loop_fixture.h>
#include <src/lib/timekeeper/clock.h>
#include <src/ui/testing/util/fidl_cpp_helpers.h>
#include <src/ui/testing/util/portable_ui_test.h>
#include <src/ui/testing/util/web_view_client.h>

#include "src/lib/json_parser/json_parser.h"

// This test exercises the virtual keyboard visibility interactions between
// Chromium and Virtual Keyboard Manager. It is a multi-component test, and
// carefully avoids sleeping or polling for component coordination.
// - It runs real Virtual Keyboard Manager, Scene Manager and Scenic components.
// - It uses a fake display controller; the physical device is unused.
//
// Components involved
// - This test program
// - Virtual Keyboard Manager
// - Scene Manager (serves root view and touch input)
// - Scenic
// - WebEngine (built from Chromium)
//
// Setup sequence
// - The test sets up a view hierarchy with two views:
//   - Top level scene, owned by Scene Manager.
//   - Bottom view, owned by Chromium.

namespace {

// Types imported for the realm_builder library.
using component_testing::ChildRef;
using component_testing::Protocol;
using component_testing::Route;

// Alias for Component child name as provided to Realm Builder.
using ChildName = std::string;

struct BoundingBox {
  uint32_t x0 = 0;
  uint32_t y0 = 0;
  uint32_t x1 = 0;
  uint32_t y1 = 0;
};

class VirtualKeyboardTest : public ui_testing::PortableUITest {
 protected:
  void SetUp() override {
    ui_testing::PortableUITest::SetUp();
    RegisterTouchScreen();
  }

  void LaunchWebEngineClient() {
    LaunchClient();
    ASSERT_NE(web_client_, nullptr);

    web_client_->LoadUrl("about:blank");
    RunLoopUntil([this]() { return web_client_->is_main_document_loaded(); });

    bool is_port_registered = false;
    web_client_->RegisterMessagePort([&is_port_registered]() { is_port_registered = true; });
    RunLoopUntil([&is_port_registered]() { return is_port_registered; });

    RunLoopUntil([this]() { return web_client_->is_window_resized(); });

    web_client_->ReceiveMessage([this](std::string message) {
      rapidjson::Document input_pos =
          json::JSONParser().ParseFromString(message, "web-virtual-keyboard-response");
      FX_CHECK(!input_pos.HasParseError()) << input_pos.GetParseError();
      for (const auto& element : {"left", "right", "top", "bottom"}) {
        FX_CHECK(input_pos.HasMember(element)) << "HasMember failed for " << element;
        FX_CHECK(input_pos[element].IsNumber()) << "IsNumber failed for " << element;
      }
      input_position_ = BoundingBox{
          .x0 = static_cast<uint32_t>(input_pos["left"].GetFloat()),
          .y0 = static_cast<uint32_t>(input_pos["top"].GetFloat()),
          .x1 = static_cast<uint32_t>(input_pos["right"].GetFloat()),
          .y1 = static_cast<uint32_t>(input_pos["bottom"].GetFloat()),
      };
    });

    bool is_js_executed = false;
    web_client_->ExecuteJavaScript(kAppCode, [&is_js_executed]() { is_js_executed = true; });
    RunLoopUntil([&is_js_executed]() { return is_js_executed; });

    FX_LOGS(INFO) << "Waiting on input box position";
    RunLoopUntil([this]() { return input_position_.has_value(); });
  }

  std::string GetTestUIStackUrl() override { return "#meta/test-ui-stack.cm"; }

  void ExtendRealm() override {
    ui_testing::WebViewClient::AddToRealm(realm_builder().root(), dispatcher(), &web_client_,
                                          fuchsia::web::ContextFeatureFlags::VULKAN |
                                              fuchsia::web::ContextFeatureFlags::NETWORK |
                                              fuchsia::web::ContextFeatureFlags::KEYBOARD |
                                              fuchsia::web::ContextFeatureFlags::VIRTUAL_KEYBOARD);
  }

  std::vector<Route> GetTestRoutes() override {
    return {
        {.capabilities =
             {
                 Protocol{fidl::DiscoverableProtocolName<
                     fuchsia_accessibility_semantics::SemanticsManager>},
                 Protocol{fidl::DiscoverableProtocolName<fuchsia_ui_input3::Keyboard>},
                 Protocol{fidl::DiscoverableProtocolName<fuchsia_ui_composition::Allocator>},
                 Protocol{fidl::DiscoverableProtocolName<fuchsia_ui_composition::Flatland>},
                 Protocol{fidl::DiscoverableProtocolName<fuchsia_ui_input::ImeService>},
                 Protocol{fidl::DiscoverableProtocolName<fuchsia_input_virtualkeyboard::Manager>},
                 Protocol{fidl::DiscoverableProtocolName<
                     fuchsia_input_virtualkeyboard::ControllerCreator>},
             },
         .source = kTestUIStackRef,
         .targets = {ui_testing::WebViewClient::kChildRef}},
    };
  }

  const std::optional<BoundingBox>& input_position() const { return input_position_; }

 private:
  static constexpr std::string_view kAppCode = R"JS(
    console.info('injecting body');
    // Create a page with a single input box.
    // * When the user taps inside the input box (and the keyboard is currently hidden),
    //   web-engine should request the virtual keyboard be made visible.
    // * When the user taps outside the input box (and the keyboard is currently visible),
    //   web-engine should request the virtual keyboard me made hidden.
    document.body.innerHTML = '<input id="textbox" />';
    document.body.style.backgroundColor='#ff00ff';
    document.body.onclick = function(event) {
      document.body.style.backgroundColor='#40e0d0';
      let touch_event = JSON.stringify({
        x: event.screenX,
        y: event.screenY,
      });
      console.info('Got touch event ', touch_event);
    };

    waitForVisible("textbox").then((element) => {
      let message = JSON.stringify(element.getBoundingClientRect());
      console.info('sending input position ', message);
      port.postMessage(message);
    });
  )JS";

  ui_testing::WebViewClient* web_client_ = nullptr;
  std::optional<BoundingBox> input_position_;
};

TEST_F(VirtualKeyboardTest, ShowAndHideKeyboard) {
  LaunchWebEngineClient();

  std::optional<bool> is_keyboard_visible;
  auto virtualkeyboard_manager_connect =
      realm_root()->component().Connect<fuchsia_input_virtualkeyboard::Manager>();
  ZX_ASSERT_OK(virtualkeyboard_manager_connect);
  auto virtualkeyboard_manager =
      fidl::Client(std::move(virtualkeyboard_manager_connect.value()), dispatcher());

  virtualkeyboard_manager->WatchTypeAndVisibility().Then(
      [&is_keyboard_visible](auto& res) { is_keyboard_visible = res->is_visible(); });
  RunLoopUntil([&]() { return is_keyboard_visible.has_value(); });
  // virtual keyboard should not be visible before tap.
  ASSERT_FALSE(is_keyboard_visible.value());
  is_keyboard_visible.reset();

  FX_LOGS(INFO) << "Tapping _inside_ input box";
  auto input_pos = *input_position();
  int32_t input_center_x_local = static_cast<int32_t>((input_pos.x0 + input_pos.x1) / 2);
  int32_t input_center_y_local = static_cast<int32_t>((input_pos.y0 + input_pos.y1) / 2);

  // TODO(b/477639582): Repeating the tap may not be necessary, but adding
  // it for now to stable the test after chromium 145.0.7622.0 roll.
  virtualkeyboard_manager->WatchTypeAndVisibility().Then(
      [&is_keyboard_visible](auto& res) { is_keyboard_visible = res->is_visible(); });

  const int kMaxRetries = 10;

  for (int i = 0; i < kMaxRetries; i++) {
    FX_LOGS(INFO) << "Tapping: " << i + 1 << "/" << kMaxRetries;
    InjectTap(input_center_x_local, input_center_y_local);
    FX_LOGS(INFO) << "Waiting for keyboard to be visible";
    RunLoopWithTimeoutOrUntil([&]() { return is_keyboard_visible.has_value(); }, zx::sec(1));
    if (is_keyboard_visible.has_value()) {
      FX_LOGS(INFO) << "Keyboard is visible: tapped " << i + 1 << " times";
      break;
    }
  }

  EXPECT_TRUE(is_keyboard_visible.value());
  is_keyboard_visible.reset();

  FX_LOGS(INFO) << "Tapping _outside_ input box";
  virtualkeyboard_manager->WatchTypeAndVisibility().Then(
      [&is_keyboard_visible](auto& res) { is_keyboard_visible = res->is_visible(); });

  for (int i = 0; i < kMaxRetries; i++) {
    FX_LOGS(INFO) << "Tapping outside: " << i + 1 << "/" << kMaxRetries;
    InjectTap(static_cast<int32_t>(input_pos.x1 + 1), static_cast<int32_t>(input_pos.y1 + 1));
    FX_LOGS(INFO) << "Waiting for keyboard to be hidden";
    RunLoopWithTimeoutOrUntil([&]() { return is_keyboard_visible.has_value(); }, zx::sec(1));
    if (is_keyboard_visible.has_value()) {
      FX_LOGS(INFO) << "Keyboard is hidden: tapped " << i + 1 << " times";
      break;
    }
  }

  EXPECT_FALSE(is_keyboard_visible.value());
}

}  // namespace
