// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_SYS_COMPONENT_CPP_TESTING_REALM_BUILDER_H_
#define LIB_SYS_COMPONENT_CPP_TESTING_REALM_BUILDER_H_

#include <fidl/fuchsia.component.decl/cpp/fidl.h>
#include <fidl/fuchsia.component.runner/cpp/fidl.h>
#include <fidl/fuchsia.component.test/cpp/fidl.h>
#include <fidl/fuchsia.component/cpp/fidl.h>
#include <fidl/fuchsia.io/cpp/fidl.h>
#include <fuchsia/component/cpp/fidl.h>
#include <fuchsia/component/decl/cpp/fidl.h>
#include <fuchsia/component/runner/cpp/fidl.h>
#include <fuchsia/component/test/cpp/fidl.h>
#include <fuchsia/io/cpp/fidl.h>
#include <lib/async/dispatcher.h>
#include <lib/fidl/cpp/client.h>
#include <lib/fidl/cpp/interface_handle.h>
#include <lib/sys/component/cpp/testing/internal/local_component_runner.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <lib/sys/component/cpp/testing/scoped_child.h>
#include <lib/sys/cpp/service_directory.h>
#include <zircon/availability.h>
#include <zircon/errors.h>

#include <cstddef>
#include <functional>
#include <memory>
#include <optional>
#include <string>
#include <string_view>
#include <type_traits>
#include <utility>
#include <variant>
#include <vector>

namespace component_testing {

// Default child options provided to all components.
const ChildOptions kDefaultChildOptions{
    .startup_mode = StartupMode::LAZY, .environment = "", .config_overrides = {}};

// Default child collection name for constructed root.
constexpr char kDefaultCollection[] = "realm_builder";

// Root of a constructed Realm. This object can not be instantiated directly.
// Instead, it can only be constructed with the Realm::Builder/Build().
class RealmRoot final {
 public:
  RealmRoot(RealmRoot&& other) = default;
  RealmRoot& operator=(RealmRoot&& other) = default;

  RealmRoot(const RealmRoot& other) = delete;
  RealmRoot& operator=(const RealmRoot& other) = delete;

  ~RealmRoot();

  // Destructs the root component and sends Component Manager a request to
  // destroy its realm, which will stop all child components. Each
  // |LocalComponentImpl| should receive an |OnStop()| callback, and after
  // returning, the |LocalComponentImpl| will be destructed.
  // |on_teardown_complete| will be invoked when Component Manager has completed
  // the realm teardown.
  void Teardown(ScopedChild::TeardownCallback on_teardown_complete) ZX_AVAILABLE_SINCE(10);

  // Natural bindings overload of |Teardown|.
  template <typename Callback,
            typename = std::enable_if_t<std::conjunction_v<
                std::negation<
                    std::is_invocable_r<void, Callback, fit::result<fuchsia::component::Error>>>,
                std::is_invocable_r<void, Callback, fit::result<fuchsia_component::Error>>>>>
  void Teardown(Callback&& on_teardown_complete) ZX_AVAILABLE_SINCE(NEXT) {
    root_.Teardown(dispatcher_, std::forward<Callback>(on_teardown_complete));
  }

  // Returns reference to underlying |ScopedChild| object. Note that this object
  // will be destroyed if |Teardown| is invoked. In that scenario, using this
  // value will yield undefined behavior. Invoking this method after |Teardown| is
  // invoked will cause this process to panic.
  ScopedChild& component() ZX_AVAILABLE_SINCE(11);
  const ScopedChild& component() const ZX_AVAILABLE_SINCE(11);

 private:
  // Friend classes are needed because the constructor is private.
  friend class Realm;
  friend class RealmBuilder;

  RealmRoot(std::unique_ptr<internal::LocalComponentRunner> local_component_runner,
            ScopedChild root, async_dispatcher_t* dispatcher);

  std::unique_ptr<internal::LocalComponentRunner> local_component_runner_;

  ScopedChild root_;
  async_dispatcher_t* dispatcher_;
};

// A `Realm` describes a component instance together with its children.
// Clients can use this class to build a realm from scratch,
// programmatically adding children and routes.
//
// Clients may also use this class to recursively build sub-realms by calling
// `AddChildRealm`.
// For more information about RealmBuilder, see the following link.
// https://fuchsia.dev/fuchsia-src/development/testing/components/realm_builder
// For examples on how to use this library, see the integration tests
// found at //sdk/cpp/tests/realm_builder_test.cc
class Realm final {
 public:
  Realm(Realm&&) = default;
  Realm& operator=(Realm&&) = default;

  Realm(const Realm&) = delete;
  Realm operator=(const Realm&) = delete;

  // Add a v2 component (.cm) to this Realm.
  // Names must be unique. Duplicate names will result in a panic.
  Realm& AddChild(const std::string& child_name, const std::string& url,
                  const ChildOptions& options = kDefaultChildOptions);

