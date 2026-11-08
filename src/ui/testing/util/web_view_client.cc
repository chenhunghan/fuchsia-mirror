// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/testing/util/web_view_client.h"

#include <fuchsia/buildinfo/cpp/fidl.h>
#include <fuchsia/component/decl/cpp/fidl.h>
#include <fuchsia/fonts/cpp/fidl.h>
#include <fuchsia/intl/cpp/fidl.h>
#include <fuchsia/kernel/cpp/fidl.h>
#include <fuchsia/logger/cpp/fidl.h>
#include <fuchsia/mem/cpp/fidl.h>
#include <fuchsia/memorypressure/cpp/fidl.h>
#include <fuchsia/metrics/cpp/fidl.h>
#include <fuchsia/net/interfaces/cpp/fidl.h>
#include <fuchsia/posix/socket/cpp/fidl.h>
#include <fuchsia/process/cpp/fidl.h>
#include <fuchsia/scheduler/cpp/fidl.h>
#include <fuchsia/sysmem/cpp/fidl.h>
#include <fuchsia/sysmem2/cpp/fidl.h>
#include <fuchsia/tracing/provider/cpp/fidl.h>
#include <fuchsia/vulkan/loader/cpp/fidl.h>
#include <lib/stdcompat/string_view.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/vmo.h>
#include <zircon/status.h>

#include <memory>
#include <utility>
#include <vector>

namespace ui_testing {
namespace {

using component_testing::ChildRef;
using component_testing::Directory;
using component_testing::ParentRef;
using component_testing::Protocol;

constexpr char kFontsProvider[] = "fonts_provider";
constexpr char kFontsProviderUrl[] = "#meta/font_provider_hermetic_for_test.cm";

constexpr char kIntl[] = "intl";
constexpr char kIntlUrl[] = "#meta/intl_property_manager.cm";

constexpr char kMemoryPressureSignaler[] = "memory_pressure_signaler";
constexpr char kMemoryPressureSignalerUrl[] = "#meta/memory_pressure_signaler.cm";

constexpr char kNetstackUrl[] = "netstack#meta/netstack3.cm";

constexpr char kWebContextProvider[] = "web_context_provider";
constexpr char kWebContextProviderUrl[] =
    "fuchsia-pkg://fuchsia.com/web_engine#meta/context_provider.cm";

constexpr char kBuildInfoProvider[] = "build_info_provider";
constexpr char kBuildInfoProviderUrl[] = "#meta/fake_build_info.cm";

constexpr char kFakeCobalt[] = "cobalt";
constexpr char kFakeCobaltUrl[] = "#meta/fake_cobalt.cm";

constexpr std::string_view kMessagePortSetupJs = R"JS(
  let port;
  window.onresize = function(event) {
    if (window.innerWidth != 0 && window.innerHeight != 0) {
      document.title = [document.title, 'window_resized'].join(' ');
    }
  };
  window.addEventListener('message', function(event) {
    if (event.data == 'REGISTER_PORT') {
      port = event.ports[0];
      if (window.innerWidth != 0 && window.innerHeight != 0) {
        port.postMessage('PORT_REGISTERED WINDOW_RESIZED');
      } else {
        port.postMessage('PORT_REGISTERED');
      }
    }
  }, false);
  function waitForVisible(elementId) {
    return new Promise((resolve) => {
      function check() {
        const element = document.getElementById(elementId);
        if (element) {
          const hasDimensions = element.offsetWidth > 0 && element.offsetHeight > 0;
          const style = window.getComputedStyle(element);
          if (hasDimensions && style.visibility === 'visible') {
            resolve(element);
            return;
          }
        }
        requestAnimationFrame(check);
      }
      requestAnimationFrame(check);
    });
  }
)JS";

fuchsia::mem::Buffer BufferFromString(std::string_view script) {
  fuchsia::mem::Buffer buffer;
  uint64_t num_bytes = script.size();

  zx::vmo vmo;
  zx_status_t status = zx::vmo::create(num_bytes, /* options = */ 0u, &vmo);
  FX_CHECK(status == ZX_OK) << zx_status_get_string(status);

  status = vmo.write(script.data(), /* offset = */ 0u, num_bytes);
  FX_CHECK(status == ZX_OK) << zx_status_get_string(status);
  buffer.vmo = std::move(vmo);
  buffer.size = num_bytes;

  return buffer;
}

std::string StringFromBuffer(const fuchsia::mem::Buffer& buffer) {
  std::string str(buffer.size, '\0');
  zx_status_t status = buffer.vmo.read(str.data(), /* offset = */ 0u, buffer.size);
  FX_CHECK(status == ZX_OK) << zx_status_get_string(status);
  return str;
}

}  // namespace

