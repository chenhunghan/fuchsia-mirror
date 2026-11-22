// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.component.decl/cpp/fidl.h>
#include <fidl/fuchsia.component.decl/cpp/hlcpp_conversion.h>
#include <fidl/fuchsia.component.runner/cpp/fidl.h>
#include <fidl/fuchsia.component.test/cpp/fidl.h>
#include <fidl/fuchsia.component/cpp/fidl.h>
#include <fidl/fuchsia.io/cpp/fidl.h>
#include <fuchsia/component/decl/cpp/fidl.h>
#include <lib/async/default.h>
#include <lib/async/dispatcher.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/io.h>
#include <lib/fidl/cpp/client.h>
#include <lib/sys/component/cpp/testing/internal/convert.h>
#include <lib/sys/component/cpp/testing/internal/errors.h>
#include <lib/sys/component/cpp/testing/internal/local_component_runner.h>
#include <lib/sys/component/cpp/testing/internal/realm.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <lib/sys/component/cpp/testing/scoped_child.h>
#include <lib/sys/cpp/service_directory.h>
#include <zircon/assert.h>
#include <zircon/availability.h>
#include <zircon/errors.h>

#include <cstddef>
#include <memory>
#include <optional>
#include <sstream>
#include <string>
#include <utility>
#include <vector>

