// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.accessibility.semantics/cpp/fidl.h>
#include <fidl/fuchsia.ui.composition/cpp/fidl.h>
#include <fidl/fuchsia.ui.input3/cpp/fidl.h>
#include <fuchsia/web/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/stdcompat/string_view.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>

#include <optional>
#include <string>
#include <utility>

#include <gtest/gtest.h>

#include "src/ui/testing/util/portable_ui_test.h"
#include "src/ui/testing/util/web_view_client.h"

namespace {

using component_testing::Protocol;

// Max timeout in failure cases.
// Set this as low as you can that still works across all test platforms.
constexpr zx::duration kTimeout = zx::min(5);

// See README.md for instructions on how to run this test with chrome remote
// devtools, which is super-useful for debugging.
class ChromiumInputTest : public ui_testing::PortableUITest {
 protected:
  std::string GetTestUIStackUrl() override { return "#meta/test-ui-stack.cm"; }

  void SetUp() override {
    ui_testing::PortableUITest::SetUp();

    // Post a "just in case" quit task, if the test hangs.
    async::PostDelayedTask(
        dispatcher(),
        [] { FX_LOGS(FATAL) << "\n\n>> Test did not complete in time, terminating.  <<\n\n"; },
        kTimeout);

    RegisterTouchScreen();
    RegisterKeyboard();
  }

  void TearDown() override {
    web_client_ = nullptr;
    ui_testing::PortableUITest::TearDown();
  }

  bool ResponseContains(std::string_view substring) {
    bool match = response_.has_value() && cpp23::contains(*response_, substring);
    if (match) {
      response_ = std::nullopt;
    }
    return match;
  }

  void LaunchWebEngineClient() {
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

    ReceiveNextTextResponse();

    FX_LOGS(INFO) << "Waiting on app focused on textarea: ready for key events";
    const int kMaxRetries = 10;
    for (int i = 0; i < kMaxRetries; i++) {
      FX_LOGS(INFO) << "Tapping on textarea: " << i + 1 << "/" << kMaxRetries;
      // TODO(b/477639582): Repeating the tap may not be necessary, but adding
      // it for now to stabilize the test after chromium 145.0.7622.0 roll.
      InjectTap(display_width() / 2, display_height() / 2);

      RunLoopWithTimeoutOrUntil(
          [this] { return cpp23::contains(web_client_->title(), "text_input_focused"); },
          zx::sec(1));
      if (cpp23::contains(web_client_->title(), "text_input_focused")) {
        FX_LOGS(INFO) << "App is ready for key events: tapped " << i + 1 << " times";
        break;
      }
    }
    ASSERT_TRUE(cpp23::contains(web_client_->title(), "text_input_focused"));
  }

 private:
  static constexpr auto kAppCode = R"JS(
    function sendMessageEvent(messageObj) {
      let message = JSON.stringify(messageObj);
      console.info('sendMessageEvent: ' + message);
      port.postMessage(message);
    }

    const headHtml = `
    <style>
      body {
        height: 100%;
        width: 100%;
        background-color: #000077; /* dark blue */
        color: white;
      }
      #text-input {
        height: 100%;
        width: 100%;
        background-color: #ca2c92; /* royal fuchsia */
        font-size: 36pt;
      }
    </style>
    `;

    document.head.innerHTML += headHtml;
    document.body.innerHTML = `<textarea id="text-input" rows="3" cols="20"></textarea>`;

    let input_element = document.querySelector("#text-input");

    input_element.addEventListener("keyup", function (e) {
      sendMessageEvent({
        text: input_element.value,
      });
    });

    input_element.addEventListener('focus', function (e) {
      document.title = [document.title, 'text_input_focused'].join(' ');
    });

    waitForVisible("text-input").then((element) => {
      document.title = [document.title, 'input_ready'].join(' ');
    });
  )JS";

  void ExtendRealm() override {
    ui_testing::WebViewClient::AddToRealm(realm_builder().root(), dispatcher(), &web_client_,
                                          fuchsia::web::ContextFeatureFlags::VULKAN |
                                              fuchsia::web::ContextFeatureFlags::NETWORK |
                                              fuchsia::web::ContextFeatureFlags::KEYBOARD);
    realm_builder().AddRoute(
        {.capabilities =
             {
                 Protocol{fidl::DiscoverableProtocolName<
                     fuchsia_accessibility_semantics::SemanticsManager>},
                 Protocol{fidl::DiscoverableProtocolName<fuchsia_ui_composition::Allocator>},
                 Protocol{fidl::DiscoverableProtocolName<fuchsia_ui_composition::Flatland>},
                 Protocol{fidl::DiscoverableProtocolName<fuchsia_ui_input3::Keyboard>},
             },
         .source = kTestUIStackRef,
         .targets = {ui_testing::WebViewClient::kChildRef}});
  }

  void ReceiveNextTextResponse() {
    if (!web_client_) {
      return;
    }
    web_client_->ReceiveMessage([this](std::string message) {
      FX_LOGS(INFO) << "App sent: '" << message << "'";
      response_ = std::move(message);
      ReceiveNextTextResponse();
    });
  }

  ui_testing::WebViewClient* web_client_ = nullptr;
  std::optional<std::string> response_;
};

// Launches a web engine to open a page with a full-screen text input window.
// Then taps the screen to move focus to that page, and types text on the
// fake injected keyboard. Loops around until the text appears in the
// text area.
TEST_F(ChromiumInputTest, BasicInputTest) {
  LaunchWebEngineClient();

  SimulateUsAsciiTextEntry("Hello\nworld!");

  RunLoopUntil([this] { return ResponseContains("Hello\\nworld!"); });

  FX_LOGS(INFO) << "Done";
}

}  // namespace