// static
void WebViewClient::AddWebEngineSupportToRealm(component_testing::Realm& realm,
                                               component_testing::ChildRef target) {
  realm.AddChild(kFontsProvider, kFontsProviderUrl);
  realm.AddChild(kIntl, kIntlUrl);
  realm.AddChild(kMemoryPressureSignaler, kMemoryPressureSignalerUrl);
  realm.AddChild(kNetstackChildName, kNetstackUrl);
  realm.AddChild(kWebContextProvider, kWebContextProviderUrl);
  realm.AddChild(kBuildInfoProvider, kBuildInfoProviderUrl);
  realm.AddChild(kFakeCobalt, kFakeCobaltUrl);

  realm.AddRoute({.capabilities = {Protocol{fuchsia::logger::LogSink::Name_}},
                  .source = ParentRef(),
                  .targets = {target, ChildRef{kFontsProvider}, ChildRef{kMemoryPressureSignaler},
                              ChildRef{kBuildInfoProvider}, ChildRef{kWebContextProvider},
                              ChildRef{kIntl}, ChildRef{kFakeCobalt}}});
  realm.AddRoute({.capabilities = {Protocol{fuchsia::fonts::Provider::Name_}},
                  .source = ChildRef{kFontsProvider},
                  .targets = {target}});
  realm.AddRoute(
      {.capabilities = {Protocol{fuchsia::tracing::provider::Registry::Name_}},
       .source = ParentRef(),
       .targets = {target, ChildRef{kFontsProvider}, ChildRef{kMemoryPressureSignaler}}});
  realm.AddRoute({.capabilities = {Protocol{fuchsia::memorypressure::Provider::Name_}},
                  .source = ChildRef{kMemoryPressureSignaler},
                  .targets = {target}});
  realm.AddRoute({.capabilities = {Protocol{fuchsia::net::interfaces::State::Name_},
                                   Protocol{fuchsia::posix::socket::Provider::Name_}},
                  .source = kNetstackRef,
                  .targets = {target}});
  realm.AddRoute({.capabilities = {Protocol{fuchsia::kernel::VmexResource::Name_},
                                   Protocol{fuchsia::process::Launcher::Name_},
                                   Protocol{fuchsia::vulkan::loader::Loader::Name_}},
                  .source = ParentRef(),
                  .targets = {target}});
  realm.AddRoute({.capabilities = {Protocol{fuchsia::web::ContextProvider::Name_}},
                  .source = ChildRef{kWebContextProvider},
                  .targets = {target}});
  realm.AddRoute({.capabilities = {Protocol{fuchsia::metrics::MetricEventLoggerFactory::Name_}},
                  .source = ChildRef{kFakeCobalt},
                  .targets = {ChildRef{kMemoryPressureSignaler}}});
  realm.AddRoute({.capabilities = {Protocol{fuchsia::sysmem::Allocator::Name_},
                                   Protocol{fuchsia::sysmem2::Allocator::Name_}},
                  .source = ParentRef(),
                  .targets = {ChildRef{kMemoryPressureSignaler}, target}});
  realm.AddRoute({.capabilities = {Protocol{fuchsia::kernel::RootJobForInspect::Name_},
                                   Protocol{fuchsia::kernel::Stats::Name_}},
                  .source = ParentRef(),
                  .targets = {ChildRef{kMemoryPressureSignaler}}});
  realm.AddRoute({.capabilities = {Protocol{fuchsia::scheduler::RoleManager::Name_}},
                  .source = ParentRef(),
                  .targets = {ChildRef{kMemoryPressureSignaler}, target}});
  realm.AddRoute({.capabilities = {Protocol{fuchsia::buildinfo::Provider::Name_}},
                  .source = ChildRef{kBuildInfoProvider},
                  .targets = {target, ChildRef{kWebContextProvider}}});
  realm.AddRoute({.capabilities =
                      {
                          Directory{
                              .name = "root-ssl-certificates",
                              .type = fuchsia::component::decl::DependencyType::STRONG,
                          },
                          Directory{
                              .name = "tzdata-icu",
                              .type = fuchsia::component::decl::DependencyType::STRONG,
                          },
                      },
                  .source = ParentRef(),
                  .targets = {ChildRef{kWebContextProvider}}});
  realm.AddRoute({.capabilities = {Protocol{fuchsia::intl::PropertyProvider::Name_}},
                  .source = ChildRef{kIntl},
                  .targets = {target}});
}

