// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_manager/dictionary_util.h"

#include <fidl/fuchsia.component.runtime/cpp/common_types_format.h>
#include <fidl/fuchsia.component.runtime/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/component/incoming/cpp/directory.h>
#include <lib/component/incoming/cpp/directory_watcher.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fdio/directory.h>

#include "src/devices/bin/driver_manager/async_sharder.h"
#include "src/devices/lib/log/log.h"

namespace driver_manager {

namespace {

zx::result<fidl::ClientEnd<fuchsia_component_runtime::Capabilities>> ConnectToCapabilities() {
  return component::Connect<fuchsia_component_runtime::Capabilities>();
}

}  // namespace

zx::result<zx::eventpair> DuplicateEventpair(const zx::eventpair& handle) {
  zx::eventpair duplicate;
  if (zx_status_t status = handle.duplicate(ZX_RIGHT_SAME_RIGHTS, &duplicate); status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(std::move(duplicate));
}

DictionaryUtil::DictionaryUtil(
    fidl::ClientEnd<fuchsia_component_runtime::Capabilities> capabilities,
    async_dispatcher_t* dispatcher, CapabilitiesConnector connector)
    : capabilities_(std::move(capabilities), dispatcher),
      connector_(connector ? std::move(connector) : CapabilitiesConnector(ConnectToCapabilities)),
      dispatcher_(dispatcher) {}

void DictionaryUtil::CopyDictionary(const zx::eventpair& dictionary,
                                    fit::callback<void(zx::result<zx::eventpair>)> callback) {
  zx::result source = DuplicateEventpair(dictionary);
  if (source.is_error()) {
    fdf_log::error("Failed to duplicate dictionary handle: {}", source.status_string());
    callback(source.take_error());
    return;
  }
  zx::eventpair copy, copy_peer;
  if (zx_status_t status = zx::eventpair::create(0, &copy, &copy_peer); status != ZX_OK) {
    callback(zx::error(status));
    return;
  }

  capabilities_->DictionaryCopy(std::move(source.value()), std::move(copy_peer))
      .Then([copy = std::move(copy), callback = std::move(callback)](
                fidl::WireUnownedResult<fuchsia_component_runtime::Capabilities::DictionaryCopy>&
                    result) mutable {
        if (!result.ok()) {
          fdf_log::error("Failed to copy dictionary: {}", result.FormatDescription());
          callback(zx::error(ZX_ERR_INTERNAL));
          return;
        }
        if (result->is_error()) {
          fdf_log::error("Failed to copy dictionary: {}", result->error_value());
          callback(zx::error(ZX_ERR_INTERNAL));
          return;
        }
        callback(zx::ok(std::move(copy)));
      });
}

void DictionaryUtil::DictionaryDirConnectorOpen(
    const zx::eventpair& dictionary, std::string_view key,
    fit::callback<void(zx::result<fidl::ClientEnd<fuchsia_io::Directory>>)> callback) {
  zx::result dict_dup = DuplicateEventpair(dictionary);
  if (dict_dup.is_error()) {
    fdf_log::error("Failed to duplicate dictionary handle: {}", dict_dup.status_string());
    callback(dict_dup.take_error());
    return;
  }
  zx::eventpair cap, cap_peer;
  if (zx_status_t status = zx::eventpair::create(0, &cap, &cap_peer); status != ZX_OK) {
    callback(zx::error(status));
    return;
  }
  capabilities_
      ->DictionaryGet(std::move(dict_dup.value()), fidl::StringView::FromExternal(key),
                      std::move(cap_peer))
      .Then([this, key = std::string(key), callback = std::move(callback), cap = std::move(cap)](
                fidl::WireUnownedResult<fuchsia_component_runtime::Capabilities::DictionaryGet>&
                    result) mutable {
        if (!result.ok()) {
          fdf_log::error("Failed to get '{}' from dictionary: {}", key, result.FormatDescription());
          callback(zx::error(ZX_ERR_INTERNAL));
          return;
        }
        if (result->is_error()) {
          fdf_log::error("Failed to get '{}' from dictionary: {}", key, result->error_value());
          callback(zx::error(ZX_ERR_INTERNAL));
          return;
        }

        auto capability_type = result->value()->capability_type;
        if (capability_type ==
            fuchsia_component_runtime::wire::CapabilityType::kDirConnectorRouter) {
          RouteDirConnectorRouter(std::move(cap), std::move(key), std::move(callback));
        } else if (capability_type ==
                   fuchsia_component_runtime::wire::CapabilityType::kDirConnector) {
          OpenDirConnector(std::move(cap), std::move(callback));
        } else {
          fdf_log::error("Dictionary entry '{}' has unexpected type {}", key, capability_type);
          callback(zx::error(ZX_ERR_WRONG_TYPE));
        }
      });
}

void DictionaryUtil::RouteDirConnectorRouter(
    zx::eventpair router, std::string key,
    fit::callback<void(zx::result<fidl::ClientEnd<fuchsia_io::Directory>>)> callback) {
  // Component manager serves each fuchsia.component.runtime.Capabilities connection one request at
  // a time and waits for routes to finish before reading the next request. Routing on
  // |capabilities_| would let a slow route stall every other dictionary operation, so each route
  // gets its own connection.
  zx::result client_end = connector_();
  if (client_end.is_error()) {
    fdf_log::error("Failed to connect to fuchsia.component.runtime.Capabilities to route '{}': {}",
                   key, client_end.status_string());
    callback(client_end.take_error());
    return;
  }
  fidl::WireClient<fuchsia_component_runtime::Capabilities> route_client(
      std::move(client_end.value()), dispatcher_);

  zx::eventpair token, token_peer;
  if (zx_status_t status = zx::eventpair::create(0, &token, &token_peer); status != ZX_OK) {
    callback(zx::error(status));
    return;
  }
  // The instance token identifies driver manager as the component the route is performed for.
  route_client->InstanceTokenCreate(std::move(token_peer))
      .Then(
          [this, route_client = std::move(route_client), router = std::move(router),
           token = std::move(token), key = std::move(key), callback = std::move(callback)](
              fidl::WireUnownedResult<fuchsia_component_runtime::Capabilities::InstanceTokenCreate>&
                  result) mutable {
            if (!result.ok()) {
              fdf_log::error("Failed to create an instance token to route '{}': {}", key,
                             result.FormatDescription());
              callback(zx::error(ZX_ERR_INTERNAL));
              return;
            }
            if (result->is_error()) {
              fdf_log::error("Failed to create an instance token to route '{}': {}", key,
                             result->error_value());
              callback(zx::error(ZX_ERR_INTERNAL));
              return;
            }

            zx::eventpair connector, connector_peer;
            if (zx_status_t status = zx::eventpair::create(0, &connector, &connector_peer);
                status != ZX_OK) {
              callback(zx::error(status));
              return;
            }
            // An empty request makes the router use the request defined by its declaration.
            route_client
                ->DirConnectorRouterRoute(std::move(router),
                                          fuchsia_component_runtime::wire::RouteRequest{},
                                          std::move(token), std::move(connector_peer))
                .Then([this, route_client = std::move(route_client),
                       connector = std::move(connector), key = std::move(key),
                       callback = std::move(callback)](
                          fidl::WireUnownedResult<
                              fuchsia_component_runtime::Capabilities::DirConnectorRouterRoute>&
                              result) mutable {
                  if (!result.ok()) {
                    fdf_log::error("Failed to route '{}': {}", key, result.FormatDescription());
                    callback(zx::error(ZX_ERR_INTERNAL));
                    return;
                  }
                  if (result->is_error()) {
                    fdf_log::error("Failed to route '{}': {}", key,
                                   zx_status_get_string(result->error_value()));
                    callback(zx::error(ZX_ERR_INTERNAL));
                    return;
                  }
                  if (result->value()->response !=
                      fuchsia_component_runtime::wire::RouterResponse::kSuccess) {
                    fdf_log::warn("Routing '{}' did not produce a capability: {}", key,
                                  result->value()->response);
                    callback(zx::error(ZX_ERR_UNAVAILABLE));
                    return;
                  }
                  OpenDirConnector(std::move(connector), std::move(callback));
                });
          });
}

void DictionaryUtil::CreateDictionaryWith(
    std::unordered_map<std::string, fidl::ClientEnd<fuchsia_component_runtime::DirReceiver>>
        receivers,
    fit::callback<void(zx::result<zx::eventpair>)> callback) {
  zx::eventpair dest, dest_peer;
  if (zx_status_t status = zx::eventpair::create(0, &dest, &dest_peer); status != ZX_OK) {
    callback(zx::error(status));
    return;
  }
  capabilities_->DictionaryCreate(std::move(dest_peer))
      .Then([this, callback = std::move(callback), dest = std::move(dest),
             receivers = std::move(receivers)](
                fidl::WireUnownedResult<fuchsia_component_runtime::Capabilities::DictionaryCreate>&
                    result) mutable {
        if (!result.ok()) {
          fdf_log::error("Failed to create dictionary: {}", result.FormatDescription());
          callback(zx::error(ZX_ERR_INTERNAL));
          return;
        }
        if (result->is_error()) {
          fdf_log::error("Failed to create dictionary: {}", result->error_value());
          callback(zx::error(ZX_ERR_INTERNAL));
          return;
        }

        // The sharder below would never complete without any shards.
        if (receivers.empty()) {
          callback(zx::ok(std::move(dest)));
          return;
        }

        auto shared_dest = std::make_shared<zx::eventpair>(std::move(dest));
        std::shared_ptr<AsyncSharder> sharder = std::make_shared<AsyncSharder>(
            receivers.size(),
            [callback = std::move(callback), shared_dest](zx::result<> result) mutable {
              if (result.is_error()) {
                callback(result.take_error());
                return;
              }
              callback(zx::ok(std::move(*shared_dest)));
            });

        for (auto& [key, receiver] : receivers) {
          zx::eventpair conn, conn_peer;
          if (zx_status_t status = zx::eventpair::create(0, &conn, &conn_peer); status != ZX_OK) {
            sharder->CompleteShardError(status);
            continue;
          }
          capabilities_->DirConnectorCreate(std::move(conn_peer), std::move(receiver))
              .Then([this, shared_dest, key, conn = std::move(conn),
                     sharder](fidl::WireUnownedResult<
                              fuchsia_component_runtime::Capabilities::DirConnectorCreate>&
                                  result) mutable {
                if (!result.ok()) {
                  fdf_log::error("Failed to create dir connector: {}", result.FormatDescription());
                  sharder->CompleteShardError(ZX_ERR_INTERNAL);
                  return;
                }
                if (result->is_error()) {
                  fdf_log::error("Failed to create dir connector: {}", result->error_value());
                  sharder->CompleteShardError(ZX_ERR_INTERNAL);
                  return;
                }

                zx::result dest_dup = DuplicateEventpair(*shared_dest);
                if (dest_dup.is_error()) {
                  sharder->CompleteShardError(dest_dup.status_value());
                  return;
                }

                capabilities_
                    ->DictionaryInsert(std::move(dest_dup.value()),
                                       fidl::StringView::FromExternal(key), std::move(conn))
                    .Then(
                        [sharder,
                         key](fidl::WireUnownedResult<
                              fuchsia_component_runtime::Capabilities::DictionaryInsert>& result) {
                          if (!result.ok()) {
                            fdf_log::error("Failed to insert '{}' into dictionary: {}", key,
                                           result.FormatDescription());
                            sharder->CompleteShardError(ZX_ERR_INTERNAL);
                            return;
                          }
                          if (result->is_error()) {
                            fdf_log::error("Failed to insert '{}' into dictionary: {}", key,
                                           result->error_value());
                            sharder->CompleteShardError(ZX_ERR_INTERNAL);
                            return;
                          }

                          sharder->CompleteShard();
                        });
              });
        }
      });
}

void DictionaryUtil::OpenDirConnector(
    zx::eventpair connector,
    fit::callback<void(zx::result<fidl::ClientEnd<fuchsia_io::Directory>>)> callback) {
  auto [client, server] = fidl::Endpoints<fuchsia_io::Directory>::Create();
  fidl::Arena arena;
  capabilities_
      ->DirConnectorOpen(
          fuchsia_component_runtime::wire::CapabilitiesDirConnectorOpenRequest::Builder(arena)
              .dir_connector(std::move(connector))
              .channel(std::move(server))
              .flags(fuchsia_io::Flags::kProtocolDirectory | fuchsia_io::kPermReadable)
              .Build())
      .Then([callback = std::move(callback), client = std::move(client)](
                fidl::WireUnownedResult<fuchsia_component_runtime::Capabilities::DirConnectorOpen>&
                    result) mutable {
        if (!result.ok()) {
          fdf_log::error("Failed to open dir connector: {}", result.FormatDescription());
          callback(zx::error(ZX_ERR_INTERNAL));
          return;
        }
        if (result->is_error()) {
          fdf_log::error("Failed to open dir connector: {}", result->error_value());
          callback(zx::error(ZX_ERR_INTERNAL));
          return;
        }

        callback(zx::ok(std::move(client)));
      });
}

// Component manager always provides |rights|. When the requester did not specify any flags it
// passes PERM_READABLE | PERM_INHERIT_WRITE | PERM_INHERIT_EXECUTE, without a protocol, which
// lets the server pick the protocol of the node being opened.
void DirReceiverImpl::Receive(ReceiveRequestView request, ReceiveCompleter::Sync& completer) {
  if (request->path.empty() || request->path.get() == ".") {
    if (dir_infos_.size() != 1 || !dir_infos_[0].is_primary) {
      fdf_log::error("invalid subdir");
      return;
    }

    zx_handle_t directory = dir_infos_[0].dir.handle()->get();
    zx_status_t status = fdio_open3_at(directory, "/", uint64_t{request->rights},
                                       request->channel.TakeChannel().release());
    if (status != ZX_OK) {
      fdf_log::error("Failed to open directory: {}", status);
    }

    return;
  }

  auto subdir = std::string(request->path.get());
  auto slash = subdir.find('/');
  if (slash == std::string::npos) {
    fdf_log::error("invalid subdir");
    return;
  }

  std::string instance = subdir.substr(0, slash);
  std::string proto = subdir.substr(slash + 1);
  zx_handle_t directory = ZX_HANDLE_INVALID;
  std::string source_instance;

  for (auto& dir_info : dir_infos_) {
    if (instance == "default") {
      if (dir_info.is_primary) {
        source_instance = dir_info.target_to_source_instance_mapping["default"];
        directory = dir_info.dir.handle()->get();
        break;
      }
    } else if (dir_info.parent_name == instance) {
      source_instance = dir_info.target_to_source_instance_mapping["default"];
      directory = dir_info.dir.handle()->get();
      break;
    } else {
      if (dir_info.target_to_source_instance_mapping.contains(instance)) {
        source_instance = dir_info.target_to_source_instance_mapping[instance];
        directory = dir_info.dir.handle()->get();
        break;
      }
    }
  }

  if (directory == ZX_HANDLE_INVALID) {
    fdf_log::error("unknown instance");
    return;
  }

  std::string new_subdir = std::format("{}/{}", source_instance, proto);

  // This seems to be an inconsistency in the CF APIs. The requested type is for a
  // fidl::ClientEnd<fuchsia_io::Directory>,
  // but it seems to expect to connect directly to the protocol at the subdir.
  // (aka fidl::ClientEnd<user::Protocol>).
  zx_status_t status = fdio_open3_at(directory, new_subdir.c_str(), uint64_t{request->rights},
                                     request->channel.TakeChannel().release());
  if (status != ZX_OK) {
    fdf_log::error("Failed to open directory: {}", status);
    return;
  }
}

}  // namespace driver_manager
