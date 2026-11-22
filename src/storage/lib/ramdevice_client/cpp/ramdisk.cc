// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <dirent.h>
#include <fcntl.h>
#include <fidl/fuchsia.hardware.ramdisk/cpp/wire.h>
#include <fidl/fuchsia.storage.block/cpp/wire.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/component/incoming/cpp/service.h>
#include <lib/fdio/cpp/caller.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/fd.h>
#include <lib/fdio/fdio.h>
#include <lib/fdio/namespace.h>
#include <lib/fdio/watcher.h>
#include <lib/fit/defer.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zbi-format/partition.h>
#include <lib/zx/channel.h>
#include <lib/zx/job.h>
#include <lib/zx/time.h>
#include <lib/zx/vmo.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>
#include <zircon/process.h>
#include <zircon/processargs.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>
#include <zircon/types.h>

#include <memory>
#include <utility>

#include <fbl/string.h>
#include <fbl/string_printf.h>
#include <fbl/unique_fd.h>
#include <ramdevice-client/ramdisk.h>

namespace ramdevice_client {

struct Ramdisk::Client {
 public:
  DISALLOW_COPY_ASSIGN_AND_MOVE(Client);

  static zx_status_t Create(fidl::ClientEnd<fuchsia_io::Directory> outgoing_directory,
                            zx::eventpair lifeline, std::unique_ptr<Client>* out) {
    auto [svc_dir, server] = fidl::Endpoints<fuchsia_io::Directory>::Create();
    if (zx_status_t status = fdio_open3_at(outgoing_directory.channel().get(), "svc",
                                           uint64_t{fuchsia_io::wire::kPermReadable},
                                           server.TakeChannel().release());
        status != ZX_OK)
      return status;

    zx::result ramdisk_interface = component::ConnectAt<fuchsia_hardware_ramdisk::Ramdisk>(svc_dir);
    if (ramdisk_interface.is_error())
      return ramdisk_interface.status_value();

    // Bind the outgoing directory to the namespace.
    static std::atomic<int> counter;
    // This deliberately binds to the top-level because there is/was watcher code that only worked
    // at the top-level of the local namespace (opening intermediate local directories is not
    // supported).
    std::string bind_path = "/ramdisk-" + std::to_string(counter++);
    fdio_ns_t* ns;
    if (zx_status_t status = fdio_ns_get_installed(&ns); status != ZX_OK)
      return status;
    if (zx_status_t status =
            fdio_ns_bind(ns, bind_path.c_str(), outgoing_directory.TakeHandle().release());
        status != ZX_OK)
      return status;
    fbl::String ram_disk_path = bind_path + "/svc/fuchsia.storage.block.Block";
    *out = std::unique_ptr<Client>(new Client(std::move(bind_path), std::move(ram_disk_path),
                                              *std::move(ramdisk_interface), std::move(lifeline)));
    return ZX_OK;
  }

  zx_status_t Destroy() {
    if (!ramdisk_interface_) {
      return ZX_ERR_BAD_STATE;
    }

    if (!bind_path_.empty()) {
      fdio_ns_t* ns;
      if (fdio_ns_get_installed(&ns) == ZX_OK)
        fdio_ns_unbind(ns, bind_path_.c_str());
      bind_path_.clear();
    }

    return ZX_OK;
  }

  zx::result<fidl::ClientEnd<fuchsia_storage_block::Block>> Connect() const {
    return component::Connect<fuchsia_storage_block::Block>(path());
  }

  fidl::UnownedClientEnd<fuchsia_hardware_ramdisk::Ramdisk> ramdisk_interface() const {
    return ramdisk_interface_.borrow();
  }

  const fbl::String& path() const { return path_; }

  ~Client() { Destroy(); }

  static zx::result<std::unique_ptr<Client>> CreateWithOptions(uint32_t block_size,
                                                               uint64_t block_count,
                                                               const uint8_t* type_guid,
                                                               zx::vmo vmo, int svc_root_fd) {
    fbl::unique_fd dir(svc_root_fd > 0
                           ? (openat(svc_root_fd, "fuchsia.hardware.ramdisk.Service", O_RDONLY))
                           : open("/svc/fuchsia.hardware.ramdisk.Service", O_RDONLY));
    if (!dir)
      return zx::error(ZX_ERR_NOT_FOUND);
    std::string instance_name;
    zx_status_t status = fdio_watch_directory(
        dir.get(),
        [](int, int event, const char* name, void* cookie) {
          if (event == WATCH_EVENT_ADD_FILE && strcmp(name, ".") != 0) {
            *reinterpret_cast<std::string*>(cookie) = name;
            return ZX_ERR_STOP;
          }
          return ZX_OK;
        },
        ZX_TIME_INFINITE, &instance_name);
    if (status != ZX_ERR_STOP)
      return zx::error(status == ZX_OK ? ZX_ERR_NOT_FOUND : status);
    zx::result<fuchsia_hardware_ramdisk::Service::ServiceClient> service;
    if (svc_root_fd > 0) {
      fdio_cpp::UnownedFdioCaller caller(svc_root_fd);
      service = component::OpenServiceAt<fuchsia_hardware_ramdisk::Service>(caller.directory(),
                                                                            instance_name);
    } else {
      service = component::OpenService<fuchsia_hardware_ramdisk::Service>(instance_name);
    }
    if (service.is_error())
      return service.take_error();
    zx::result controller = service->connect_controller();
    if (controller.is_error())
      return controller.take_error();

    fidl::Arena arena;
    fuchsia_hardware_ramdisk::wire::Guid guid;
    auto fidl_options = fuchsia_hardware_ramdisk::wire::Options::Builder(arena);
    if (block_size > 0)
      fidl_options.block_size(block_size);
    if (block_count > 0)
      fidl_options.block_count(block_count);
    if (type_guid) {
      memcpy(&guid.value.data_, type_guid, 16);
      fidl_options.type_guid(guid);
      ZX_ASSERT(fidl_options.has_type_guid());
    }
    if (vmo.is_valid())
      fidl_options.vmo(std::move(vmo));
    fidl_options.publish(true);
    const fidl::WireResult result = fidl::WireCall(*controller)->Create(fidl_options.Build());
    if (!result.ok())
      return zx::error(result.status());
    fit::result response = result.value();
    if (response.is_error())
      return zx::error(response.error_value());
    std::unique_ptr<Client> client;
    if (zx_status_t status =
            Client::Create(std::move(response->outgoing), std::move(response->lifeline), &client);
        status != ZX_OK) {
      return zx::error(status);
    }
    return zx::ok(std::move(client));
  }