// static
void WebViewClient::AddToRealm(component_testing::Realm& realm, async_dispatcher_t* dispatcher,
                               WebViewClient** out_client,
                               fuchsia::web::ContextFeatureFlags features) {
  realm.AddLocalChild(kChildName, [dispatcher, out_client, features] {
    return std::make_unique<WebViewClient>(dispatcher, features, out_client);
  });
  AddWebEngineSupportToRealm(realm, kChildRef);
  realm.AddRoute({.capabilities = {Protocol{fuchsia::ui::app::ViewProvider::Name_}},
                  .source = kChildRef,
                  .targets = {ParentRef()}});
}

WebViewClient::WebViewClient(async_dispatcher_t* dispatcher,
                             fuchsia::web::ContextFeatureFlags features, WebViewClient** out_client)
    : dispatcher_(dispatcher),
      features_(features),
      out_client_(out_client),
      navigation_event_listener_binding_(this) {
  if (out_client_ != nullptr) {
    *out_client_ = this;
  }
}

WebViewClient::~WebViewClient() {
  if (out_client_ != nullptr && *out_client_ == this) {
    *out_client_ = nullptr;
  }
}

void WebViewClient::LoadUrl(std::string url) {
  FX_CHECK(is_started_);
  is_main_document_loaded_ = false;
  is_window_resized_ = false;
  title_.clear();
  navigation_controller_->LoadUrl(std::move(url), fuchsia::web::LoadUrlParams(), [](auto result) {
    FX_CHECK(!result.is_err()) << "Error while loading URL: "
                               << static_cast<uint32_t>(result.err());
  });
}

void WebViewClient::ExecuteJavaScript(std::string_view script, fit::closure on_complete) {
  FX_CHECK(is_started_);
  web_frame_->ExecuteJavaScript(
      {"*"}, BufferFromString(script), [on_complete = std::move(on_complete)](auto result) mutable {
        FX_CHECK(!result.is_err())
            << "Error while executing JavaScript: " << static_cast<uint32_t>(result.err());
        if (on_complete) {
          on_complete();
        }
      });
}

