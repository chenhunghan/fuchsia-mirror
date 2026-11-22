// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "lib/component/testing/cpp/service_directory_provider.h"

#include <fidl/fuchsia.io/cpp/wire.h>
#include <zircon/assert.h>

#include <memory>
#include <string>
#include <utility>

namespace component::testing {

ServiceDirectoryProvider::ServiceDirectoryProvider(async_dispatcher_t* dispatcher)
    : dispatcher_(dispatcher) {
  ZX_ASSERT_MSG(dispatcher != nullptr, "ServiceDirectoryProvider requires a dispatcher.");
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_io::Directory>::Create();
  zx::result<> result = Serve(std::move(server_end));
  ZX_ASSERT_MSG(result.is_ok(), "Failed to serve service directory: %s", result.status_string());
  client_end_ = std::move(client_end);
}

ServiceDirectoryProvider::~ServiceDirectoryProvider() = default;

zx::result<> ServiceDirectoryProvider::AddEntry(std::string_view name,
                                                std::unique_ptr<vfs::Node> node) {
  if (name.empty()) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  return zx::make_result(dir_.AddEntry(std::string(name), std::move(node)));
}

zx::result<> ServiceDirectoryProvider::RemoveProtocol(std::string_view name) {
  std::string entry(name);
  if (zx_status_t status = dir_.RemoveEntry(entry); status != ZX_OK) {
    return zx::error(status);
  }
  // Destroying the binding group closes the connections bound by this object, if any.
  owned_bindings_.erase(entry);
  return zx::ok();
}

zx::result<fidl::ClientEnd<fuchsia_io::Directory>> ServiceDirectoryProvider::Clone() const {
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_io::Directory>::Create();
  if (zx::result<> result = Serve(std::move(server_end)); result.is_error()) {
    return result.take_error();
  }
  return zx::ok(std::move(client_end));
}

zx::result<> ServiceDirectoryProvider::Serve(
    fidl::ServerEnd<fuchsia_io::Directory> server_end) const {
  return zx::make_result(
      dir_.Serve(fuchsia_io::wire::kPermReadable, std::move(server_end), dispatcher_));
}

}  // namespace component::testing