namespace component_testing {
namespace {
constexpr char kFrameworkIntermediaryChildName[] = "realm_builder_server";
constexpr char kChildPathSeparator[] = "/";

fidl::ClientEnd<fuchsia_io::Directory> CreatePkgDirHandle() {
  int fd;
  ZX_COMPONENT_ASSERT_STATUS_OK(
      "fdio_open3_fd",
      // It's okay to cast Rights to Flags as there is a direct mapping.
      fdio_open3_fd("/pkg", static_cast<uint64_t>(fuchsia_io::kRxStarDir), &fd));
  zx_handle_t handle;
  ZX_COMPONENT_ASSERT_STATUS_OK("fdio_fd_transfer", fdio_fd_transfer(fd, &handle));
  return fidl::ClientEnd<fuchsia_io::Directory>(zx::channel(handle));
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

// Implementation methods for Realm.

Realm& Realm::AddChild(const std::string& child_name, const std::string& url,
                       const ChildOptions& options) {
  ZX_COMPONENT_ASSERT_RESULT_OK(
      "Realm/AddChild",
      realm_proxy_->AddChild(
          {{.name = child_name, .url = url, .options = internal::ConvertToFidl(options)}}));
  return *this;
}

Realm& Realm::AddLocalChild(const std::string& child_name, LocalComponentFactory local_impl,
                            const ChildOptions& options) {
  return AddLocalChildImpl(child_name, LocalComponentKind(std::move(local_impl)), options);
}

Realm& Realm::AddLocalChildImpl(const std::string& child_name, LocalComponentKind local_impl,
                                const ChildOptions& options) {
  runner_builder_->Register(GetResolvedName(child_name), std::move(local_impl));
  ZX_COMPONENT_ASSERT_RESULT_OK(
      "Realm/AddLocalChild",
      realm_proxy_->AddLocalChild(
          {{.name = child_name, .options = internal::ConvertToFidl(options)}}));
  return *this;
}

Realm Realm::AddChildRealm(const std::string& child_name, const ChildOptions& options) {
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_component_test::Realm>::Create();
  ZX_COMPONENT_ASSERT_RESULT_OK("Realm/AddChildRealm",
                                realm_proxy_->AddChildRealm({{
                                    .name = child_name,
                                    .options = internal::ConvertToFidl(options),
                                    .child_realm = std::move(server_end),
                                }}));
  std::vector<std::string> sub_realm_scope = scope_;
  sub_realm_scope.push_back(child_name);
  return Realm(fidl::SyncClient<fuchsia_component_test::Realm>(std::move(client_end)),
               runner_builder_, std::move(sub_realm_scope));
}

Realm Realm::AddChildRealmFromDecl(const std::string& child_name,
                                   fuchsia::component::decl::Component& decl,
                                   const ChildOptions& options) {
  return AddChildRealmFromDeclImpl(child_name, fidl::HLCPPToNatural(std::move(decl)), options);
}

Realm Realm::AddChildRealmFromDecl(const std::string& child_name,
                                   fuchsia_component_decl::Component decl,
                                   const ChildOptions& options) {
  return AddChildRealmFromDeclImpl(child_name, std::move(decl), options);
}

Realm Realm::AddChildRealmFromDeclImpl(const std::string& child_name,
                                       fuchsia_component_decl::Component decl,
                                       const ChildOptions& options) {
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_component_test::Realm>::Create();
  ZX_COMPONENT_ASSERT_RESULT_OK("Realm/AddChildRealmFromDecl",
                                realm_proxy_->AddChildRealmFromDecl({{
                                    .name = child_name,
                                    .decl = std::move(decl),
                                    .options = internal::ConvertToFidl(options),
                                    .child_realm = std::move(server_end),
                                }}));
  std::vector<std::string> sub_realm_scope = scope_;
  sub_realm_scope.push_back(child_name);
  return Realm(fidl::SyncClient<fuchsia_component_test::Realm>(std::move(client_end)),
               runner_builder_, std::move(sub_realm_scope));
}

Realm& Realm::AddRoute(Route route) {
  auto capabilities = internal::ConvertToFidlVec<Capability, fuchsia_component_test::Capability>(
      route.capabilities);
  auto [source, from_dictionary] =
      internal::ConvertRefToFidl(route.source, internal::RefContext::SOURCE);
  std::vector<fuchsia_component_decl::Ref> target;
  for (auto& t : internal::ConvertRefToFidlVec(route.targets, internal::RefContext::TARGET)) {
    target.push_back(std::move(t.first));
  }

  ZX_COMPONENT_ASSERT_RESULT_OK("Realm/AddRouteFromDictionary",
                                realm_proxy_->AddRouteFromDictionary({{
                                    .capabilities = std::move(capabilities),
                                    .from = std::move(source),
                                    .from_dictionary = std::move(from_dictionary),
                                    .to = std::move(target),
                                }}));
  return *this;
}

Realm& Realm::RouteReadOnlyDirectory(const std::string& name, std::vector<Ref> to,
                                     DirectoryContents directory) {
  std::vector<fuchsia_component_decl::Ref> to_fidl;
  to_fidl.reserve(to.size());
  for (auto& ref : to) {
    to_fidl.push_back(internal::ConvertRefToFidl(ref, internal::RefContext::SOURCE).first);
  }
  auto directory_fidl = directory.TakeAsFidl();

  ZX_COMPONENT_ASSERT_RESULT_OK("Realm/ReadOnlyDirectory",
                                realm_proxy_->ReadOnlyDirectory({{
                                    .name = name,
                                    .to = std::move(to_fidl),
                                    .directory_contents = std::move(directory_fidl),
                                }}));

  return *this;
}

Realm& Realm::InitMutableConfigFromPackage(const std::string& name) {
  ZX_COMPONENT_ASSERT_RESULT_OK("Realm/InitMutableConfigFromPackage",
                                realm_proxy_->InitMutableConfigFromPackage({{.name = name}}));
  return *this;
}

Realm& Realm::InitMutableConfigToEmpty(const std::string& name) {
  ZX_COMPONENT_ASSERT_RESULT_OK("Realm/InitMutableConfigToEmpty",
                                realm_proxy_->InitMutableConfigToEmpty({{.name = name}}));
  return *this;
}

Realm& Realm::SetConfigValue(const std::string& name, const std::string& key, ConfigValue value) {
  ZX_COMPONENT_ASSERT_RESULT_OK(
      "Realm/SetConfigValue",
      realm_proxy_->SetConfigValue({{.name = name, .key = key, .value = value.TakeAsFidl()}}));
  return *this;
}

Realm& Realm::AddConfiguration(std::vector<ConfigCapability> configurations) {
  for (ConfigCapability& c : configurations) {
    fuchsia_component_decl::Configuration config{{
        .name = c.name,
        .value = std::move(*c.value.TakeAsFidl().value()),
    }};
    ZX_COMPONENT_ASSERT_RESULT_OK(
        "Realm/AddCapability",
        realm_proxy_->AddCapability({{
            .capability = fuchsia_component_decl::Capability::WithConfig(std::move(config)),
        }}));
  }

  return *this;
}

Realm& Realm::AddCapability(fuchsia::component::decl::Capability capability) {
  return AddCapabilityImpl(fidl::HLCPPToNatural(std::move(capability)));
}

Realm& Realm::AddCapability(fuchsia_component_decl::Capability capability) {
  return AddCapabilityImpl(std::move(capability));
}

Realm& Realm::AddCapabilityImpl(fuchsia_component_decl::Capability capability) {
  ZX_COMPONENT_ASSERT_RESULT_OK(
      "Realm/AddCapability", realm_proxy_->AddCapability({{.capability = std::move(capability)}}));
  return *this;
}

Realm& Realm::AddDictionary(std::string_view name) { return AddDictionaryImpl(name); }

Realm& Realm::AddDictionaryImpl(std::string_view name) {
  fuchsia_component_decl::Dictionary dictionary{{.name = std::string(name)}};
  return AddCapabilityImpl(
      fuchsia_component_decl::Capability::WithDictionary(std::move(dictionary)));
}

Realm& Realm::AddCollection(fuchsia::component::decl::Collection collection) {
  return AddCollectionImpl(fidl::HLCPPToNatural(std::move(collection)));
}

Realm& Realm::AddCollection(fuchsia_component_decl::Collection collection) {
  return AddCollectionImpl(std::move(collection));
}

Realm& Realm::AddCollectionImpl(fuchsia_component_decl::Collection collection) {
  ZX_COMPONENT_ASSERT_RESULT_OK(
      "Realm/AddCollection", realm_proxy_->AddCollection({{.collection = std::move(collection)}}));
  return *this;
}

Realm& Realm::AddEnvironment(fuchsia::component::decl::Environment environment) {
  return AddEnvironmentImpl(fidl::HLCPPToNatural(std::move(environment)));
}

Realm& Realm::AddEnvironment(fuchsia_component_decl::Environment environment) {
  return AddEnvironmentImpl(std::move(environment));
}

Realm& Realm::AddEnvironmentImpl(fuchsia_component_decl::Environment environment) {
  ZX_COMPONENT_ASSERT_RESULT_OK(
      "Realm/AddEnvironment",
      realm_proxy_->AddEnvironment({{.environment = std::move(environment)}}));
  return *this;
}

void Realm::ReplaceComponentDecl(const std::string& child_name,
                                 fuchsia::component::decl::Component decl) {
  ReplaceComponentDeclImpl(child_name, fidl::HLCPPToNatural(std::move(decl)));
}

void Realm::ReplaceComponentDecl(const std::string& child_name,
                                 fuchsia_component_decl::Component decl) {
  ReplaceComponentDeclImpl(child_name, std::move(decl));
}

void Realm::ReplaceComponentDeclImpl(const std::string& child_name,
                                     fuchsia_component_decl::Component decl) {
  ZX_COMPONENT_ASSERT_RESULT_OK("Realm/ReplaceComponentDecl", realm_proxy_->ReplaceComponentDecl({{
                                                                  .name = child_name,
                                                                  .component_decl = std::move(decl),
                                                              }}));
}

void Realm::ReplaceRealmDecl(fuchsia::component::decl::Component decl) {
  ReplaceRealmDeclImpl(fidl::HLCPPToNatural(std::move(decl)));
}

void Realm::ReplaceRealmDecl(fuchsia_component_decl::Component decl) {
  ReplaceRealmDeclImpl(std::move(decl));
}

void Realm::ReplaceRealmDeclImpl(fuchsia_component_decl::Component decl) {
  ZX_COMPONENT_ASSERT_RESULT_OK(
      "Realm/ReplaceRealmDecl",
      realm_proxy_->ReplaceRealmDecl({{.component_decl = std::move(decl)}}));
}

fuchsia::component::decl::Component Realm::GetComponentDecl(const std::string& child_name) {
  auto result = realm_proxy_->GetComponentDecl({{.name = child_name}});
  ZX_COMPONENT_ASSERT_RESULT_OK("Realm/GetComponentDecl", result);

  return fidl::NaturalToHLCPP(std::move(result->component_decl()));
}

fuchsia::component::decl::Component Realm::GetRealmDecl() {
  auto result = realm_proxy_->GetRealmDecl();
  ZX_COMPONENT_ASSERT_RESULT_OK("Realm/GetRealmDecl", result);

  return fidl::NaturalToHLCPP(std::move(result->component_decl()));
}

Realm::Realm(fidl::SyncClient<fuchsia_component_test::Realm> realm_proxy,
             std::shared_ptr<internal::LocalComponentRunner::Builder> runner_builder,
             std::vector<std::string> scope)
    : realm_proxy_(std::move(realm_proxy)),
      runner_builder_(std::move(runner_builder)),
      scope_(std::move(scope)) {}

std::string Realm::GetResolvedName(const std::string& child_name) {
  if (scope_.empty()) {
    return child_name;
  }

  std::stringstream path;
  for (const auto& s : scope_) {
    path << s << kChildPathSeparator;
  }
  return path.str() + child_name;
}

// Implementation methods for RealmBuilder.

RealmBuilder RealmBuilder::Create(std::shared_ptr<sys::ServiceDirectory> svc) {
  return CreateImpl(std::nullopt, ToNaturalSvc(std::move(svc)));
}

RealmBuilder RealmBuilder::Create(fidl::ClientEnd<fuchsia_io::Directory> svc) {
  return CreateImpl(std::nullopt, std::move(svc));
}

RealmBuilder RealmBuilder::CreateFromRelativeUrl(std::string_view fragment_only_url,
                                                 std::shared_ptr<sys::ServiceDirectory> svc) {
  return CreateImpl(fragment_only_url, ToNaturalSvc(std::move(svc)));
}

RealmBuilder RealmBuilder::CreateFromRelativeUrl(std::string_view fragment_only_url,
                                                 fidl::ClientEnd<fuchsia_io::Directory> svc) {
  return CreateImpl(fragment_only_url, std::move(svc));
}

RealmBuilder RealmBuilder::CreateImpl(std::optional<std::string_view> fragment_only_url,
                                      fidl::ClientEnd<fuchsia_io::Directory> svc) {
  fidl::SyncClient<fuchsia_component::Realm> realm_proxy =
      internal::CreateRealmClient(svc.borrow());
  auto child_ref = fuchsia_component_decl::ChildRef{{.name = kFrameworkIntermediaryChildName}};
  fidl::ClientEnd<fuchsia_io::Directory> exposed_dir =
      internal::OpenExposedDir(realm_proxy, child_ref);
  zx::result factory_end =
      component::ConnectAt<fuchsia_component_test::RealmBuilderFactory>(exposed_dir);
  ZX_COMPONENT_ASSERT_STATUS_OK("RealmBuilderFactory/Create", factory_end.status_value());
  fidl::SyncClient<fuchsia_component_test::RealmBuilderFactory> factory_proxy(
      std::move(*factory_end));
  auto [realm_client_end, realm_server_end] =
      fidl::Endpoints<fuchsia_component_test::Realm>::Create();
  auto [builder_client_end, builder_server_end] =
      fidl::Endpoints<fuchsia_component_test::Builder>::Create();
  if (fragment_only_url.has_value()) {
    ZX_ASSERT_MSG(!fragment_only_url.value().empty(), "fragment_only_url can't be empty");

    ZX_COMPONENT_ASSERT_RESULT_OK("RealmBuilderFactory/CreateFromRelativeUrl",
                                  factory_proxy->CreateFromRelativeUrl({{
                                      .pkg_dir_handle = CreatePkgDirHandle(),
                                      .relative_url = std::string(fragment_only_url.value()),
                                      .realm_server_end = std::move(realm_server_end),
                                      .builder_server_end = std::move(builder_server_end),
                                  }}));
  } else {
    ZX_COMPONENT_ASSERT_RESULT_OK("RealmBuilderFactory/Create",
                                  factory_proxy->Create({{
                                      .pkg_dir_handle = CreatePkgDirHandle(),
                                      .realm_server_end = std::move(realm_server_end),
                                      .builder_server_end = std::move(builder_server_end),
                                  }}));
  }
  return RealmBuilder(
      std::move(svc),
      fidl::SyncClient<fuchsia_component_test::Builder>(std::move(builder_client_end)),
      fidl::SyncClient<fuchsia_component_test::Realm>(std::move(realm_client_end)));
}

RealmBuilder& RealmBuilder::AddChild(const std::string& child_name, const std::string& url,
                                     const ChildOptions& options) {
  ZX_ASSERT_MSG(!child_name.empty(), "child_name can't be empty");
  ZX_ASSERT_MSG(!url.empty(), "url can't be empty");

  root_.AddChild(child_name, url, options);
  return *this;
}

RealmBuilder& RealmBuilder::AddLocalChild(const std::string& child_name,
                                          LocalComponentFactory local_impl,
                                          const ChildOptions& options) {
  ZX_ASSERT_MSG(!child_name.empty(), "child_name can't be empty");
  root_.AddLocalChildImpl(child_name, LocalComponentKind(std::move(local_impl)), options);
  return *this;
}

Realm RealmBuilder::AddChildRealm(const std::string& child_name, const ChildOptions& options) {
  ZX_ASSERT_MSG(!child_name.empty(), "child_name can't be empty");
  return root_.AddChildRealm(child_name, options);
}

Realm RealmBuilder::AddChildRealmFromDecl(const std::string& child_name,
                                          fuchsia::component::decl::Component& decl,
                                          const ChildOptions& options) {
  ZX_ASSERT_MSG(!child_name.empty(), "child_name can't be empty");
  return root_.AddChildRealmFromDecl(child_name, decl, options);
}

Realm RealmBuilder::AddChildRealmFromDecl(const std::string& child_name,
                                          fuchsia_component_decl::Component decl,
                                          const ChildOptions& options) {
  ZX_ASSERT_MSG(!child_name.empty(), "child_name can't be empty");
  return root_.AddChildRealmFromDeclImpl(child_name, std::move(decl), options);
}

RealmBuilder& RealmBuilder::AddRoute(Route route) {
  ZX_ASSERT_MSG(!route.capabilities.empty(), "route.capabilities can't be empty");
  ZX_ASSERT_MSG(!route.targets.empty(), "route.targets can't be empty");

  root_.AddRoute(std::move(route));
  return *this;
}

RealmBuilder& RealmBuilder::RouteReadOnlyDirectory(const std::string& name, std::vector<Ref> to,
                                                   DirectoryContents directory) {
  root_.RouteReadOnlyDirectory(name, std::move(to), std::move(directory));
  return *this;
}

RealmBuilder& RealmBuilder::InitMutableConfigFromPackage(const std::string& name) {
  root_.InitMutableConfigFromPackage(name);
  return *this;
}

RealmBuilder& RealmBuilder::InitMutableConfigToEmpty(const std::string& name) {
  root_.InitMutableConfigToEmpty(name);
  return *this;
}

RealmBuilder& RealmBuilder::AddConfiguration(std::vector<ConfigCapability> configurations) {
  root_.AddConfiguration(std::move(configurations));
  return *this;
}

RealmBuilder& RealmBuilder::AddCapability(fuchsia::component::decl::Capability capability) {
  root_.AddCapability(std::move(capability));
  return *this;
}

RealmBuilder& RealmBuilder::AddCapability(fuchsia_component_decl::Capability capability) {
  root_.AddCapabilityImpl(std::move(capability));
  return *this;
}

RealmBuilder& RealmBuilder::AddDictionary(std::string_view name) {
  root_.AddDictionaryImpl(name);
  return *this;
}

RealmBuilder& RealmBuilder::AddCollection(fuchsia_component_decl::Collection collection) {
  root_.AddCollectionImpl(std::move(collection));
  return *this;
}

RealmBuilder& RealmBuilder::AddEnvironment(fuchsia_component_decl::Environment environment) {
  root_.AddEnvironmentImpl(std::move(environment));
  return *this;
}

RealmBuilder& RealmBuilder::SetConfigValue(const std::string& name, const std::string& key,
                                           ConfigValue value) {
  root_.SetConfigValue(name, key, std::move(value));
  return *this;
}

void RealmBuilder::ReplaceComponentDecl(const std::string& child_name,
                                        fuchsia::component::decl::Component decl) {
  root_.ReplaceComponentDecl(child_name, std::move(decl));
}

void RealmBuilder::ReplaceComponentDecl(const std::string& child_name,
                                        fuchsia_component_decl::Component decl) {
  root_.ReplaceComponentDeclImpl(child_name, std::move(decl));
}

void RealmBuilder::ReplaceRealmDecl(fuchsia::component::decl::Component decl) {
  root_.ReplaceRealmDecl(std::move(decl));
}

void RealmBuilder::ReplaceRealmDecl(fuchsia_component_decl::Component decl) {
  root_.ReplaceRealmDeclImpl(std::move(decl));
}

fuchsia::component::decl::Component RealmBuilder::GetComponentDecl(const std::string& child_name) {
  return root_.GetComponentDecl(child_name);
}

fuchsia::component::decl::Component RealmBuilder::GetRealmDecl() { return root_.GetRealmDecl(); }

RealmBuilder& RealmBuilder::SetRealmCollection(const std::string& collection) {
  realm_collection_ = collection;
  return *this;
}

RealmBuilder& RealmBuilder::SetRealmName(const std::string& name) {
  realm_name_ = name;
  return *this;
}

RealmRoot RealmBuilder::Build(async_dispatcher_t* dispatcher) {
  ZX_ASSERT_MSG(!realm_commited_, "Builder::Build() called after Realm already created");
  if (dispatcher == nullptr) {
    dispatcher = async_get_default_dispatcher();
  }
  ZX_ASSERT_MSG(dispatcher != nullptr, "Builder::Build() called without configured dispatcher");
  auto local_component_runner = runner_builder_->Build(dispatcher);
  auto result = builder_proxy_->Build({{.runner = local_component_runner->NewBinding()}});
  ZX_COMPONENT_ASSERT_RESULT_OK("Builder/Build", result);
  realm_commited_ = true;

  auto scoped_child = ScopedChild::NewFromClientEnd(realm_collection_, realm_name_,
                                                    result->root_component_url(), std::move(svc_));

  // Connect to fuchsia.component.Binder to automatically start Realm.
  if (start_on_build_) {
    zx::result binder = scoped_child.Connect<fuchsia_component::Binder>();
    ZX_COMPONENT_ASSERT_STATUS_OK("ScopedChild/Connect<Binder>", binder.status_value());
  }

  return RealmRoot(std::move(local_component_runner), std::move(scoped_child), dispatcher);
}

Realm& RealmBuilder::root() { return root_; }

RealmBuilder::RealmBuilder(fidl::ClientEnd<fuchsia_io::Directory> svc,
                           fidl::SyncClient<fuchsia_component_test::Builder> builder_proxy,
                           fidl::SyncClient<fuchsia_component_test::Realm> test_realm_proxy)
    : svc_(std::move(svc)),
      builder_proxy_(std::move(builder_proxy)),
      runner_builder_(std::make_shared<internal::LocalComponentRunner::Builder>()),
      root_(Realm(std::move(test_realm_proxy), runner_builder_)) {}

// Implementation methods for RealmRoot.

RealmRoot::RealmRoot(std::unique_ptr<internal::LocalComponentRunner> local_component_runner,
                     ScopedChild root, async_dispatcher_t* dispatcher)
    : local_component_runner_(std::move(local_component_runner)),
      root_(std::move(root)),
      dispatcher_(dispatcher) {}

RealmRoot::~RealmRoot() = default;

void RealmRoot::Teardown(ScopedChild::TeardownCallback on_teardown_complete) {
  root_.Teardown(dispatcher_, std::move(on_teardown_complete));
}

ScopedChild& RealmRoot::component() { return root_; }

const ScopedChild& RealmRoot::component() const { return root_; }

}  // namespace component_testing
