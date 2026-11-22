// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/loader_service/loader_service.h"

#include <fidl/fuchsia.io/cpp/fidl.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/io.h>
#include <lib/fdio/unsafe.h>
#include <lib/syslog/cpp/macros.h>
#include <limits.h>
#include <zircon/errors.h>

#include <mutex>

#include "src/lib/files/path.h"
#include "src/lib/fxl/strings/string_printf.h"

namespace loader {

namespace fio = fuchsia_io;

LoaderServiceBase::LoaderServiceBase(async_dispatcher_t* dispatcher, std::string name)
    : dispatcher_(dispatcher),
      name_(std::move(name)),
      log_prefix_(fxl::StringPrintf("ldsvc (%s): ", name_.data())) {}

LoaderServiceBase::~LoaderServiceBase() = default;

void LoaderServiceBase::LoadObjectAsync(std::string path,
                                        fit::callback<void(zx::result<zx::vmo>)> callback) {
  callback(LoadObjectImpl(std::move(path)));
}

zx::result<fidl::ClientEnd<fuchsia_ldsvc::Loader>> LoaderServiceBase::Connect() {
  auto [client, server] = fidl::Endpoints<fuchsia_ldsvc::Loader>::Create();
  Bind(std::move(server));
  return zx::ok(std::move(client));
}

void LoaderServiceBase::Bind(fidl::ServerEnd<fuchsia_ldsvc::Loader> channel) {
  // Each connection gets a strong (shared_ptr) reference to the server, which keeps the overall
  // service alive as long as there is one open connection even if the original reference is
  // dropped.
  auto conn = std::make_unique<LoaderConnection>(shared_from_this());
  // This returns a ServerBindingRef, but we don't need it since we don't currently need a way
  // to unbind connections from the server side. Dropping it does not automatically unbind.
  fidl::BindServer(
      dispatcher_, std::move(channel), std::move(conn),
      [](LoaderConnection* connection, fidl::UnbindInfo info,
         fidl::ServerEnd<fuchsia_ldsvc::Loader> server_end) {
        if (info.is_peer_closed() || info.is_user_initiated() || info.is_dispatcher_shutdown()) {
          // Client broke away or server shutting down.
          return;
        }
        FX_LOGS(ERROR) << "Loader connection error: " << info;
      });
}

void LoaderConnection::Done(DoneCompleter::Sync& completer) { completer.Close(ZX_OK); }

void LoaderConnection::LoadObject(LoadObjectRequestView request,
                                  LoadObjectCompleter::Sync& completer) {
  std::string name(request->object_name.data(), request->object_name.size());
  std::shared_ptr<LoaderServiceBase> server = server_;
  LoadConfig config = config_;
  auto async_completer = completer.ToAsync();

  auto reply = [](const std::shared_ptr<LoaderServiceBase>& server, const std::string& name,
                  LoadObjectCompleter::Async completer, zx::result<zx::vmo> status) {
    // Generally we wouldn't want to log in a library, but these logs have proven to be useful in
    // past, and the new loader name in the prefix will make them moreso.
    if (status.status_value() == ZX_ERR_NOT_FOUND) {
      FX_LOGS(WARNING) << server->log_prefix() << "could not find '" << name << "'";
    }

    completer.Reply(status.status_value(), std::move(status).value_or(zx::vmo()));
    fidl::Status result = completer.result_of_reply();
    if (!result.ok() && result.status() != ZX_ERR_PEER_CLOSED &&
        result.status() != ZX_ERR_CANCELED) {
      FX_LOGS(WARNING) << server->log_prefix() << "failed to reply to LoadObject(" << name
                       << "): " << result.error();
    }
  };

  if (config.subdir.empty()) {
    server->LoadObjectAsync(name, [server, name, completer = std::move(async_completer),
                                   reply](zx::result<zx::vmo> status) mutable {
      reply(server, name, std::move(completer), std::move(status));
    });
    return;
  }

  // If subdir is non-empty, the loader should search this subdirectory for the object first. If
  // exclusive is also true, only subdir should be searched.
  std::string prefixed_name = files::JoinPath(config.subdir, name);
  bool exclusive = config.exclusive;
  server->LoadObjectAsync(std::move(prefixed_name), [server, name = std::move(name), exclusive,
                                                     completer = std::move(async_completer),
                                                     reply](zx::result<zx::vmo> status) mutable {
    if (status.is_error() && status.status_value() != ZX_ERR_CANCELED && !exclusive) {
      server->LoadObjectAsync(name, [server, name, completer = std::move(completer),
                                     reply](zx::result<zx::vmo> fallback_status) mutable {
        reply(server, name, std::move(completer), std::move(fallback_status));
      });
      return;
    }
    reply(server, name, std::move(completer), std::move(status));
  });
}

void LoaderConnection::Config(ConfigRequestView request, ConfigCompleter::Sync& completer) {
  // fidl::StringView is not null-terminated so must pass size to std::string constructor.
  std::string config_str(request->config.data(), request->config.size());

  auto reply = [this, &config_str, &completer](zx_status_t status) {
    completer.Reply(status);
    fidl::Status result = completer.result_of_reply();
    if (!result.ok()) {
      FX_LOGS(WARNING) << log_prefix() << "failed to reply to Config(" << config_str
                       << "): " << result.error();
    }
  };

  // Config strings must not contain path separators.
  if (cpp23::contains(config_str, '/')) {
    reply(ZX_ERR_INVALID_ARGS);
    return;
  }

  // The config string is a single subdirectory name to be searched for objects first, optionally
  // followed by a '!' character, which indicates that only the subdirectory should be searched.
  bool exclusive = false;
  if (!config_str.empty() && config_str.back() == '!') {
    exclusive = true;
    config_str.pop_back();

    // Make sure config wasn't "!" (though just "" is ok to reset config)
    if (config_str.empty()) {
      reply(ZX_ERR_INVALID_ARGS);
      return;
    }
  }
  config_ = LoadConfig{.subdir = config_str, .exclusive = exclusive};
  reply(ZX_OK);
}

void LoaderConnection::Clone(CloneRequestView request, CloneCompleter::Sync& completer) {
  server_->Bind(std::move(request->loader));
  completer.Reply(ZX_OK);
}

// static
std::shared_ptr<LoaderService> LoaderService::Create(async_dispatcher_t* dispatcher,
                                                     fbl::unique_fd lib_dir, std::string name) {
  // Can't use make_shared because constructor is protected
  return std::shared_ptr<LoaderService>(
      new LoaderService(dispatcher, std::move(lib_dir), std::move(name)));
}

zx::result<zx::vmo> LoaderService::LoadObjectImpl(std::string path) {
  const fio::Flags kFlags = fio::Flags::kProtocolFile | fio::kPermReadable | fio::kPermExecutable;

  fbl::unique_fd fd;
  zx_status_t status = fdio_open3_fd_at(dir_.get(), path.data(), static_cast<uint64_t>(kFlags),
                                        fd.reset_and_get_address());
  if (status != ZX_OK) {
    return zx::error(status);
  }

  zx::vmo vmo;
  status = fdio_get_vmo_exec(fd.get(), vmo.reset_and_get_address());
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(std::move(vmo));
}

void LoaderService::LoadObjectAsync(std::string path,
                                    fit::callback<void(zx::result<zx::vmo>)> callback) {
  if (path.empty()) {
    callback(zx::error(ZX_ERR_NOT_FOUND));
    return;
  }
  // Reject overlong paths, embedded NULs, and leading '/' upfront before creating the File
  // channel (matching fdio_validate_path and fdio_open3_fd_at's allow_absolute_path=false check,
  // since VFS Directory.Open otherwise treats "/" as "." and strips a single leading '/').
  if (path.size() >= PATH_MAX || path.size() > fio::wire::kMaxPathLength ||
      path.find('\0') != std::string::npos || path.front() == '/') {
    callback(zx::error(ZX_ERR_INVALID_ARGS));
    return;
  }
  if (path.back() == '/') {
    callback(zx::error(ZX_ERR_NOT_DIR));
    return;
  }

  if (dispatcher() == nullptr) {
    callback(LoadObjectImpl(std::move(path)));
    return;
  }

  fdio_t* io = fdio_unsafe_fd_to_io(dir_.get());
  if (io == nullptr) {
    callback(LoadObjectImpl(std::move(path)));
    return;
  }
  zx_handle_t dir_chan = fdio_unsafe_borrow_channel(io);
  if (dir_chan == ZX_HANDLE_INVALID) {
    fdio_unsafe_release(io);
    callback(LoadObjectImpl(std::move(path)));
    return;
  }

  struct OpenState {
    std::mutex mu;
    zx_status_t status = ZX_OK;
  };

  constexpr fio::Flags kFlags =
      fio::Flags::kProtocolFile | fio::kPermReadable | fio::kPermExecutable;
  auto [client_end, server_end] = fidl::Endpoints<fio::File>::Create();
  auto open_state = std::make_shared<OpenState>();

  // Bind client_end and enqueue GetBackingMemory BEFORE sending server_end to Directory.Open.
  // While server_end is still held locally, SendTwoWay cannot race with VFS closing server_end,
  // ensuring AsyncClientBinding registers the pending transaction and deterministically reads any
  // VFS epitaph (e.g. ZX_ERR_NOT_FOUND, ZX_ERR_ACCESS_DENIED, ZX_ERR_NOT_FILE, ZX_ERR_INVALID_ARGS)
  // in a single pipelined round-trip.
  fidl::WireSharedClient<fio::File> file_client(std::move(client_end), dispatcher());
  file_client
      ->GetBackingMemory(fio::wire::VmoFlags::kRead | fio::wire::VmoFlags::kExecute |
                         fio::wire::VmoFlags::kPrivateClone)
      .ThenExactlyOnce([self = shared_from_this(), client_keepalive = file_client.Clone(),
                        open_state, callback = std::move(callback)](
                           fidl::WireUnownedResult<fio::File::GetBackingMemory>& result) mutable {
        if (!result.ok()) {
          zx_status_t open_status = ZX_OK;
          {
            std::scoped_lock lock(open_state->mu);
            open_status = open_state->status;
          }
          callback(zx::error(open_status != ZX_OK ? open_status : result.status()));
          return;
        }
        if (result->is_error()) {
          callback(zx::error(result->error_value()));
          return;
        }
        callback(zx::ok(std::move(result->value()->vmo)));
      });

  // Hold open_state->mu across fdio_open3_at so that if the one-way Directory.Open write fails
  // synchronously (closing server_end inside fdio_open3_at), a multi-threaded dispatcher waking on
  // ZX_CHANNEL_PEER_CLOSED cannot observe open_state->status before it is recorded.
  {
    std::unique_lock lock(open_state->mu);
    zx_status_t status = fdio_open3_at(dir_chan, path.c_str(), static_cast<uint64_t>(kFlags),
                                       server_end.TakeChannel().release());
    if (status != ZX_OK) {
      open_state->status = status;
    }
  }
  fdio_unsafe_release(io);
}

}  // namespace loader