  // Add a component by implementing a factory function that creates and returns
  // a new instance of a |LocalComponentImpl|-derived class. The factory
  // function will be called whenever the local child is started.
  //
  // After returning the |LocalComponentImpl|, the RealmBuilder framework will
  // call |LocalComponentImpl::OnStart()|. Component handles (|ns()|, |svc()|,
  // and |outgoing()|) are not available during the |LocalComponentImpl|
  // construction, but are available when |OnStart()| is invoked.
  //
  // If the component's associated |ComponentController| receives a |Stop()|
  // request, the |LocalComponentImpl::OnStop()| method will be called. A
  // derived |LocalComponentImpl| class can override the |OnStop()| method if
  // the component wishes to take some action during component stop.
  //
  // A |LocalComponentImpl| can also self-terminate, by calling `Exit()`.
  //
  // Names must be unique. Duplicate names will result in a panic.
  Realm& AddLocalChild(const std::string& child_name, LocalComponentFactory local_impl,
                       const ChildOptions& options = kDefaultChildOptions);

  // Create a sub realm as child of this Realm instance. The constructed
  // Realm is returned.
  Realm AddChildRealm(const std::string& child_name,
                      const ChildOptions& options = kDefaultChildOptions);

  // Create a sub realm as child of this Realm instance initialized with |decl|. The constructed
  // Realm is returned.
  Realm AddChildRealmFromDecl(const std::string& child_name,
                              fuchsia::component::decl::Component& decl,
                              const ChildOptions& options = kDefaultChildOptions);

  // Natural bindings overload of |AddChildRealmFromDecl|.
  Realm AddChildRealmFromDecl(const std::string& child_name, fuchsia_component_decl::Component decl,
                              const ChildOptions& options = kDefaultChildOptions)
      ZX_AVAILABLE_SINCE(NEXT);

  // Route a capability from one child to another.
  Realm& AddRoute(Route route);

  // Offers a directory capability to a component in this realm. The
  // directory will be read-only (i.e. have `r*` rights), and will have the
  // contents described in `directory`.
  Realm& RouteReadOnlyDirectory(const std::string& name, std::vector<Ref> to,
                                DirectoryContents directory);

  // Load the packaged configuration of the component if available.
  Realm& InitMutableConfigFromPackage(const std::string& name);

  // Allow setting configuration values without loading packaged configuration.
  Realm& InitMutableConfigToEmpty(const std::string& name);

  // Replaces the value of a given configuration field
  Realm& SetConfigValue(const std::string& name, const std::string& key, ConfigValue value);

  // Adds Configuration Capabilities to the root realm.
  Realm& AddConfiguration(std::vector<ConfigCapability> configurations);

  // Adds a capability to the realm.
  Realm& AddCapability(fuchsia::component::decl::Capability capability);
  Realm& AddCapability(fuchsia_component_decl::Capability capability) ZX_AVAILABLE_SINCE(NEXT);

  // Adds a dictionary capability with |name| to the realm.
  Realm& AddDictionary(std::string_view name) ZX_AVAILABLE_SINCE(NEXT);

  Realm& AddCollection(fuchsia::component::decl::Collection collection);
  Realm& AddCollection(fuchsia_component_decl::Collection collection) ZX_AVAILABLE_SINCE(NEXT);

  Realm& AddEnvironment(fuchsia::component::decl::Environment environment);
  Realm& AddEnvironment(fuchsia_component_decl::Environment environment) ZX_AVAILABLE_SINCE(NEXT);

  // Fetches the Component decl of the given child. This operation is only
  // supported for:
  //
  // * A component with a local implementation
  // * A legacy component
  // * A component added with a fragment-only component URL (typically,
  //   components bundled in the same package as the realm builder client,
  //   sharing the same `/pkg` directory, for example,
  //   `#meta/other-component.cm`; see
  //   https://fuchsia.dev/fuchsia-src/reference/components/url#relative-fragment-only)
  // * An automatically generated realm (such as the root)
  fuchsia::component::decl::Component GetComponentDecl(const std::string& child_name);

  // Fetches the Component decl of this Realm.
  fuchsia::component::decl::Component GetRealmDecl();

  // Updates the Component decl of the given child. This operation is only
  // supported for:
  //
  // * A component with a local implementation
  // * A legacy component
  // * A component added with a fragment-only component URL (typically,
  //   components bundled in the same package as the realm builder client,
  //   sharing the same `/pkg` directory, for example,
  //   `#meta/other-component.cm`; see
  //   https://fuchsia.dev/fuchsia-src/reference/components/url#relative-fragment-only)
  // * An automatically generated realm (such as the root)
  void ReplaceComponentDecl(const std::string& child_name,
                            fuchsia::component::decl::Component decl);
  void ReplaceComponentDecl(const std::string& child_name, fuchsia_component_decl::Component decl)
      ZX_AVAILABLE_SINCE(NEXT);

