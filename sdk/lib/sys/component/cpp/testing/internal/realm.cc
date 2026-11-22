// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.component.decl/cpp/fidl.h>
#include <fidl/fuchsia.component/cpp/fidl.h>
#include <fidl/fuchsia.io/cpp/fidl.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fidl/cpp/client.h>
#include <lib/fit/function.h>
#include <lib/sys/component/cpp/testing/internal/errors.h>
#include <lib/sys/component/cpp/testing/internal/realm.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/sys/cpp/service_directory.h>

#include <memory>
#include <utility>

namespace component_testing {
namespace internal {

fidl::SyncClient<fuchsia_component::Realm> CreateRealmClient(
    fidl::UnownedClientEnd<fuchsia_io::Directory> svc) {
  zx::result client_end = component::ConnectAt<fuchsia_component::Realm>(svc);
  ZX_COMPONENT_ASSERT_STATUS_OK("component::ConnectAt<Realm>", client_end.status_value());
  return fidl::SyncClient<fuchsia_component::Realm>(std::move(*client_end));
}

fidl::SyncClient<fuchsia_component::Realm> CreateRealmClient(const sys::ComponentContext* context) {
  if (context) {
    return CreateRealmClient(context->svc());
  }
  return CreateRealmClient(std::shared_ptr<sys::ServiceDirectory>(nullptr));
}

fidl::SyncClient<fuchsia_component::Realm> CreateRealmClient(
    std::shared_ptr<sys::ServiceDirectory> svc) {
  if (svc == nullptr) {
    zx::result svc_root = component::OpenServiceRoot();
    ZX_COMPONENT_ASSERT_STATUS_OK("component::OpenServiceRoot", svc_root.status_value());
    return CreateRealmClient(svc_root->borrow());
  }

  fidl::ClientEnd<fuchsia_io::Directory> dir(svc->CloneChannel().TakeChannel());
  return CreateRealmClient(dir.borrow());
}

fidl::ClientEnd<fuchsia_io::Directory> OpenExposedDir(
    const fidl::SyncClient<fuchsia_component::Realm>& realm,
    const fuchsia_component_decl::ChildRef& child_ref) {
  ZX_ASSERT_MSG(realm.is_valid(), "realm client must be valid");
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_io::Directory>::Create();
  ZX_COMPONENT_ASSERT_RESULT_OK(
      "Realm/OpenExposedDir",
      realm->OpenExposedDir({{.child = child_ref, .exposed_dir = std::move(server_end)}}));
  return std::move(client_end);
}

void CreateChild(const fidl::SyncClient<fuchsia_component::Realm>& realm,
                 fidl::ServerEnd<fuchsia_component::Controller> controller, std::string collection,
                 std::string name, std::string url) {
  ZX_ASSERT_MSG(realm.is_valid(), "realm client must be valid");
  fuchsia_component_decl::CollectionRef collection_ref{{
      .name = std::move(collection),
  }};
  fuchsia_component_decl::Child child_decl{{
      .name = std::move(name),
      .url = std::move(url),
      .startup = fuchsia_component_decl::StartupMode::kLazy,
  }};
  fuchsia_component::CreateChildArgs child_args{{
      .controller = std::move(controller),
  }};
  ZX_COMPONENT_ASSERT_RESULT_OK("Realm/CreateChild", realm->CreateChild({{
                                                         .collection = std::move(collection_ref),
                                                         .decl = std::move(child_decl),
                                                         .args = std::move(child_args),
                                                     }}));
}

void DestroyChild(const fidl::SyncClient<fuchsia_component::Realm>& realm,
                  fuchsia_component_decl::ChildRef child_ref) {
  ZX_ASSERT_MSG(realm.is_valid(), "realm client must be valid");
  ZX_COMPONENT_ASSERT_RESULT_OK("Realm/DestroyChild",
                                realm->DestroyChild({{.child = std::move(child_ref)}}));
}

void DestroyChild(fidl::UnownedClientEnd<fuchsia_io::Directory> svc, async_dispatcher_t* dispatcher,
                  fuchsia_component_decl::ChildRef child_ref,
                  fit::function<void(fit::result<fuchsia_component::Error>)> callback) {
  zx::result client_end = component::ConnectAt<fuchsia_component::Realm>(svc);
  ZX_COMPONENT_ASSERT_STATUS_OK("component::ConnectAt<Realm>", client_end.status_value());
  auto shared_client = std::make_shared<fidl::SharedClient<fuchsia_component::Realm>>(
      std::move(*client_end), dispatcher);
  (*shared_client)
      ->DestroyChild({{.child = std::move(child_ref)}})
      .Then([client = shared_client, callback = std::move(callback)](
                fidl::Result<fuchsia_component::Realm::DestroyChild>& result) mutable {
        if (result.is_ok()) {
          if (callback) {
            callback(fit::ok());
          }
          return;
        }
        if (result.error_value().is_domain_error()) {
          if (callback) {
            callback(fit::error(result.error_value().domain_error()));
          }
          return;
        }
        if (result.error_value().framework_error().is_dispatcher_shutdown()) {
          ZX_PANIC("async callback dropped without being called");
        }
        ZX_PANIC("%s", result.error_value().framework_error().status_string());
      });
}

}  // namespace internal
}  // namespace component_testing
