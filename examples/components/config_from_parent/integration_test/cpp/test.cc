// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.component.decl/cpp/fidl.h>
#include <fidl/fuchsia.component/cpp/fidl.h>
#include <fidl/fuchsia.diagnostics/cpp/fidl.h>
#include <fuchsia/diagnostics/cpp/fidl.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/diagnostics/reader/cpp/archive_reader.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>
#include <lib/syslog/cpp/macros.h>

#include <string>

#include <rapidjson/document.h>
#include <rapidjson/pointer.h>

#include "src/lib/testing/loop_fixture/real_loop_fixture.h"

using diagnostics::reader::InspectData;

constexpr char kChildUrl[] = "#meta/config_example.cm";
constexpr char kCollectionName[] = "realm_api_collection";

class IntegrationTest : public gtest::RealLoopFixture {
 protected:
  InspectData GetInspect(const std::string& child_name) {
    std::string child_moniker = std::string(kCollectionName) + ":" + child_name;
    return GetInspect(child_name, child_moniker);
  }

  InspectData GetInspect(const std::string& child_name, const std::string& child_moniker) {
    std::string selector =
        diagnostics::reader::SanitizeMonikerForSelectors(child_moniker) + ":root";
    diagnostics::reader::ArchiveReader reader(dispatcher(), {selector});
    fpromise::result<std::vector<InspectData>, std::string> result;
    async::Executor executor(dispatcher());
    executor.schedule_task(
        reader.SnapshotInspectUntilPresent({child_moniker})
            .then([&](fpromise::result<std::vector<InspectData>, std::string>& rest) {
              result = std::move(rest);
            }));
    RunLoopUntil([&] { return result.is_ok() || result.is_error(); });

    EXPECT_EQ(result.is_error(), false) << "Error was " << result.error();
    EXPECT_EQ(result.value().size(), 1ul) << "Expected only one component";

    return std::move(result.value()[0]);
  }
};

TEST_F(IntegrationTest, ParentValuesObserved) {
  std::string child_name = "dynamic_child_realm_api_parent_values";
  std::string expected_greeting = "parent component";
  zx::result client_end = component::Connect<fuchsia_component::Realm>();
  ZX_ASSERT(client_end.is_ok());
  fidl::Client realm(std::move(*client_end), dispatcher());
  ZX_ASSERT(realm.is_valid());

  // [START create_child]
  fuchsia_component_decl::Child child_decl;
  child_decl.name(child_name);
  child_decl.url(kChildUrl);
  child_decl.startup(fuchsia_component_decl::StartupMode::kLazy);

  fuchsia_component_decl::CollectionRef collection;
  collection.name(kCollectionName);

  fuchsia_component_decl::OfferConfiguration offer_config;
  offer_config.source(fuchsia_component_decl::Ref::WithSelf(fuchsia_component_decl::SelfRef{}));
  offer_config.source_name("fuchsia.example.config.Greeting");
  offer_config.target_name("fuchsia.example.config.Greeting");
  offer_config.availability(fuchsia_component_decl::Availability::kRequired);

  fuchsia_component::CreateChildArgs child_args;
  child_args.dynamic_offers(
      std::vector{fuchsia_component_decl::Offer::WithConfig(std::move(offer_config))});

  realm->CreateChild({collection, child_decl, std::move(child_args)})
      .ThenExactlyOnce([this](fidl::Result<fuchsia_component::Realm::CreateChild>& result) {
        if (!result.is_ok()) {
          FX_LOGS(ERROR) << "CreateChild failed: " << result.error_value();
          ZX_PANIC("%s", result.error_value().FormatDescription().c_str());
        }
        QuitLoop();
      });
  RunLoop();
  // [END create_child]

  fuchsia_component_decl::ChildRef child_ref;
  child_ref.collection(kCollectionName);
  child_ref.name(child_name);

  auto exposed_endpoints = fidl::Endpoints<fuchsia_io::Directory>::Create();
  auto exposed_client = std::move(exposed_endpoints.client);

  realm->OpenExposedDir({child_ref, std::move(exposed_endpoints.server)})
      .ThenExactlyOnce([this](fidl::Result<fuchsia_component::Realm::OpenExposedDir>& result) {
        if (!result.is_ok()) {
          FX_LOGS(ERROR) << "OpenExposedDir failed: " << result.error_value();
          ZX_PANIC("%s", result.error_value().FormatDescription().c_str());
        }
        QuitLoop();
      });
  RunLoop();

  zx::result bind_result = component::ConnectAt<fuchsia_component::Binder>(exposed_client);
  if (!bind_result.is_ok()) {
    FX_LOGS(ERROR) << "Opening fuchsia.component.Binder failed: " << bind_result.error_value();
  }

  auto data = GetInspect(child_name);

  EXPECT_EQ(expected_greeting, data.GetByPath({"root", "config", "greeting"}).GetString());
}