void WebViewClient::RegisterMessagePort(fit::closure on_registered) {
  FX_CHECK(is_started_);
  ExecuteJavaScript(
      kMessagePortSetupJs, [this, on_registered = std::move(on_registered)]() mutable {
        fuchsia::web::WebMessage web_message;
        web_message.set_data(BufferFromString("REGISTER_PORT"));

        std::vector<fuchsia::web::OutgoingTransferable> outgoing;
        outgoing.emplace_back(fuchsia::web::OutgoingTransferable::WithMessagePort(
            message_port_.NewRequest(dispatcher_)));
        web_message.set_outgoing_transfer(std::move(outgoing));

        web_frame_->PostMessage(/*target_origin=*/"*", std::move(web_message),
                                [](auto result) { FX_CHECK(!result.is_err()); });
        message_port_->ReceiveMessage(
            [this, on_registered = std::move(on_registered)](fuchsia::web::WebMessage web_message) {
              std::string message = StringFromBuffer(web_message.data());
              if (message == "PORT_REGISTERED WINDOW_RESIZED") {
                is_window_resized_ = true;
              } else {
                FX_CHECK(message == "PORT_REGISTERED") << "Unexpected message: " << message;
              }
              if (on_registered) {
                on_registered();
              }
            });
      });
}

void WebViewClient::ReceiveMessage(fit::function<void(std::string)> callback) {
  FX_CHECK(message_port_.is_bound());
  message_port_->ReceiveMessage(
      [callback = std::move(callback)](fuchsia::web::WebMessage web_message) {
        callback(StringFromBuffer(web_message.data()));
      });
}

void WebViewClient::OnStart() {
  FX_CHECK(outgoing()->AddPublicService(
               fidl::InterfaceRequestHandler<fuchsia::ui::app::ViewProvider>([this](auto request) {
                 view_provider_bindings_.AddBinding(this, std::move(request), dispatcher_);
               })) == ZX_OK);

  FX_CHECK(svc().Connect(web_context_provider_.NewRequest(dispatcher_)) == ZX_OK);
  web_context_provider_.set_error_handler([](zx_status_t status) {
    FX_LOGS(WARNING) << "web_context_provider_: " << zx_status_get_string(status);
  });

  auto incoming_service_clone = svc().CloneChannel();
  FX_CHECK(incoming_service_clone.is_valid());

  fuchsia::web::CreateContextParams params;
  params.set_service_directory(std::move(incoming_service_clone));
  params.set_features(features_);
  web_context_provider_->Create(std::move(params), web_context_.NewRequest(dispatcher_));
  web_context_.set_error_handler([](zx_status_t status) {
    FX_LOGS(WARNING) << "web_context_: " << zx_status_get_string(status);
  });

  web_context_->CreateFrame(web_frame_.NewRequest(dispatcher_));
  web_frame_.set_error_handler([](zx_status_t status) {
    FX_LOGS(WARNING) << "web_frame_: " << zx_status_get_string(status);
  });
  web_frame_->SetJavaScriptLogLevel(fuchsia::web::ConsoleLogLevel::ERROR);
  web_frame_->SetNavigationEventListener(
      navigation_event_listener_binding_.NewBinding(dispatcher_));
  web_frame_->GetNavigationController(navigation_controller_.NewRequest(dispatcher_));

  is_started_ = true;
}

void WebViewClient::CreateView2(fuchsia::ui::app::CreateView2Args args) {
  fuchsia::web::CreateView2Args create_view_args;
  create_view_args.set_view_creation_token(std::move(*args.mutable_view_creation_token()));
  web_frame_->CreateView2(std::move(create_view_args));
}

void WebViewClient::CreateViewWithViewRef(zx::eventpair token,
                                          fuchsia::ui::views::ViewRefControl view_ref_control,
                                          fuchsia::ui::views::ViewRef view_ref) {
  ZX_PANIC("Not Implemented");
}

void WebViewClient::OnNavigationStateChanged(fuchsia::web::NavigationState nav_state,
                                             OnNavigationStateChangedCallback send_ack) {
  if (nav_state.has_is_main_document_loaded()) {
    is_main_document_loaded_ = nav_state.is_main_document_loaded();
  }
  if (nav_state.has_page_type()) {
    FX_CHECK(nav_state.page_type() != fuchsia::web::PageType::ERROR);
  }
  if (nav_state.has_title()) {
    title_ = nav_state.title();
    if (cpp23::contains(title_, "window_resized")) {
      is_window_resized_ = true;
    }
  }
  send_ack();
}

}  // namespace ui_testing
