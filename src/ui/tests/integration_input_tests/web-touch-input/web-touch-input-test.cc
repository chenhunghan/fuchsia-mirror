// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.accessibility.semantics/cpp/fidl.h>
#include <fidl/fuchsia.ui.composition/cpp/fidl.h>
#include <fidl/fuchsia.ui.input/cpp/fidl.h>
#include <fuchsia/web/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/stdcompat/string_view.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>

#include <string>
#include <tuple>
#include <utility>
#include <vector>

#include <gtest/gtest.h>

#include "src/lib/json_parser/json_parser.h"
#include "src/ui/testing/util/portable_ui_test.h"
#include "src/ui/testing/util/web_view_client.h"

namespace {

using component_testing::ChildRef;
using component_testing::Protocol;
using ui_testing::ChildName;

// Max timeout in failure cases.
// Set this as low as you can that still works across all test platforms.
constexpr zx::duration kTimeout = zx::min(5);

// Maximum distance between two physical pixel coordinates so that they are considered equal.
constexpr double kEpsilon = 0.5f;

std::vector<float> ConfigsToTest() {
  std::vector<float> configs;
  // TODO: https://fxbug.dev/42082519 - Test for DPR=2.0, too.
  configs.push_back(2.f);
  return configs;
}

template <typename T>
std::vector<std::tuple<T>> AsTuples(std::vector<T> v) {
  std::vector<std::tuple<T>> result;
  for (const auto& elt : v) {
    result.push_back(elt);
  }

  return result;
}

enum class TapLocation { kTopLeft, kTopRight };

struct TouchEvent {
  int local_x = 0;
  int local_y = 0;
  double device_pixel_ratio = 1.0;
};

class WebEngineTest : public ui_testing::PortableUITest,
                      public testing::WithParamInterface<std::tuple<float>> {
 protected:
  std::string GetTestUIStackUrl() override { return "#meta/test-ui-stack.cm"; }

  void SetUp() override {
    ui_testing::PortableUITest::SetUp();

    // Post a "just in case" quit task, if the test hangs.
    async::PostDelayedTask(
        dispatcher(),
        [] { FX_LOGS(FATAL) << "\n\n>> Test did not complete in time, terminating.  <<\n\n"; },
        kTimeout);

    // Register input injection device.
    FX_LOGS(INFO) << "Registering input injection device";
    RegisterTouchScreen();

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
    RunLoopUntil([this, &js_loaded] {
      return js_loaded && cpp23::contains(web_client_->title(), "input_ready");
    });

    ReceiveNextTouchEvent();

    // TODO(b/477639582): Repeating the tap may not be necessary, but adding
    // it for now to stabilize the test after chromium 145.0.7622.0 roll.
    const int kMaxRetries = 10;
    FX_LOGS(INFO) << "Waiting on app to be ready for input";
    for (int i = 0; i < kMaxRetries; i++) {
      FX_LOGS(INFO) << "Tapping: " << i + 1 << "/" << kMaxRetries;
      InjectTap(display_width() / 2, display_height() / 2);

      RunLoopWithTimeoutOrUntil([this] { return !events_received_.empty(); }, zx::sec(1));
      if (!events_received_.empty()) {
        FX_LOGS(INFO) << "App is ready for input events: tapped " << i + 1 << " times";
        break;
      }
    }
    ASSERT_FALSE(events_received_.empty());
    events_received_.clear();
  }

  void TearDown() override {
    web_client_ = nullptr;
    ui_testing::PortableUITest::TearDown();
  }

  std::vector<std::pair<ChildName, std::string>> GetTestComponents() override {
    return {
        std::make_pair(kTextManager, kTextManagerUrl),
    };
  }

  void ExpectLocation(const TouchEvent& e, float expected_x, float expected_y) {
    auto actual_x = e.device_pixel_ratio * static_cast<double>(e.local_x);
    auto actual_y = e.device_pixel_ratio * static_cast<double>(e.local_y);

    EXPECT_NEAR(expected_x, actual_x, kEpsilon);
    EXPECT_NEAR(expected_y, actual_y, kEpsilon);
  }

  void InjectInput(TapLocation tap_location) {
    // The /config/data/display_rotation (90) specifies how many degrees to rotate the
    // presentation child view, counter-clockwise, in a right-handed coordinate system. Thus,
    // the user observes the child view to rotate *clockwise* by that amount (90).
    //
    // Hence, a tap in the center of the display's top-right quadrant is observed by the child
    // view as a tap in the center of its top-left quadrant.
    switch (tap_location) {
      case TapLocation::kTopLeft:
        // center of top right quadrant -> ends up as center of top left quadrant
        InjectTap(/* x = */ 3 * display_width() / 4, /* y = */ display_height() / 4);
        break;
      case TapLocation::kTopRight:
        // center of bottom right quadrant -> ends up as center of top right quadrant
        InjectTap(/* x = */ 3 * display_width() / 4, /* y = */ 3 * display_height() / 4);
        break;
      default:
        FX_NOTREACHED();
    }
  }

  uint32_t display_rotation() override { return 90; }

  // TODO: https://fxbug.dev/42082519 - Test for DPR=2.0, too.
  float device_pixel_ratio() override { return 1.f; }

  const std::vector<TouchEvent>& events_received() const { return events_received_; }

 private:
  static constexpr auto kAppCode = R"JS(
    document.body.innerHTML = '<div id="click-target" style="width: 100%; height: 100%; background-color: #ff00ff;"></div>';
    let target = document.getElementById("click-target");

    target.onclick = function(event) {
      target.style.backgroundColor = '#40e0d0';
      console.assert(port != null);
      let response = JSON.stringify({
        epoch_msec: Date.now(),
        x: event.screenX,
        y: event.screenY,
        device_pixel_ratio: window.devicePixelRatio,
      });
      console.info('Reporting touch event ', response);
      port.postMessage(response);
    };

    waitForVisible("click-target").then((element) => {
      document.title = [document.title, 'input_ready'].join(' ');
    });
  )JS";

