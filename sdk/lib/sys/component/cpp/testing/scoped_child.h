// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_SYS_COMPONENT_CPP_TESTING_SCOPED_CHILD_H_
#define LIB_SYS_COMPONENT_CPP_TESTING_SCOPED_CHILD_H_

#include <fidl/fuchsia.component.decl/cpp/fidl.h>
#include <fidl/fuchsia.component/cpp/fidl.h>
#include <fidl/fuchsia.io/cpp/fidl.h>
#include <fuchsia/component/cpp/fidl.h>
#include <fuchsia/component/decl/cpp/fidl.h>
#include <fuchsia/io/cpp/fidl.h>
#include <lib/async/dispatcher.h>
#include <lib/component/incoming/cpp/clone.h>
#include <lib/fidl/cpp/client.h>
#include <lib/fidl/cpp/interface_handle.h>
#include <lib/fidl/cpp/interface_request.h>
#include <lib/fidl/cpp/wire/connect_service.h>
#include <lib/fit/result.h>
#include <lib/sys/component/cpp/testing/execution_controller.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/sys/cpp/service_directory.h>
#include <lib/zx/result.h>
#include <zircon/status.h>
#include <zircon/types.h>

#include <memory>
#include <optional>
#include <string>
#include <type_traits>
#include <utility>

namespace component_testing {

// A scoped instance of a dynamically created child component. This class
// will automatically destroy the child component once it goes out of scope.
class ScopedChild final {
 public:
  // Create a dynamic child component using the fuchsia.component.Realm API.
  // |realm_proxy| must be bound to a connection to the fuchsia.component.Realm protocol.
  // |collection| is the name of the collection to create the child under. This
  // field must refer to a name in the current component's manifest file.
  // |name| is the name to assign to the child.
  // |url| is the component component URL of the child component.
  static ScopedChild New(fuchsia::component::RealmSyncPtr realm_proxy, std::string collection,
                         std::string name, std::string url);

  // Same as above with a randomly generated `name`.
  static ScopedChild New(fuchsia::component::RealmSyncPtr realm_proxy, std::string collection,
                         std::string url);

  // Create a dynamic child component using the fuchsia.component.Realm API.
  // |collection| is the name of the collection to create the child under. This
  // field must refer to a name in the current component's manifest file.
  // |name| is the name to assign to the child.
  // |url| is the component component URL of the child component.
  // |svc| is used to make a connection to the protocol. If it's not provided,
  // then the namespace entry will be used.
  static ScopedChild New(std::string collection, std::string name, std::string url,
                         std::shared_ptr<sys::ServiceDirectory> svc = nullptr);

  // Same as above with a randomly generated `name`.
  static ScopedChild New(std::string collection, std::string url,
                         std::shared_ptr<sys::ServiceDirectory> svc = nullptr);

  // Create a dynamic child component using the fuchsia.component.Realm API.
  static ScopedChild New(fidl::ClientEnd<fuchsia_component::Realm> realm, std::string collection,
                         std::string name, std::string url) ZX_AVAILABLE_SINCE(NEXT);

  // Same as above with a randomly generated `name`.
  static ScopedChild New(fidl::ClientEnd<fuchsia_component::Realm> realm, std::string collection,
                         std::string url) ZX_AVAILABLE_SINCE(NEXT);

  // Create a dynamic child component using the fuchsia.component.Realm API via `svc`.
  static ScopedChild New(std::string collection, std::string name, std::string url,
                         fidl::ClientEnd<fuchsia_io::Directory> svc) ZX_AVAILABLE_SINCE(NEXT);

  // Same as above with a randomly generated `name`.
  static ScopedChild New(std::string collection, std::string url,
                         fidl::ClientEnd<fuchsia_io::Directory> svc) ZX_AVAILABLE_SINCE(NEXT);

  ~ScopedChild();

  ScopedChild(ScopedChild&&) = default;
  ScopedChild& operator=(ScopedChild&&) = default;

  ScopedChild(const ScopedChild&) = delete;
  ScopedChild& operator=(const ScopedChild&) = delete;

