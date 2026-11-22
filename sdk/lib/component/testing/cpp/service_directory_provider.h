// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_COMPONENT_TESTING_CPP_SERVICE_DIRECTORY_PROVIDER_H_
#define LIB_COMPONENT_TESTING_CPP_SERVICE_DIRECTORY_PROVIDER_H_

#include <fidl/fuchsia.io/cpp/markers.h>
#include <lib/async/dispatcher.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fidl/cpp/wire/channel.h>
#include <lib/fidl/cpp/wire/server.h>
#include <lib/fidl/cpp/wire/service_handler.h>
#include <lib/fidl/cpp/wire/traits.h>
#include <lib/vfs/cpp/node.h>
#include <lib/vfs/cpp/pseudo_dir.h>
#include <lib/vfs/cpp/service.h>
#include <lib/zx/result.h>
#include <zircon/errors.h>

#include <map>
#include <memory>
#include <string>
#include <string_view>
#include <type_traits>
#include <utility>

namespace component::testing {

// Serves a fake service directory (a directory shaped like a component's `/svc`) for unit tests
// written against the new C++ FIDL bindings. This is the natural-bindings counterpart of
// `sys::testing::ServiceDirectoryProvider` /
// `ComponentContextProvider::service_directory_provider`.
//
// A test injects fakes with |AddProtocol| and hands the directory to the code under test as a
// `fidl::ClientEnd<fuchsia_io::Directory>` (see |Clone|) or a
// `fidl::UnownedClientEnd<fuchsia_io::Directory>` (see |client_end|). Code under test then
// connects to the fakes with `component::ConnectAt`. The same object can stand in for an outgoing
// directory when the code under test exposes `fidl::ProtocolHandler`s: add them with
// |AddProtocol| and connect to them with |Connect|.
//
// All connection requests are processed on |dispatcher|, so a test driving an `async::TestLoop`
// must run the loop (e.g. `RunLoopUntilIdle()`) before a fake observes a connection.
//
// # Thread safety
//
// This class is thread-unsafe. Calls to `ServiceDirectoryProvider` methods must not be made
// concurrently across threads. |dispatcher| must be a single-threaded, synchronized dispatcher.
// If |dispatcher| is served by a background thread, that dispatcher must be stopped before
// mutating or destroying the provider.
//
// # Example
//
// ```
// class MyTest : public gtest::TestLoopFixture {
//  protected:
//   FakeFoo fake_foo_;  // A fidl::Server<fuchsia_foo::Foo>.
//   component::testing::ServiceDirectoryProvider svc_provider_{dispatcher()};
// };
//
// TEST_F(MyTest, Connects) {
//   ASSERT_TRUE(svc_provider_.AddProtocol<fuchsia_foo::Foo>(&fake_foo_).is_ok());
//   CodeUnderTest code_under_test(dispatcher(), svc_provider_.Clone().value());
//   RunLoopUntilIdle();
//   ...
// }
// ```
class ServiceDirectoryProvider final {
 public:
  // |dispatcher| must not be null. It serves the directory and, for protocols added with the
  // server implementation overload of |AddProtocol|, the bindings.
  explicit ServiceDirectoryProvider(async_dispatcher_t* dispatcher);

  // Closes all connections to the directory and the bindings this object owns.
  ~ServiceDirectoryProvider();

  ServiceDirectoryProvider(const ServiceDirectoryProvider&) = delete;
  ServiceDirectoryProvider& operator=(const ServiceDirectoryProvider&) = delete;
  ServiceDirectoryProvider(ServiceDirectoryProvider&&) = delete;
  ServiceDirectoryProvider& operator=(ServiceDirectoryProvider&&) = delete;

