// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_TESTING_UTIL_WEB_VIEW_CLIENT_H_
#define SRC_UI_TESTING_UTIL_WEB_VIEW_CLIENT_H_

#include <fuchsia/ui/app/cpp/fidl.h>
#include <fuchsia/ui/views/cpp/fidl.h>
#include <fuchsia/web/cpp/fidl.h>
#include <lib/async/dispatcher.h>
#include <lib/fidl/cpp/binding.h>
#include <lib/fidl/cpp/binding_set.h>
#include <lib/fit/function.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <lib/zx/eventpair.h>

#include <string>
#include <string_view>

namespace ui_testing {

// In-process RealmBuilder component that hosts a Chromium WebEngine Frame and
// serves `fuchsia.ui.app.ViewProvider` to the test scene.
class WebViewClient final : public fuchsia::ui::app::ViewProvider,
                            public fuchsia::web::NavigationEventListener,
                            public component_testing::LocalComponentImpl {
 public:
  static constexpr char kChildName[] = "web_view";
  static constexpr component_testing::ChildRef kChildRef{kChildName};

  static constexpr char kNetstackChildName[] = "netstack";
  static constexpr component_testing::ChildRef kNetstackRef{kNetstackChildName};

  // Adds the hermetic WebEngine support components (`context_provider`,
  // `fonts_provider`, `intl`, `memory_pressure_signaler`, `netstack`,
  // `build_info_provider`, and `cobalt`) to `realm` and routes their
  // capabilities to `target`.
  static void AddWebEngineSupportToRealm(component_testing::Realm& realm,
                                         component_testing::ChildRef target);

  // Adds a local `WebViewClient` child (`kChildName`) and hermetic WebEngine
  // support components to `realm`, routing `ViewProvider` to `ParentRef`.
  //
  // When Component Manager starts the local child, `*out_client` is set to the
  // running instance and cleared when the instance is destroyed.
  //
  // Callers must still route UI and accessibility protocols (such as
  // `Flatland`, `Allocator`, `ImeService`, and `SemanticsManager`) from their
  // respective realm sources to `kChildRef`.
  static void AddToRealm(
      component_testing::Realm& realm, async_dispatcher_t* dispatcher, WebViewClient** out_client,
      fuchsia::web::ContextFeatureFlags features = fuchsia::web::ContextFeatureFlags::NETWORK);

  explicit WebViewClient(
      async_dispatcher_t* dispatcher,
      fuchsia::web::ContextFeatureFlags features = fuchsia::web::ContextFeatureFlags::NETWORK,
      WebViewClient** out_client = nullptr);
  ~WebViewClient() override;

  bool is_started() const { return is_started_; }
  bool is_main_document_loaded() const { return is_main_document_loaded_; }
  bool is_window_resized() const { return is_window_resized_; }
  const std::string& title() const { return title_; }

  // Starts navigating the frame to `url` and resets navigation state.
  //
  // Precondition: `is_started()` must be true.
  void LoadUrl(std::string url);

  // Executes `script` in the frame across all origins and invokes `on_complete`
  // when execution finishes.
  //
  // Precondition: `is_started()` must be true.
  void ExecuteJavaScript(std::string_view script, fit::closure on_complete = nullptr);

  // Injects message-port and resize-tracking helpers (`port` and
  // `waitForVisible(elementId)`) into the current page, registers a
  // `MessagePort` with the page, and invokes `on_registered` once the port
  // handshake completes. Also updates `is_window_resized()` once the page
  // reports non-zero window dimensions.
  //
  // Precondition: `is_started()` must be true.
  void RegisterMessagePort(fit::closure on_registered);

  // Reads the next message sent by the web page on the registered `MessagePort`
  // and passes its payload as a string to `callback`.
  //
  // Precondition: `RegisterMessagePort()` must have been called.
  void ReceiveMessage(fit::function<void(std::string)> callback);

  // |component_testing::LocalComponentImpl|
  void OnStart() override;

  // |fuchsia::ui::app::ViewProvider|
  void CreateView2(fuchsia::ui::app::CreateView2Args args) override;
  void CreateViewWithViewRef(zx::eventpair token,
                             fuchsia::ui::views::ViewRefControl view_ref_control,
                             fuchsia::ui::views::ViewRef view_ref) override;

  // |fuchsia::web::NavigationEventListener|
  void OnNavigationStateChanged(fuchsia::web::NavigationState nav_state,
                                OnNavigationStateChangedCallback send_ack) override;

 private:
  async_dispatcher_t* dispatcher_ = nullptr;
  fuchsia::web::ContextFeatureFlags features_;
  WebViewClient** out_client_ = nullptr;
  fidl::BindingSet<fuchsia::ui::app::ViewProvider> view_provider_bindings_;
  fidl::Binding<fuchsia::web::NavigationEventListener> navigation_event_listener_binding_;
  fuchsia::web::ContextProviderPtr web_context_provider_;
  fuchsia::web::ContextPtr web_context_;
  fuchsia::web::FramePtr web_frame_;
  fuchsia::web::NavigationControllerPtr navigation_controller_;
  fuchsia::web::MessagePortPtr message_port_;
  bool is_started_ = false;
  bool is_main_document_loaded_ = false;
  bool is_window_resized_ = false;
  std::string title_;
};

}  // namespace ui_testing

#endif  // SRC_UI_TESTING_UTIL_WEB_VIEW_CLIENT_H_