// This test ensures that the test above is passing because parent overrides work rather than
// lucking into the same value as the packaged config.
TEST_F(IntegrationTest, DefaultValuesObserved) {
  std::string child_name = "dynamic_child_realm_api_default_values";
  std::string expected_greeting = "World!";
  zx::result client_end = component::Connect<fuchsia_component::Realm>();
  ZX_ASSERT(client_end.is_ok());
  fidl::Client realm(std::move(*client_end), dispatcher());
  ZX_ASSERT(realm.is_valid());

  fuchsia_component_decl::Child child_decl;
  child_decl.name(child_name);
  child_decl.url(kChildUrl);
  child_decl.startup(fuchsia_component_decl::StartupMode::kLazy);

  fuchsia_component_decl::CollectionRef collection;
  collection.name(kCollectionName);

  fuchsia_component::CreateChildArgs child_args;

  realm->CreateChild({collection, child_decl, std::move(child_args)})
      .ThenExactlyOnce([this](fidl::Result<fuchsia_component::Realm::CreateChild>& result) {
        if (!result.is_ok()) {
          FX_LOGS(ERROR) << "CreateChild failed: " << result.error_value();
          ZX_PANIC("%s", result.error_value().FormatDescription().c_str());
        }
        QuitLoop();
      });
  RunLoop();

  fuchsia_component_decl::ChildRef child_ref;
  child_ref.collection(kCollectionName);
  child_ref.name(child_name);

  auto exposed_endpoints = fidl::Endpoints<fuchsia_io::Directory>::Create();
  auto exposed_client = std::move(exposed_endpoints.client);

  realm->OpenExposedDir({child_ref, std::move(exposed_endpoints.server)})
      .ThenExactlyOnce([this](fidl::Result<fuchsia_component::Realm::OpenExposedDir>& result) {
        if (!result.is_ok()) {
          FX_LOGS(ERROR) << "OpenExposedDir failed: " << result.error_value();
          ZX_PANIC("%s", result.error_value().FormatDescription().c_str());
        }
        QuitLoop();
      });
  RunLoop();

  zx::result bind_result = component::ConnectAt<fuchsia_component::Binder>(exposed_client);
  if (!bind_result.is_ok()) {
    FX_LOGS(ERROR) << "Opening fuchsia.component.Binder failed: " << bind_result.error_value();
  }

  auto data = GetInspect(child_name);

  EXPECT_EQ(expected_greeting, data.GetByPath({"root", "config", "greeting"}).GetString());
}

TEST_F(IntegrationTest, ConfigCppRealmBuilderParentOverride) {
  auto realm_builder = component_testing::RealmBuilder::Create();
  auto options =
      component_testing::ChildOptions{.startup_mode = component_testing::StartupMode::EAGER};
  auto child_name = "config_example_override_mutable_by_parent";
  realm_builder.AddChild(child_name, kChildUrl, options);

  std::vector<component_testing::ConfigCapability> configurations;
  configurations.push_back({
      .name = "fuchsia.example.config.Greeting",
      .value = component_testing::ConfigValue("parent component"),
  });
  realm_builder.AddConfiguration(std::move(configurations));
  realm_builder.AddRoute({
      .capabilities = {component_testing::Config{.name = "fuchsia.example.config.Greeting"}},
      .source = component_testing::SelfRef{},
      .targets = {component_testing::ChildRef{child_name}},
  });

  realm_builder.AddRoute(component_testing::Route{
      .capabilities = {component_testing::Protocol{"fuchsia.logger.LogSink"}},
      .source = component_testing::ParentRef(),
      .targets = {component_testing::ChildRef{child_name}}});
  auto realm = realm_builder.Build();
  auto moniker = "realm_builder:" + realm.component().GetChildName() + "/" + child_name;

  auto data = GetInspect(child_name, moniker);

  EXPECT_EQ(rapidjson::Value("parent component"), data.GetByPath({"root", "config", "greeting"}));
}

// TODO(https://fxbug.dev/42053123): Include a subpackaged child component in the examples
//  and tests, including the Rust version of this suite.
