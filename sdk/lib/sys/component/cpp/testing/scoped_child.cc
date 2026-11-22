// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.component.decl/cpp/fidl.h>
#include <fidl/fuchsia.component/cpp/fidl.h>
#include <fidl/fuchsia.component/cpp/hlcpp_conversion.h>
#include <fidl/fuchsia.io/cpp/fidl.h>
#include <fuchsia/component/cpp/fidl.h>
#include <fuchsia/io/cpp/fidl.h>
#include <lib/async/default.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/io.h>
#include <lib/sys/component/cpp/testing/internal/errors.h>
#include <lib/sys/component/cpp/testing/internal/realm.h>
#include <lib/sys/component/cpp/testing/scoped_child.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/sys/cpp/service_directory.h>
#include <zircon/assert.h>
#include <zircon/status.h>
#include <zircon/types.h>

#include <memory>
#include <optional>
#include <random>
#include <utility>

namespace component_testing {

namespace {

std::size_t random_unsigned() {
  std::random_device random_device;
  std::mt19937 generator(random_device());
  std::uniform_int_distribution<std::size_t> distribution;
  return distribution(generator);
}

fidl::ClientEnd<fuchsia_io::Directory> ToNaturalSvc(std::shared_ptr<sys::ServiceDirectory> svc) {
  if (svc == nullptr) {
    zx::result svc_root = component::OpenServiceRoot();
    ZX_COMPONENT_ASSERT_STATUS_OK("component::OpenServiceRoot", svc_root.status_value());
    return std::move(*svc_root);
  }
  return fidl::ClientEnd<fuchsia_io::Directory>(svc->CloneChannel().TakeChannel());
}

}  // namespace

ScopedChild ScopedChild::New(fuchsia::component::RealmSyncPtr realm_proxy, std::string collection,
                             std::string url) {
  return NewFromRealm(fidl::ClientEnd<fuchsia_component::Realm>(realm_proxy.Unbind().TakeChannel()),
                      std::move(collection), std::nullopt, std::move(url));
}

ScopedChild ScopedChild::New(fuchsia::component::RealmSyncPtr realm_proxy, std::string collection,
                             std::string name, std::string url) {
  return NewFromRealm(fidl::ClientEnd<fuchsia_component::Realm>(realm_proxy.Unbind().TakeChannel()),
                      std::move(collection), std::move(name), std::move(url));
}

ScopedChild ScopedChild::New(fidl::ClientEnd<fuchsia_component::Realm> realm,
                             std::string collection, std::string name, std::string url) {
  return NewFromRealm(std::move(realm), std::move(collection), std::move(name), std::move(url));
}

ScopedChild ScopedChild::New(fidl::ClientEnd<fuchsia_component::Realm> realm,
                             std::string collection, std::string url) {
  return NewFromRealm(std::move(realm), std::move(collection), std::nullopt, std::move(url));
}

ScopedChild ScopedChild::New(std::string collection, std::string name, std::string url,
                             std::shared_ptr<sys::ServiceDirectory> svc) {
  return NewFromClientEnd(std::move(collection), std::move(name), std::move(url),
                          ToNaturalSvc(std::move(svc)));
}

ScopedChild ScopedChild::New(std::string collection, std::string url,
                             std::shared_ptr<sys::ServiceDirectory> svc) {
  return NewFromClientEnd(std::move(collection), std::nullopt, std::move(url),
                          ToNaturalSvc(std::move(svc)));
}

ScopedChild ScopedChild::New(std::string collection, std::string name, std::string url,
                             fidl::ClientEnd<fuchsia_io::Directory> svc) {
  return NewFromClientEnd(std::move(collection), std::move(name), std::move(url), std::move(svc));
}

ScopedChild ScopedChild::New(std::string collection, std::string url,
                             fidl::ClientEnd<fuchsia_io::Directory> svc) {
  return NewFromClientEnd(std::move(collection), std::nullopt, std::move(url), std::move(svc));
}

ScopedChild ScopedChild::NewFromRealm(fidl::ClientEnd<fuchsia_component::Realm> realm_client_end,
                                      std::string collection, std::optional<std::string> name,
                                      std::string url) {
  std::string resolved_name =
      name.has_value() ? std::move(*name) : "auto-" + std::to_string(random_unsigned());
  fidl::SyncClient<fuchsia_component::Realm> realm(std::move(realm_client_end));
  auto [controller_client, controller_server] =
      fidl::Endpoints<fuchsia_component::Controller>::Create();
  internal::CreateChild(realm, std::move(controller_server), collection, resolved_name,
                        std::move(url));
  fuchsia_component_decl::ChildRef child_ref{{
      .name = std::move(resolved_name),
      .collection = std::move(collection),
  }};
  fidl::ClientEnd<fuchsia_io::Directory> exposed_dir = internal::OpenExposedDir(realm, child_ref);
  zx::result svc = component::OpenServiceRoot();
  ZX_COMPONENT_ASSERT_STATUS_OK("component::OpenServiceRoot", svc.status_value());
  return ScopedChild(std::move(*svc), std::move(child_ref), std::move(exposed_dir),
                     fidl::SyncClient<fuchsia_component::Controller>(std::move(controller_client)));
}

ScopedChild ScopedChild::NewFromClientEnd(std::string collection, std::optional<std::string> name,
                                          std::string url,
                                          fidl::ClientEnd<fuchsia_io::Directory> svc) {
  std::string resolved_name =
      name.has_value() ? std::move(*name) : "auto-" + std::to_string(random_unsigned());
  fidl::SyncClient<fuchsia_component::Realm> realm = internal::CreateRealmClient(svc.borrow());
  auto [controller_client, controller_server] =
      fidl::Endpoints<fuchsia_component::Controller>::Create();
  internal::CreateChild(realm, std::move(controller_server), collection, resolved_name,
                        std::move(url));
  fuchsia_component_decl::ChildRef child_ref{{
      .name = std::move(resolved_name),
      .collection = std::move(collection),
  }};
  fidl::ClientEnd<fuchsia_io::Directory> exposed_dir = internal::OpenExposedDir(realm, child_ref);
  return ScopedChild(std::move(svc), std::move(child_ref), std::move(exposed_dir),
                     fidl::SyncClient<fuchsia_component::Controller>(std::move(controller_client)));
}

ScopedChild::ScopedChild(fidl::ClientEnd<fuchsia_io::Directory> svc,
                         fuchsia_component_decl::ChildRef child_ref,
                         fidl::ClientEnd<fuchsia_io::Directory> exposed_dir,
                         fidl::SyncClient<fuchsia_component::Controller> controller_proxy)
    : svc_(std::move(svc)),
      child_ref_(std::move(child_ref)),
      controller_proxy_(std::move(controller_proxy)) {
  exposed_dir_.Bind(exposed_dir.TakeChannel());
}

ScopedChild::~ScopedChild() {
  if (!svc_.is_valid()) {
    return;
  }

  fidl::SyncClient<fuchsia_component::Realm> sync_realm =
      internal::CreateRealmClient(svc_.borrow());
  internal::DestroyChild(sync_realm, child_ref_);
}

void ScopedChild::Teardown(async_dispatcher_t* dispatcher, TeardownCallback callback) {
  ZX_ASSERT(callback != nullptr);

  TeardownNatural(dispatcher, [callback = std::move(callback)](
                                  fit::result<fuchsia_component::Error> result) mutable {
    if (result.is_ok()) {
      callback(fit::ok());
    } else {
      callback(fit::error(fidl::NaturalToHLCPP(result.error_value())));
    }
  });
}

void ScopedChild::TeardownNatural(async_dispatcher_t* dispatcher,
                                  NaturalTeardownCallback callback) {
  ZX_ASSERT(callback != nullptr);

  if (!svc_.is_valid()) {
    callback(fit::ok());
    return;
  }

  internal::DestroyChild(svc_.borrow(), dispatcher, child_ref_, std::move(callback));

  // Prevent the destructor from calling DestroyChild again.
  svc_.reset();
}

zx_status_t ScopedChild::Connect(const std::string& interface_name, zx::channel request) const {
  return fdio_service_connect_at(exposed_dir_.unowned_channel()->get(), interface_name.c_str(),
                                 request.release());
}

std::string ScopedChild::GetChildName() const { return child_ref_.name(); }

fidl::UnownedClientEnd<fuchsia_io::Directory> ScopedChild::exposed_dir() const {
  return fidl::UnownedClientEnd<fuchsia_io::Directory>(exposed_dir_.unowned_channel());
}

const fuchsia::io::DirectorySyncPtr& ScopedChild::exposed() const { return exposed_dir_; }

ExecutionController ScopedChild::Start(fuchsia::component::StartChildArgs start_args) const {
  fuchsia::component::ExecutionControllerPtr execution_controller_proxy;
  fidl::ServerEnd<fuchsia_component::ExecutionController> server_end(
      execution_controller_proxy.NewRequest().TakeChannel());
  ZX_COMPONENT_ASSERT_RESULT_OK("Controller/Start",
                                controller_proxy_->Start({{
                                    .args = fidl::HLCPPToNatural(std::move(start_args)),
                                    .execution_controller = std::move(server_end),
                                }}));
  return ExecutionController(std::move(execution_controller_proxy));
}

}  // namespace component_testing