 private:
  Client(std::string bind_path, fbl::String path,
         fidl::ClientEnd<fuchsia_hardware_ramdisk::Ramdisk> ramdisk_interface,
         zx::eventpair lifeline)
      : path_(std::move(path)),
        ramdisk_interface_(std::move(ramdisk_interface)),
        lifeline_(std::move(lifeline)),
        bind_path_(std::move(bind_path)) {}

  // The fully qualified path.
  const fbl::String path_;
  fidl::ClientEnd<fuchsia_hardware_ramdisk::Ramdisk> ramdisk_interface_;
  zx::eventpair lifeline_;
  std::string bind_path_;
};

zx::result<Ramdisk> Ramdisk::Create(int block_size, uint64_t block_count,
                                    std::optional<int> svc_root_fd,
                                    const Ramdisk::Options& options) {
  zx::result client =
      Client::CreateWithOptions(static_cast<uint32_t>(block_size), block_count,
                                options.type_guid ? options.type_guid->data() : nullptr, zx::vmo(),
                                svc_root_fd ? *svc_root_fd : -1);
  if (client.is_error()) {
    FX_LOGS(ERROR) << "Could not create ramdisk for test: " << client.status_string();
    return client.take_error();
  }
  return zx::ok(Ramdisk(*std::move(client)));
}

zx::result<Ramdisk> Ramdisk::CreateWithVmo(zx::vmo vmo, uint64_t block_size,
                                           std::optional<int> svc_root_fd,
                                           const Ramdisk::Options& options) {
  zx::result client = Client::CreateWithOptions(
      static_cast<uint32_t>(block_size), 0, options.type_guid ? options.type_guid->data() : nullptr,
      std::move(vmo), svc_root_fd ? *svc_root_fd : -1);
  if (client.is_error()) {
    FX_LOGS(ERROR) << "Could not create ramdisk for test: " << client.status_string();
    return client.take_error();
  }
  return zx::ok(Ramdisk(*std::move(client)));
}

void Ramdisk::Reset() { client_.reset(); }

Ramdisk::Ramdisk() = default;
Ramdisk::Ramdisk(Ramdisk&& other) noexcept = default;
Ramdisk& Ramdisk::operator=(Ramdisk&& other) noexcept = default;
Ramdisk::~Ramdisk() = default;
Ramdisk::Ramdisk(std::unique_ptr<Client> client) : client_(std::move(client)) {}

std::string Ramdisk::path() const { return client_->path().c_str(); }

zx::result<fidl::ClientEnd<fuchsia_storage_block::Block>> Ramdisk::ConnectBlock() const {
  return client_->Connect();
}

zx::result<> Ramdisk::SleepAfter(uint64_t block_count) const {
  const fidl::WireResult result =
      fidl::WireCall(client_->ramdisk_interface())->SleepAfter(block_count);
  return zx::make_result(result.status());
}

zx::result<> Ramdisk::Wake() const {
  const fidl::WireResult result = fidl::WireCall(client_->ramdisk_interface())->Wake();
  return zx::make_result(result.status());
}

zx::result<> Ramdisk::SetFlags(uint32_t flags) const {
  const fidl::WireResult result =
      fidl::WireCall(client_->ramdisk_interface())
          ->SetFlags(static_cast<fuchsia_hardware_ramdisk::wire::RamdiskFlag>(flags));
  return zx::make_result(result.status());
}

zx::result<ramdisk_block_write_counts_t> Ramdisk::GetBlockCounts() const {
  static_assert(sizeof(ramdisk_block_write_counts_t) ==
                    sizeof(fuchsia_hardware_ramdisk::wire::BlockWriteCounts),
                "Cannot convert between C library / FIDL block counts");

  const fidl::WireResult result = fidl::WireCall(client_->ramdisk_interface())->GetBlockCounts();
  if (!result.ok()) {
    return zx::error(result.status());
  }
  ramdisk_block_write_counts_t counts;
  memcpy(&counts, &result.value().counts, sizeof(ramdisk_block_write_counts_t));
  return zx::ok(counts);
}

}  // namespace ramdevice_client