  void ExtendRealm() override {
    ui_testing::WebViewClient::AddToRealm(
        realm_builder().root(), dispatcher(), &web_client_,
        fuchsia::web::ContextFeatureFlags::VULKAN | fuchsia::web::ContextFeatureFlags::NETWORK);
    realm_builder().AddRoute(
        {.capabilities =
             {Protocol{fidl::DiscoverableProtocolName<
                  fuchsia_accessibility_semantics::SemanticsManager>},
              Protocol{fidl::DiscoverableProtocolName<fuchsia_ui_composition::Flatland>},
              Protocol{fidl::DiscoverableProtocolName<fuchsia_ui_composition::Allocator>}},
         .source = kTestUIStackRef,
         .targets = {ui_testing::WebViewClient::kChildRef}});
    realm_builder().AddRoute(
        {.capabilities = {Protocol{fidl::DiscoverableProtocolName<fuchsia_ui_input::ImeService>}},
         .source = ChildRef{kTextManager},
         .targets = {ui_testing::WebViewClient::kChildRef}});
  }

  void ReceiveNextTouchEvent() {
    if (!web_client_) {
      return;
    }
    web_client_->ReceiveMessage([this](std::string message) {
      rapidjson::Document tap_resp =
          json::JSONParser().ParseFromString(message, "web-touch-response");
      FX_CHECK(!tap_resp.HasParseError()) << tap_resp.GetParseError();
      FX_CHECK(tap_resp.HasMember("x") && tap_resp["x"].IsInt());
      FX_CHECK(tap_resp.HasMember("y") && tap_resp["y"].IsInt());
      FX_CHECK(tap_resp.HasMember("device_pixel_ratio") &&
               tap_resp["device_pixel_ratio"].IsNumber());

      events_received_.push_back(TouchEvent{
          .local_x = tap_resp["x"].GetInt(),
          .local_y = tap_resp["y"].GetInt(),
          .device_pixel_ratio = tap_resp["device_pixel_ratio"].GetDouble(),
      });
      ReceiveNextTouchEvent();
    });
  }

  static constexpr auto kTextManager = "text_manager";
  static constexpr auto kTextManagerUrl = "#meta/text_manager.cm";

  ui_testing::WebViewClient* web_client_ = nullptr;
  std::vector<TouchEvent> events_received_;
};

INSTANTIATE_TEST_SUITE_P(WebEngineTestParameterized, WebEngineTest,
                         testing::ValuesIn(AsTuples(ConfigsToTest())));

TEST_P(WebEngineTest, ChromiumTap) {
  InjectInput(TapLocation::kTopLeft);

  RunLoopUntil([this] { return !events_received().empty(); });

  ASSERT_EQ(events_received().size(), 1u);

  ExpectLocation(events_received()[0],
                 /*expected_x=*/static_cast<float>(display_height()) / 4.f,
                 /*expected_y=*/static_cast<float>(display_width()) / 4.f);
}

}  // namespace