  // Updates the Component decl of this Realm.
  void ReplaceRealmDecl(fuchsia::component::decl::Component decl);
  void ReplaceRealmDecl(fuchsia_component_decl::Component decl) ZX_AVAILABLE_SINCE(NEXT);

  friend class RealmBuilder;

 private:
  explicit Realm(fidl::SyncClient<fuchsia_component_test::Realm> realm_proxy,
                 std::shared_ptr<internal::LocalComponentRunner::Builder> runner_builder,
                 std::vector<std::string> scope = {});

  std::string GetResolvedName(const std::string& child_name);

  Realm& AddLocalChildImpl(const std::string& child_name, LocalComponentKind local_impl,
                           const ChildOptions& options = kDefaultChildOptions);
  Realm AddChildRealmFromDeclImpl(const std::string& child_name,
                                  fuchsia_component_decl::Component decl,
                                  const ChildOptions& options);
  Realm& AddCapabilityImpl(fuchsia_component_decl::Capability capability);
  Realm& AddDictionaryImpl(std::string_view name);
  Realm& AddCollectionImpl(fuchsia_component_decl::Collection collection);
  Realm& AddEnvironmentImpl(fuchsia_component_decl::Environment environment);
  void ReplaceComponentDeclImpl(const std::string& child_name,
                                fuchsia_component_decl::Component decl);
  void ReplaceRealmDeclImpl(fuchsia_component_decl::Component decl);

  fidl::SyncClient<fuchsia_component_test::Realm> realm_proxy_;
  std::shared_ptr<internal::LocalComponentRunner::Builder> runner_builder_;
  std::vector<std::string> scope_;
};

// Use this Builder class to construct a Realm object.
class RealmBuilder final {
 public:
  // Factory method to create a new Realm::Builder object.
  // |svc| must outlive the RealmBuilder object and created Realm object.
  // If it's nullptr, then the current process' "/svc" namespace entry is used.
  static RealmBuilder Create(std::shared_ptr<sys::ServiceDirectory> svc = nullptr);
  static RealmBuilder Create(fidl::ClientEnd<fuchsia_io::Directory> svc) ZX_AVAILABLE_SINCE(NEXT);

  // Same as above but the Realm will contain the contents of the manifest
  // located in the test package at the path indicated by the fragment-only URL
  // (for example, `#meta/other-component.cm`; see
  // https://fuchsia.dev/fuchsia-src/reference/components/url#relative-fragment-only).
  static RealmBuilder CreateFromRelativeUrl(std::string_view fragment_only_url,
                                            std::shared_ptr<sys::ServiceDirectory> svc = nullptr);
  static RealmBuilder CreateFromRelativeUrl(std::string_view fragment_only_url,
                                            fidl::ClientEnd<fuchsia_io::Directory> svc)
      ZX_AVAILABLE_SINCE(NEXT);

  RealmBuilder(RealmBuilder&&) = default;
  RealmBuilder& operator=(RealmBuilder&&) = default;

  RealmBuilder(const RealmBuilder&) = delete;
  RealmBuilder& operator=(const RealmBuilder&) = delete;

  // Add a v2 component (.cm) to the root realm being constructed.
  // See |Realm.AddChild| for more details.
  RealmBuilder& AddChild(const std::string& child_name, const std::string& url,
                         const ChildOptions& options = kDefaultChildOptions);

  // Add a component by LocalComponentFactory.
  //
  // See |Realm.AddLocalChild| for more details.
  RealmBuilder& AddLocalChild(const std::string& child_name, LocalComponentFactory local_impl,
                              const ChildOptions& options = kDefaultChildOptions);

  // Create a sub realm as child of the root realm. The constructed
  // Realm is returned.
  // See |Realm.AddChildRealm| for more details.
  Realm AddChildRealm(const std::string& child_name,
                      const ChildOptions& options = kDefaultChildOptions);

  // Create a sub realm as child of the root realm initialized with |decl|. The constructed
  // Realm is returned.
  // See |Realm.AddChildRealm| for more details.
  Realm AddChildRealmFromDecl(const std::string& child_name,
                              fuchsia::component::decl::Component& decl,
                              const ChildOptions& options = kDefaultChildOptions);
  Realm AddChildRealmFromDecl(const std::string& child_name, fuchsia_component_decl::Component decl,
                              const ChildOptions& options = kDefaultChildOptions)
      ZX_AVAILABLE_SINCE(NEXT);

  // Route a capability for the root realm being constructed.
  // See |Realm.AddRoute| for more details.
  RealmBuilder& AddRoute(Route route);