  using TeardownCallback = fit::function<void(fit::result<fuchsia::component::Error>)>;
  using NaturalTeardownCallback = fit::function<void(fit::result<fuchsia_component::Error>)>;

  // Calls fuchsia.component/Realm.DestroyChild asynchronously on |dispatcher|.
  // |callback| will be invoked when Component Manager has completed
  // the realm teardown.
  void Teardown(async_dispatcher_t* dispatcher, TeardownCallback callback);

  // Natural bindings overload of |Teardown|.
  template <typename Callback,
            typename = std::enable_if_t<std::conjunction_v<
                std::negation<
                    std::is_invocable_r<void, Callback, fit::result<fuchsia::component::Error>>>,
                std::is_invocable_r<void, Callback, fit::result<fuchsia_component::Error>>>>>
  void Teardown(async_dispatcher_t* dispatcher, Callback&& callback) ZX_AVAILABLE_SINCE(NEXT) {
    TeardownNatural(dispatcher, NaturalTeardownCallback(std::forward<Callback>(callback)));
  }

  // Connect to an interface in the exposed directory of the child component.
  //
  // The discovery name of the interface is inferred from the C++ type of the
  // interface. Callers can supply an interface name explicitly to override
  // the default name.
  //
  // This overload panics if the connection operation doesn't return ZX_OK.
  // Callers that wish to receive that status should use one of the other
  // overloads that returns a |zx_status_t|.
  //
  // # Example
  //
  // ```
  // auto echo = instance.Connect<test::placeholders::Echo>();
  // ```
  template <typename Interface, typename = std::enable_if_t<!fidl::IsProtocolV<Interface>>>
  fidl::InterfacePtr<Interface> Connect(
      const std::string& interface_name = Interface::Name_) const {
    fidl::InterfacePtr<Interface> result;
    zx_status_t status = Connect(interface_name, result.NewRequest().TakeChannel());
    ZX_ASSERT_MSG(status == ZX_OK, "Connect to protocol %s on the exposed dir of %s failed: %s",
                  interface_name.c_str(), child_ref_.name().c_str(), zx_status_get_string(status));
    return std::move(result);
  }

  // SynchronousInterfacePtr method variant of |Connect|. See |Connect| for
  // more details.
  template <typename Interface, typename = std::enable_if_t<!fidl::IsProtocolV<Interface>>>
  fidl::SynchronousInterfacePtr<Interface> ConnectSync(
      const std::string& interface_name = Interface::Name_) const {
    fidl::SynchronousInterfacePtr<Interface> result;
    zx_status_t status = Connect(interface_name, result.NewRequest().TakeChannel());
    ZX_ASSERT_MSG(status == ZX_OK, "Connect to protocol %s on the exposed dir of %s failed",
                  interface_name.c_str(), child_ref_.name().c_str());
    return std::move(result);
  }

  // Connect to exposed directory of the child component.
  template <typename Interface>
  zx_status_t Connect(fidl::InterfaceRequest<Interface> request) const {
    return Connect(Interface::Name_, request.TakeChannel());
  }

  // ServerEnd variant of |Connect| for natural/wire protocols.
  template <typename Protocol, typename = std::enable_if_t<fidl::IsProtocolV<Protocol>>>
  zx_status_t Connect(fidl::ServerEnd<Protocol> server_end) const ZX_AVAILABLE_SINCE(NEXT) {
    return Connect(std::string(fidl::DiscoverableProtocolName<Protocol>), server_end.TakeChannel());
  }

  // Connect to an interface in the exposed directory using the supplied
  // channel.
  zx_status_t Connect(const std::string& interface_name, zx::channel request) const;