  // Adds an entry named |name| that invokes |handler| on |dispatcher()| for every connection.
  //
  // The caller owns the connections |handler| creates; they are unaffected by |RemoveProtocol|
  // and by the destruction of this object. A common pattern is
  //
  // ```
  // fidl::ServerBindingGroup<fuchsia_foo::Foo> bindings_;
  // svc_provider_.AddProtocol<fuchsia_foo::Foo>(
  //     bindings_.CreateHandler(&fake_foo_, dispatcher(), fidl::kIgnoreBindingClosure));
  // ```
  //
  // # Errors
  //
  // ZX_ERR_ALREADY_EXISTS: An entry named |name| already exists.
  //
  // ZX_ERR_INVALID_ARGS: |handler| is null or |name| is empty.
  template <typename Protocol, typename = std::enable_if_t<fidl::IsProtocolV<Protocol>>>
  zx::result<> AddProtocol(fidl::ProtocolHandler<Protocol> handler,
                           std::string_view name = fidl::DiscoverableProtocolName<Protocol>) {
    if (!handler) {
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    return AddEntry(
        name, std::make_unique<vfs::Service>(
                  [handler = std::move(handler)](fidl::ServerEnd<Protocol> server_end) mutable {
                    handler(std::move(server_end));
                  }));
  }

  // Adds an entry named |name| whose connections are bound to |impl| on |dispatcher()|.
  // |ServerImpl| may be a `fidl::Server<Protocol>` or a `fidl::WireServer<Protocol>`.
  //
  // This object owns the bindings: |RemoveProtocol| and the destruction of this object close all
  // connections to |impl|. |impl| must outlive the bindings, i.e. |impl| must be destroyed after
  // this object or after |RemoveProtocol| returns. Use the |fidl::ProtocolHandler| overload to
  // keep ownership of the bindings, e.g. to observe binding closure or to close connections
  // manually.
  //
  // # Errors
  //
  // ZX_ERR_ALREADY_EXISTS: An entry named |name| already exists.
  //
  // ZX_ERR_INVALID_ARGS: |impl| is null or |name| is empty.
  template <typename Protocol, typename ServerImpl,
            typename = std::enable_if_t<fidl::IsProtocolV<Protocol>>>
  zx::result<> AddProtocol(ServerImpl* impl,
                           std::string_view name = fidl::DiscoverableProtocolName<Protocol>) {
    if (impl == nullptr) {
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    auto bindings = std::make_unique<TypedBindingGroup<Protocol>>();
    zx::result<> result = AddProtocol<Protocol>(
        bindings->group.CreateHandler(impl, dispatcher_, fidl::kIgnoreBindingClosure), name);
    if (result.is_ok()) {
      owned_bindings_.emplace(std::string(name), std::move(bindings));
    }
    return result;
  }

  // Removes the entry named |name|. Later connection attempts fail with ZX_ERR_NOT_FOUND.
  //
  // Connections bound by the server implementation overload of |AddProtocol| are closed;
  // connections created by a |fidl::ProtocolHandler| are left untouched.
  //
  // # Errors
  //
  // ZX_ERR_NOT_FOUND: There is no entry named |name|.
  template <typename Protocol, typename = std::enable_if_t<fidl::IsProtocolV<Protocol>>>
  zx::result<> RemoveProtocol(std::string_view name = fidl::DiscoverableProtocolName<Protocol>) {
    return RemoveProtocol(name);
  }

  // Same as above but untyped.
  zx::result<> RemoveProtocol(std::string_view name);

  // Borrows this object's own connection to the directory. Valid for the lifetime of this object.
  fidl::UnownedClientEnd<fuchsia_io::Directory> client_end() const { return client_end_.borrow(); }

  // Returns a new, independent connection to the directory, e.g. to hand ownership of the
  // service directory to the code under test.
  zx::result<fidl::ClientEnd<fuchsia_io::Directory>> Clone() const;

  // Serves the directory on |server_end|, e.g. a server end obtained from the code under test or
  // one that is about to be installed in a namespace.
  zx::result<> Serve(fidl::ServerEnd<fuchsia_io::Directory> server_end) const;

  // Connects to the entry named |name|, e.g. to drive a protocol published by the code under test
  // through |AddProtocol|.
  //
  // The connection completes asynchronously on |dispatcher()|: a `zx::ok()` result does not mean
  // the entry exists.
  template <typename Protocol, typename = std::enable_if_t<fidl::IsProtocolV<Protocol>>>
  zx::result<fidl::ClientEnd<Protocol>> Connect(
      std::string_view name = fidl::DiscoverableProtocolName<Protocol>) const {
    return component::ConnectAt<Protocol>(client_end(), name);
  }

  // Same as above but uses the given |server_end|.
  template <typename Protocol, typename = std::enable_if_t<fidl::IsProtocolV<Protocol>>>
  zx::result<> Connect(fidl::ServerEnd<Protocol> server_end,
                       std::string_view name = fidl::DiscoverableProtocolName<Protocol>) const {
    return component::ConnectAt<Protocol>(client_end(), std::move(server_end), name);
  }

  async_dispatcher_t* dispatcher() const { return dispatcher_; }

 private:
  struct OwnedBindingGroup {
    virtual ~OwnedBindingGroup() = default;
  };

  template <typename Protocol>
  struct TypedBindingGroup final : OwnedBindingGroup {
    fidl::ServerBindingGroup<Protocol> group;
  };

  zx::result<> AddEntry(std::string_view name, std::unique_ptr<vfs::Node> node);

  async_dispatcher_t* dispatcher_;

  // Binding groups created by the server implementation overload of |AddProtocol|, keyed by entry
  // name. Declared before |dir_| so that the directory, whose service nodes reference the groups,
  // is destroyed first.
  std::map<std::string, std::unique_ptr<OwnedBindingGroup>, std::less<>> owned_bindings_;

  vfs::PseudoDir dir_;
  fidl::ClientEnd<fuchsia_io::Directory> client_end_;
};

}  // namespace component::testing

#endif  // LIB_COMPONENT_TESTING_CPP_SERVICE_DIRECTORY_PROVIDER_H_