  // Offers a directory capability to a component for the root realm.
  // See |Realm.RouteReadOnlyDirectory| for more details.
  RealmBuilder& RouteReadOnlyDirectory(const std::string& name, std::vector<Ref> to,
                                       DirectoryContents directory);

  // Load the packaged configuration of the component if available.
  RealmBuilder& InitMutableConfigFromPackage(const std::string& name);

  // Allow setting configuration values without loading packaged configuration.
  RealmBuilder& InitMutableConfigToEmpty(const std::string& name);

  // Adds Configuration Capabilities to the root realm.
  RealmBuilder& AddConfiguration(std::vector<ConfigCapability> configurations);

  // Adds a capability to the root realm.
  RealmBuilder& AddCapability(fuchsia::component::decl::Capability capability);
  RealmBuilder& AddCapability(fuchsia_component_decl::Capability capability)
      ZX_AVAILABLE_SINCE(NEXT);

  // Adds a dictionary capability with |name| to the root realm.
  RealmBuilder& AddDictionary(std::string_view name) ZX_AVAILABLE_SINCE(NEXT);

  // Adds a collection to the root realm.
  RealmBuilder& AddCollection(fuchsia_component_decl::Collection collection)
      ZX_AVAILABLE_SINCE(NEXT);

  // Adds an environment to the root realm.
  RealmBuilder& AddEnvironment(fuchsia_component_decl::Environment environment)
      ZX_AVAILABLE_SINCE(NEXT);

  // Replaces the value of a given configuration field for the root realm.
  RealmBuilder& SetConfigValue(const std::string& name, const std::string& key, ConfigValue value);

  // Fetches the Component decl of the given child of the root realm.
  // See |Realm.GetComponentDecl| for more details.
  fuchsia::component::decl::Component GetComponentDecl(const std::string& child_name);

  // Fetches the Component decl of this root realm.
  fuchsia::component::decl::Component GetRealmDecl();

  // Updates the Component decl of the given child of the root realm.
  // See |Realm.GetRealmDecl| for more details.
  void ReplaceComponentDecl(const std::string& child_name,
                            fuchsia::component::decl::Component decl);
  void ReplaceComponentDecl(const std::string& child_name, fuchsia_component_decl::Component decl)
      ZX_AVAILABLE_SINCE(NEXT);

  // Updates the Component decl of this root realm.
  void ReplaceRealmDecl(fuchsia::component::decl::Component decl);
  void ReplaceRealmDecl(fuchsia_component_decl::Component decl) ZX_AVAILABLE_SINCE(NEXT);

  // Set the name of the collection that the realm will be added to.
  // By default this is set to |kDefaultCollection|.
  //
  // Note that this collection name is referenced in the Realm Builder
  // shard (//sdk/lib/sys/component/realm_builder_base.shard.cml) under the
  // collection name |kDefaultCollection|. To retain the same routing, component
  // authors that override the collection name should make the appropriate
  // changes in the test component's manifest.
  RealmBuilder& SetRealmCollection(const std::string& collection);

  // Set the name for the constructed realm. By default, a randomly
  // generated string is used.
  RealmBuilder& SetRealmName(const std::string& name);

  // Sets whether or not the realm will be started when `Build` is called.
  RealmBuilder& StartOnBuild(bool start_on_build) {
    start_on_build_ = start_on_build;
    return *this;
  }

  // Build the realm root prepared by the associated builder methods, e.g. |AddComponent|.
  // |dispatcher| must be non-null, or |async_get_default_dispatcher| must be
  // configured to return a non-null value
  // This function can only be called once per Realm::Builder instance.
  // Multiple invocations will result in a panic.
  // |dispatcher| must outlive the lifetime of the constructed |RealmRoot|.
  RealmRoot Build(async_dispatcher_t* dispatcher = nullptr);

  // A reference to the root `Realm` object.
  Realm& root();

 private:
  RealmBuilder(fidl::ClientEnd<fuchsia_io::Directory> svc,
               fidl::SyncClient<fuchsia_component_test::Builder> builder_proxy,
               fidl::SyncClient<fuchsia_component_test::Realm> test_realm_proxy);

  static RealmBuilder CreateImpl(std::optional<std::string_view> fragment_only_url,
                                 fidl::ClientEnd<fuchsia_io::Directory> svc);

  bool realm_commited_ = false;
  bool start_on_build_ = true;
  std::string realm_collection_ = kDefaultCollection;
  std::optional<std::string> realm_name_ = std::nullopt;
  fidl::ClientEnd<fuchsia_io::Directory> svc_;
  fidl::SyncClient<fuchsia_component_test::Builder> builder_proxy_;
  std::shared_ptr<internal::LocalComponentRunner::Builder> runner_builder_;
  Realm root_;
};

}  // namespace component_testing

#endif  // LIB_SYS_COMPONENT_CPP_TESTING_REALM_BUILDER_H_