  // ClientEnd variant of |Connect|. See |Connect| for more details.
  template <typename Protocol, typename = std::enable_if_t<fidl::IsProtocolV<Protocol>>,
            typename = void>
  zx::result<fidl::ClientEnd<Protocol>> Connect(
      std::string path = fidl::DiscoverableProtocolName<Protocol>) const {
    auto endpoints = fidl::CreateEndpoints<Protocol>();
    if (endpoints.is_error()) {
      return endpoints.take_error();
    }

    if (auto result = Connect(path, endpoints->server.TakeChannel()); result != ZX_OK) {
      return zx::error(result);
    }

    return zx::ok(std::move(endpoints->client));
  }

  // Get the child name of this instance.
  std::string GetChildName() const;

  // Clone the exposed directory.
  fidl::InterfaceHandle<fuchsia::io::Directory> CloneExposedDir() const {
    zx::result clone = component::Clone(
        fidl::UnownedClientEnd<fuchsia_io::Directory>(exposed_dir_.unowned_channel()));
    ZX_ASSERT_MSG(clone.is_ok(), "Cloning exposed directory failed: %s", clone.status_string());
    return fidl::InterfaceHandle<fuchsia::io::Directory>(clone->TakeChannel());
  }

  // Clone the exposed directory.
  void CloneExposedDir(fidl::InterfaceRequest<fuchsia::io::Directory> directory_request) const
      ZX_AVAILABLE_SINCE(11) {
    fidl::OneWayStatus status =
        fidl::WireCall(
            fidl::UnownedClientEnd<fuchsia_io::Directory>(exposed_dir_.unowned_channel()))
            ->Clone(fidl::ServerEnd<fuchsia_unknown::Cloneable>(directory_request.TakeChannel()));
    ZX_ASSERT_MSG(status.ok(), "Cloning exposed directory failed: %s", status.status_string());
  }

  // Clone the exposed directory into a natural/wire ServerEnd.
  void CloneExposedDir(fidl::ServerEnd<fuchsia_io::Directory> directory_request) const
      ZX_AVAILABLE_SINCE(NEXT) {
    fidl::OneWayStatus status =
        fidl::WireCall(
            fidl::UnownedClientEnd<fuchsia_io::Directory>(exposed_dir_.unowned_channel()))
            ->Clone(fidl::ServerEnd<fuchsia_unknown::Cloneable>(directory_request.TakeChannel()));
    ZX_ASSERT_MSG(status.ok(), "Cloning exposed directory failed: %s", status.status_string());
  }

  // Returns an unowned client end to the underlying exposed directory.
  fidl::UnownedClientEnd<fuchsia_io::Directory> exposed_dir() const ZX_AVAILABLE_SINCE(NEXT);

  // Returns reference to underlying exposed directory handle.
  const fuchsia::io::DirectorySyncPtr& exposed() const ZX_AVAILABLE_SINCE(11);

  // Starts the component with the provided start arguments (if any).
  ExecutionController Start(fuchsia::component::StartChildArgs = {}) const;

 private:
  friend class RealmBuilder;

  static ScopedChild NewFromRealm(fidl::ClientEnd<fuchsia_component::Realm> realm,
                                  std::string collection, std::optional<std::string> name,
                                  std::string url);

  static ScopedChild NewFromClientEnd(std::string collection, std::optional<std::string> name,
                                      std::string url, fidl::ClientEnd<fuchsia_io::Directory> svc);

  void TeardownNatural(async_dispatcher_t* dispatcher, NaturalTeardownCallback callback);

  ScopedChild(fidl::ClientEnd<fuchsia_io::Directory> svc,
              fuchsia_component_decl::ChildRef child_ref,
              fidl::ClientEnd<fuchsia_io::Directory> exposed_dir,
              fidl::SyncClient<fuchsia_component::Controller> controller_proxy);

  // Invalid iff `this` has been moved OR `Teardown` has been called.
  fidl::ClientEnd<fuchsia_io::Directory> svc_;
  fuchsia_component_decl::ChildRef child_ref_;
  fuchsia::io::DirectorySyncPtr exposed_dir_;
  fidl::SyncClient<fuchsia_component::Controller> controller_proxy_;
};

}  // namespace component_testing

#endif  // LIB_SYS_COMPONENT_CPP_TESTING_SCOPED_CHILD_H_
