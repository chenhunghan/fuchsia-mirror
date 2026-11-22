// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.component.decl/cpp/fidl.h>
#include <fidl/fuchsia.component.test/cpp/fidl.h>
#include <fidl/fuchsia.io/cpp/fidl.h>
#include <fidl/fuchsia.mem/cpp/fidl.h>
#include <lib/fdio/namespace.h>
#include <lib/sys/component/cpp/testing/internal/errors.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <lib/sys/cpp/outgoing_directory.h>
#include <lib/zx/channel.h>
#include <lib/zx/vmo.h>
#include <zircon/assert.h>
#include <zircon/status.h>
#include <zircon/types.h>

#include <memory>

namespace component_testing {

namespace {

constexpr char kSvcDirectoryPath[] = "/svc";

constexpr uint64_t kSvcDirectoryFlags = static_cast<uint64_t>(fuchsia_io::kPermReadable);

#define ZX_SYS_COMPONENT_REPLACE_CONFIG_SINGLE_VALUE_DEF(MethodName, Type, FidlType) \
  ConfigValue ConfigValue::MethodName(Type value) {                                  \
    fuchsia_component_decl::ConfigValueSpec spec{{                                   \
        .value = fuchsia_component_decl::ConfigValue::WithSingle(                    \
            fuchsia_component_decl::ConfigSingleValue::FidlType(std::move(value))),  \
    }};                                                                              \
    return ConfigValue(std::move(spec));                                             \
  }

#define ZX_SYS_COMPONENT_REPLACE_CONFIG_SINGLE_VALUE_CTOR_DEF(Type, FidlType)           \
  ConfigValue::ConfigValue(Type value)                                                  \
      : spec({{                                                                         \
            .value = fuchsia_component_decl::ConfigValue::WithSingle(                   \
                fuchsia_component_decl::ConfigSingleValue::FidlType(std::move(value))), \
        }}) {}

#define ZX_SYS_COMPONENT_REPLACE_CONFIG_VECTOR_VALUE_CTOR_DEF(Type, FidlType)           \
  ConfigValue::ConfigValue(Type value)                                                  \
      : spec({{                                                                         \
            .value = fuchsia_component_decl::ConfigValue::WithVector(                   \
                fuchsia_component_decl::ConfigVectorValue::FidlType(std::move(value))), \
        }}) {}

// Checks that path doesn't contain leading nor trailing slashes.
bool IsValidPath(std::string_view path) {
  return !path.empty() && path.front() != '/' && path.back() != '/';
}
}  // namespace

fdio_ns_t* LocalComponentImplBase::ns() {
  ZX_ASSERT_MSG(
      initialized_,
      "LocalComponentImplBase::ns() cannot be called until RealmBuilder calls Initialize()");
  return namespace_;
}

zx_status_t LocalComponentImplBase::Initialize(fdio_ns_t* ns, zx::channel outgoing_dir,
                                               async_dispatcher_t* dispatcher,
                                               fit::function<void(zx_status_t)> on_exit) {
  namespace_ = ns;
  on_exit_ = std::move(on_exit);
  zx_status_t status = SetOutgoingDirectory(std::move(outgoing_dir), dispatcher);
  if (status == ZX_OK) {
    initialized_ = true;
  }
  return status;
}

sys::OutgoingDirectory* LocalHlcppComponent::outgoing() {
  ZX_ASSERT_MSG(
      initialized_,
      "LocalHlcppComponent::outgoing() cannot be called until RealmBuilder calls Initialize()");
  return &outgoing_dir_;
}

sys::ServiceDirectory LocalHlcppComponent::svc() {
  ZX_ASSERT_MSG(
      initialized_,
      "LocalHlcppComponent::svc() cannot be called until RealmBuilder calls Initialize()");

  zx::channel local;
  zx::channel remote;
  ZX_COMPONENT_ASSERT_STATUS_OK("zx::channel/create", zx::channel::create(0, &local, &remote));

  auto status = fdio_ns_open3(namespace_, kSvcDirectoryPath, kSvcDirectoryFlags, remote.release());
  ZX_ASSERT_MSG(status == ZX_OK,
                "fdio_ns_open3 on LocalComponent's /svc directory failed: %s\nThis most"
                "often occurs when a component has no FIDL protocols routed to it.",
                zx_status_get_string(status));

  return sys::ServiceDirectory(std::move(local));
}

component::OutgoingDirectory* LocalCppComponent::outgoing() {
  ZX_ASSERT_MSG(
      initialized_,
      "LocalCppComponent::outgoing() cannot be called until RealmBuilder calls Initialize()");
  return outgoing_dir_.get();
}

zx::result<fidl::ClientEnd<fuchsia_io::Directory>> LocalCppComponent::svc() {
  ZX_ASSERT_MSG(initialized_,
                "LocalCppComponent::svc() cannot be called until RealmBuilder calls Initialize()");

  auto [client, server] = fidl::Endpoints<fuchsia_io::Directory>::Create();
  zx_status_t status = fdio_ns_open3(namespace_, kSvcDirectoryPath, kSvcDirectoryFlags,
                                     server.TakeChannel().release());
  if (status != ZX_OK) {
    return zx::error(status);
  }

  return zx::ok(std::move(client));
}

void LocalComponentImplBase::Exit(zx_status_t return_code) {
  ZX_ASSERT_MSG(
      initialized_,
      "LocalComponentImplBase::Exit() cannot be called until RealmBuilder calls Initialize()");

  if (on_exit_) {
    on_exit_(return_code);
  }
}

LocalComponentImplBase::~LocalComponentImplBase() {
  if (namespace_) {
    ZX_ASSERT(fdio_ns_destroy(namespace_) == ZX_OK);
  }
}

LocalComponentHandles::LocalComponentHandles(fdio_ns_t* ns, sys::OutgoingDirectory outgoing_dir)
    : namespace_(ns), outgoing_dir_(std::move(outgoing_dir)) {}

LocalComponentHandles::~LocalComponentHandles() { ZX_ASSERT(fdio_ns_destroy(namespace_) == ZX_OK); }

LocalComponentHandles::LocalComponentHandles(LocalComponentHandles&& other) noexcept
    : namespace_(other.namespace_), outgoing_dir_(std::move(other.outgoing_dir_)) {
  other.namespace_ = nullptr;
}

LocalComponentHandles& LocalComponentHandles::operator=(LocalComponentHandles&& other) noexcept {
  namespace_ = other.namespace_;
  outgoing_dir_ = std::move(other.outgoing_dir_);
  other.namespace_ = nullptr;
  return *this;
}

fdio_ns_t* LocalComponentHandles::ns() { return namespace_; }

sys::OutgoingDirectory* LocalComponentHandles::outgoing() { return &outgoing_dir_; }

sys::ServiceDirectory LocalComponentHandles::svc() {
  zx::channel local;
  zx::channel remote;
  ZX_COMPONENT_ASSERT_STATUS_OK("zx::channel/create", zx::channel::create(0, &local, &remote));

  auto status = fdio_ns_open3(namespace_, kSvcDirectoryPath, kSvcDirectoryFlags, remote.release());
  ZX_ASSERT_MSG(status == ZX_OK,
                "fdio_ns_open3 on LocalComponent's /svc directory failed: %s\nThis most"
                "often occurs when a component has no FIDL protocols routed to it.",
                zx_status_get_string(status));

  return sys::ServiceDirectory(std::move(local));
}

void LocalComponentHandles::Exit(zx_status_t return_code) {
  if (on_exit_) {
    on_exit_(return_code);
  }
}

constexpr size_t kDefaultVmoSize = 4096;

DirectoryContents& DirectoryContents::AddFile(std::string_view path, BinaryContents contents) {
  ZX_ASSERT_MSG(IsValidPath(path), "[DirectoryContents/AddFile] Encountered invalid path: %s",
                path.data());

  zx::vmo vmo;
  ZX_COMPONENT_ASSERT_STATUS_OK("AddFile/zx_vmo_create", zx::vmo::create(kDefaultVmoSize, 0, &vmo));
  ZX_COMPONENT_ASSERT_STATUS_OK("AddFile/zx_vmo_write",
                                vmo.write(contents.buffer, contents.offset, contents.size));
  fuchsia_mem::Buffer out_buffer{{.vmo = std::move(vmo), .size = contents.size}};
  contents_.entries().emplace_back(fuchsia_component_test::DirectoryEntry{{
      .file_path = std::string(path),
      .file_contents = std::move(out_buffer),
  }});
  return *this;
}

DirectoryContents& DirectoryContents::AddFile(std::string_view path, std::string_view contents) {
  return AddFile(path,
                 BinaryContents{.buffer = contents.data(), .size = contents.size(), .offset = 0});
}

fuchsia_component_test::DirectoryContents DirectoryContents::TakeAsFidl() {
  return std::exchange(contents_, {});
}

ZX_SYS_COMPONENT_REPLACE_CONFIG_SINGLE_VALUE_CTOR_DEF(std::string, WithString)
ZX_SYS_COMPONENT_REPLACE_CONFIG_SINGLE_VALUE_CTOR_DEF(const char*, WithString)
ZX_SYS_COMPONENT_REPLACE_CONFIG_SINGLE_VALUE_DEF(Bool, bool, WithBool_)
ZX_SYS_COMPONENT_REPLACE_CONFIG_SINGLE_VALUE_DEF(Uint8, uint8_t, WithUint8)
ZX_SYS_COMPONENT_REPLACE_CONFIG_SINGLE_VALUE_DEF(Uint16, uint16_t, WithUint16)
ZX_SYS_COMPONENT_REPLACE_CONFIG_SINGLE_VALUE_DEF(Uint32, uint32_t, WithUint32)
ZX_SYS_COMPONENT_REPLACE_CONFIG_SINGLE_VALUE_DEF(Uint64, uint64_t, WithUint64)
ZX_SYS_COMPONENT_REPLACE_CONFIG_SINGLE_VALUE_DEF(Int8, int8_t, WithInt8)
ZX_SYS_COMPONENT_REPLACE_CONFIG_SINGLE_VALUE_DEF(Int16, int16_t, WithInt16)
ZX_SYS_COMPONENT_REPLACE_CONFIG_SINGLE_VALUE_DEF(Int32, int32_t, WithInt32)
ZX_SYS_COMPONENT_REPLACE_CONFIG_SINGLE_VALUE_DEF(Int64, int64_t, WithInt64)
ZX_SYS_COMPONENT_REPLACE_CONFIG_VECTOR_VALUE_CTOR_DEF(std::vector<bool>, WithBoolVector)
ZX_SYS_COMPONENT_REPLACE_CONFIG_VECTOR_VALUE_CTOR_DEF(std::vector<uint8_t>, WithUint8Vector)
ZX_SYS_COMPONENT_REPLACE_CONFIG_VECTOR_VALUE_CTOR_DEF(std::vector<uint16_t>, WithUint16Vector)
ZX_SYS_COMPONENT_REPLACE_CONFIG_VECTOR_VALUE_CTOR_DEF(std::vector<uint32_t>, WithUint32Vector)
ZX_SYS_COMPONENT_REPLACE_CONFIG_VECTOR_VALUE_CTOR_DEF(std::vector<uint64_t>, WithUint64Vector)
ZX_SYS_COMPONENT_REPLACE_CONFIG_VECTOR_VALUE_CTOR_DEF(std::vector<int8_t>, WithInt8Vector)
ZX_SYS_COMPONENT_REPLACE_CONFIG_VECTOR_VALUE_CTOR_DEF(std::vector<int16_t>, WithInt16Vector)
ZX_SYS_COMPONENT_REPLACE_CONFIG_VECTOR_VALUE_CTOR_DEF(std::vector<int32_t>, WithInt32Vector)
ZX_SYS_COMPONENT_REPLACE_CONFIG_VECTOR_VALUE_CTOR_DEF(std::vector<int64_t>, WithInt64Vector)
ZX_SYS_COMPONENT_REPLACE_CONFIG_VECTOR_VALUE_CTOR_DEF(std::vector<std::string>, WithStringVector)

ConfigValue::ConfigValue(fuchsia_component_decl::ConfigValueSpec spec) : spec(std::move(spec)) {}
fuchsia_component_decl::ConfigValueSpec ConfigValue::TakeAsFidl() {
  return std::exchange(spec, {});
}
ConfigValue& ConfigValue::operator=(ConfigValue&& other) noexcept {
  spec = std::move(other.spec);
  return *this;
}
ConfigValue::ConfigValue(ConfigValue&& other) noexcept : spec(std::move(other.spec)) {}

}  // namespace component_testing
