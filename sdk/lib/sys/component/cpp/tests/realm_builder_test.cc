// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <errno.h>
#include <fcntl.h>
#include <fidl/fuchsia.component.decl/cpp/fidl.h>
#include <fidl/fuchsia.component/cpp/fidl.h>
#include <fidl/fuchsia.io/cpp/fidl.h>
#include <fidl/fuchsia.logger/cpp/fidl.h>
#include <fidl/test.placeholders/cpp/fidl.h>
#include <fuchsia/component/cpp/fidl.h>
#include <fuchsia/component/decl/cpp/fidl.h>
#include <fuchsia/data/cpp/fidl.h>
#include <fuchsia/examples/cpp/fidl.h>
#include <fuchsia/io/cpp/fidl.h>
#include <fuchsia/logger/cpp/fidl.h>
#include <fuchsia/sys2/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async/dispatcher.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fdio/namespace.h>
#include <lib/fidl/cpp/binding_set.h>
#include <lib/fidl/cpp/comparison.h>
#include <lib/fidl/cpp/string.h>
#include <lib/fit/function.h>
#include <lib/stdcompat/optional.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <lib/sys/component/cpp/tests/utils.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/syslog/cpp/macros.h>
#include <string.h>
#include <zircon/assert.h>
#include <zircon/availability.h>
#include <zircon/status.h>
#include <zircon/types.h>

#include <iostream>
#include <memory>
#include <optional>
#include <sstream>
#include <vector>

#include <gtest/gtest.h>
#include <src/lib/testing/loop_fixture/real_loop_fixture.h>
#include <test/placeholders/cpp/fidl.h>

namespace {

using namespace component_testing;

namespace fdecl = fuchsia::component::decl;

constexpr char kEchoServerUrl[] =
    "fuchsia-pkg://fuchsia.com/component_cpp_testing_realm_builder_tests#meta/echo_server.cm";
constexpr char kEchoServerScUrl[] = "#meta/echo_server_sc.cm";
constexpr char kEchoServerConfigCapUrl[] = "#meta/echo_server_config_cap.cm";
constexpr char kEchoServerFragmentOnlyUrl[] = "#meta/echo_server.cm";
constexpr char kEchoServiceServerUrl[] = "#meta/echo_service_server.cm";

class RealmBuilderTest : public gtest::RealLoopFixture {};

TEST_F(RealmBuilderTest, RoutesProtocolFromChild) {
  static constexpr char kEchoServer[] = "echo_server";

  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddChild(kEchoServer, kEchoServerUrl);
  realm_builder.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                               .source = ChildRef{kEchoServer},
                               .targets = {ParentRef()}});
  auto realm = realm_builder.Build(dispatcher());
  auto echo = realm.component().ConnectSync<test::placeholders::Echo>();
  fidl::StringPtr response;
  ASSERT_EQ(echo->EchoString("hello", &response), ZX_OK);
  EXPECT_EQ(response, fidl::StringPtr("hello"));
}

TEST_F(RealmBuilderTest, PackagedConfigValuesOnly) {
  static constexpr char kEchoServerSc[] = "echo_server_sc";

  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddChild(kEchoServerSc, kEchoServerScUrl);
  realm_builder.AddRoute(Route{.capabilities = {Protocol{fuchsia::logger::LogSink::Name_}},
                               .source = ParentRef(),
                               .targets = {ChildRef{kEchoServerSc}}});
  realm_builder.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                               .source = ChildRef{kEchoServerSc},
                               .targets = {ParentRef()}});
  auto realm = realm_builder.Build(dispatcher());
  auto echo = realm.component().ConnectSync<test::placeholders::Echo>();
  fidl::StringPtr response;
  ASSERT_EQ(echo->EchoString("hello", &response), ZX_OK);
  EXPECT_EQ(response,
            fidl::StringPtr(
                "hello "
                "[1][255][65535][4000000000][8000000000][-127][-32766][-2000000000][-4000000000]["
                "hello][1,0,][1,2,][2,3,][3,4,][4,5,][-1,-2,][-2,-3,][-3,-4,][-4,-5,][foo,bar,]"));
}

TEST_F(RealmBuilderTest, SetConfigValuesOnly) {
  static constexpr char kEchoServerSc[] = "echo_server_sc";

  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddChild(kEchoServerSc, kEchoServerScUrl);
  realm_builder.InitMutableConfigToEmpty(kEchoServerSc);
  realm_builder.SetConfigValue(kEchoServerSc, "my_flag", ConfigValue::Bool(true));
  realm_builder.SetConfigValue(kEchoServerSc, "my_uint8", ConfigValue::Uint8(1));
  realm_builder.SetConfigValue(kEchoServerSc, "my_uint16", ConfigValue::Uint16(1));
  realm_builder.SetConfigValue(kEchoServerSc, "my_uint32", ConfigValue::Uint32(1));
  realm_builder.SetConfigValue(kEchoServerSc, "my_uint64", ConfigValue::Uint64(1));
  realm_builder.SetConfigValue(kEchoServerSc, "my_int8", ConfigValue::Int8(-1));
  realm_builder.SetConfigValue(kEchoServerSc, "my_int16", ConfigValue::Int16(-1));
  realm_builder.SetConfigValue(kEchoServerSc, "my_int32", ConfigValue::Int32(-1));
  realm_builder.SetConfigValue(kEchoServerSc, "my_int64", ConfigValue::Int64(-1));
  realm_builder.SetConfigValue(kEchoServerSc, "my_string", "foo");
  realm_builder.SetConfigValue(kEchoServerSc, "my_vector_of_flag", std::vector<bool>{false, true});
  realm_builder.SetConfigValue(kEchoServerSc, "my_vector_of_uint8", std::vector<uint8_t>{1, 1});
  realm_builder.SetConfigValue(kEchoServerSc, "my_vector_of_uint16", std::vector<uint16_t>{1, 1});
  realm_builder.SetConfigValue(kEchoServerSc, "my_vector_of_uint32", std::vector<uint32_t>{1, 1});
  realm_builder.SetConfigValue(kEchoServerSc, "my_vector_of_uint64", std::vector<uint64_t>{1, 1});
  realm_builder.SetConfigValue(kEchoServerSc, "my_vector_of_int8", std::vector<int8_t>{-1, 1});
  realm_builder.SetConfigValue(kEchoServerSc, "my_vector_of_int16", std::vector<int16_t>{-1, 1});
  realm_builder.SetConfigValue(kEchoServerSc, "my_vector_of_int32", std::vector<int32_t>{-1, 1});
  realm_builder.SetConfigValue(kEchoServerSc, "my_vector_of_int64", std::vector<int64_t>{-1, 1});
  realm_builder.SetConfigValue(kEchoServerSc, "my_vector_of_string",
                               std::vector<std::string>{"bar", "foo"});
  realm_builder.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                               .source = ChildRef{kEchoServerSc},
                               .targets = {ParentRef()}});
  realm_builder.AddRoute(Route{.capabilities = {Protocol{fuchsia::logger::LogSink::Name_}},
                               .source = ParentRef(),
                               .targets = {ChildRef{kEchoServerSc}}});
  auto realm = realm_builder.Build(dispatcher());
  auto echo = realm.component().ConnectSync<test::placeholders::Echo>();
  fidl::StringPtr response;
  ASSERT_EQ(echo->EchoString("hello", &response), ZX_OK);
  EXPECT_EQ(response, fidl::StringPtr("hello "
                                      "[1][1][1][1][1][-1][-1][-1][-1][foo][0,1,][1,1,][1,1,][1,1,]"
                                      "[1,1,][-1,1,][-1,1,][-1,1,][-1,1,][bar,foo,]"));
}

TEST_F(RealmBuilderTest, MixPackagedAndSetConfigValues) {
  static constexpr char kEchoServerSc[] = "echo_server_sc";

  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddChild(kEchoServerSc, kEchoServerScUrl);
  realm_builder.InitMutableConfigFromPackage(kEchoServerSc);
  realm_builder.SetConfigValue(kEchoServerSc, "my_flag", ConfigValue::Bool(true));
  realm_builder.SetConfigValue(kEchoServerSc, "my_uint8", ConfigValue::Uint8(1));
  realm_builder.SetConfigValue(kEchoServerSc, "my_uint16", ConfigValue::Uint16(1));
  realm_builder.SetConfigValue(kEchoServerSc, "my_uint32", ConfigValue::Uint32(1));
  realm_builder.SetConfigValue(kEchoServerSc, "my_uint64", ConfigValue::Uint64(1));
  realm_builder.SetConfigValue(kEchoServerSc, "my_int8", ConfigValue::Int8(-1));
  realm_builder.SetConfigValue(kEchoServerSc, "my_int16", ConfigValue::Int16(-1));
  realm_builder.SetConfigValue(kEchoServerSc, "my_int32", ConfigValue::Int32(-1));
  realm_builder.SetConfigValue(kEchoServerSc, "my_int64", ConfigValue::Int64(-1));
  realm_builder.SetConfigValue(kEchoServerSc, "my_string", "foo");
  realm_builder.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                               .source = ChildRef{kEchoServerSc},
                               .targets = {ParentRef()}});
  realm_builder.AddRoute(Route{.capabilities = {Protocol{fuchsia::logger::LogSink::Name_}},
                               .source = ParentRef(),
                               .targets = {ChildRef{kEchoServerSc}}});
  auto realm = realm_builder.Build(dispatcher());
  auto echo = realm.component().ConnectSync<test::placeholders::Echo>();
  fidl::StringPtr response;
  ASSERT_EQ(echo->EchoString("hello", &response), ZX_OK);
  EXPECT_EQ(response, fidl::StringPtr("hello "
                                      "[1][1][1][1][1][-1][-1][-1][-1][foo][1,0,][1,2,][2,3,][3,4,]"
                                      "[4,5,][-1,-2,][-2,-3,][-3,-4,][-4,-5,][foo,bar,]"));
}

TEST_F(RealmBuilderTest, SetConfigValueFails) {
  ASSERT_DEATH(
      {
        static constexpr char kEchoServer[] = "echo_server";
        auto realm_builder = RealmBuilder::Create();
        realm_builder.AddChild(kEchoServer, kEchoServerFragmentOnlyUrl);
        realm_builder.SetConfigValue(kEchoServer, "my_flag", ConfigValue::Bool(true));
      },
      "");
  ASSERT_DEATH(
      {
        static constexpr char kEchoServerSc[] = "echo_server_sc";
        auto realm_builder = RealmBuilder::Create();
        realm_builder.AddChild(kEchoServerSc, kEchoServerScUrl);
        realm_builder.SetConfigValue(kEchoServerSc, "doesnt_exist", ConfigValue::Bool(true));
      },
      "");
  ASSERT_DEATH(
      {
        static constexpr char kEchoServerSc[] = "echo_server_sc";
        auto realm_builder = RealmBuilder::Create();
        realm_builder.AddChild(kEchoServerSc, kEchoServerScUrl);
        realm_builder.SetConfigValue(kEchoServerSc, "my_string", ConfigValue::Bool(true));
      },
      "");
  ASSERT_DEATH(
      {
        static constexpr char kEchoServerSc[] = "echo_server_sc";
        auto realm_builder = RealmBuilder::Create();
        realm_builder.AddChild(kEchoServerSc, kEchoServerScUrl);
        realm_builder.SetConfigValue(kEchoServerSc, "my_string", "abccdefghijklmnop");
      },
      "");
  ASSERT_DEATH(
      {
        static constexpr char kEchoServerSc[] = "echo_server_sc";
        auto realm_builder = RealmBuilder::Create();
        realm_builder.AddChild(kEchoServerSc, kEchoServerScUrl);
        realm_builder.SetConfigValue(kEchoServerSc, "my_string",
                                     std::vector<std::string>{"abcdefghijklmnopqrstuvwxyz", "abc"});
      },
      "");
}

TEST_F(RealmBuilderTest, ConfigCapabilityDefaultValuesOnly) {
  static constexpr char kEchoServerConfigCap[] = "echo_server_config_cap";

  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddChild(kEchoServerConfigCap, kEchoServerConfigCapUrl);
  realm_builder.AddRoute(Route{.capabilities = {Protocol{fuchsia::logger::LogSink::Name_}},
                               .source = ParentRef(),
                               .targets = {ChildRef{kEchoServerConfigCap}}});
  realm_builder.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                               .source = ChildRef{kEchoServerConfigCap},
                               .targets = {ParentRef()}});
  auto realm = realm_builder.Build(dispatcher());
  auto echo = realm.component().ConnectSync<test::placeholders::Echo>();
  fidl::StringPtr response;
  ASSERT_EQ(echo->EchoString("hello", &response), ZX_OK);
  EXPECT_EQ(response,
            fidl::StringPtr(
                "hello "
                "[1][255][65535][4000000000][8000000000][-127][-32766][-2000000000][-4000000000]["
                "hello][1,0,][1,2,][2,3,][3,4,][4,5,][-1,-2,][-2,-3,][-3,-4,][-4,-5,][foo,bar,]"));
}

TEST_F(RealmBuilderTest, RoutesAllConfigCapabilities) {
  static constexpr char kEchoServerConfigCap[] = "echo_server_config_cap";

  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddChild(kEchoServerConfigCap, kEchoServerConfigCapUrl);
  std::vector<ConfigCapability> configurations;
  configurations.push_back({.name = "fuchsia.config.MyFlag", .value = ConfigValue::Bool(false)});
  configurations.push_back({.name = "fuchsia.config.MyUint8", .value = ConfigValue::Uint8(1)});
  configurations.push_back({.name = "fuchsia.config.MyUint16", .value = ConfigValue::Uint16(1)});
  configurations.push_back({.name = "fuchsia.config.MyUint32", .value = ConfigValue::Uint32(1)});
  configurations.push_back({.name = "fuchsia.config.MyUint64", .value = ConfigValue::Uint64(1)});
  configurations.push_back({.name = "fuchsia.config.MyInt8", .value = ConfigValue::Int8(-1)});
  configurations.push_back({.name = "fuchsia.config.MyInt16", .value = ConfigValue::Int16(-1)});
  configurations.push_back({.name = "fuchsia.config.MyInt32", .value = ConfigValue::Int32(-1)});
  configurations.push_back({.name = "fuchsia.config.MyInt64", .value = ConfigValue::Int64(-1)});
  configurations.push_back({.name = "fuchsia.config.MyString", .value = "foo"});
  configurations.push_back(
      {.name = "fuchsia.config.MyVectorOfFlag", .value = std::vector<bool>{false, true}});
  configurations.push_back(
      {.name = "fuchsia.config.MyVectorOfUint8", .value = std::vector<uint8_t>{1, 1}});
  configurations.push_back(
      {.name = "fuchsia.config.MyVectorOfUint16", .value = std::vector<uint16_t>{1, 1}});
  configurations.push_back(
      {.name = "fuchsia.config.MyVectorOfUint32", .value = std::vector<uint32_t>{1, 1}});
  configurations.push_back(
      {.name = "fuchsia.config.MyVectorOfUint64", .value = std::vector<uint64_t>{1, 1}});
  configurations.push_back(
      {.name = "fuchsia.config.MyVectorOfInt8", .value = std::vector<int8_t>{-1, 1}});
  configurations.push_back(
      {.name = "fuchsia.config.MyVectorOfInt16", .value = std::vector<int16_t>{-1, 1}});
  configurations.push_back(
      {.name = "fuchsia.config.MyVectorOfInt32", .value = std::vector<int32_t>{-1, 1}});
  configurations.push_back(
      {.name = "fuchsia.config.MyVectorOfInt64", .value = std::vector<int64_t>{-1, 1}});
  configurations.push_back(
      {.name = "fuchsia.config.MyVectorOfString", .value = std::vector<std::string>{"bar", "foo"}});
  realm_builder.AddConfiguration(std::move(configurations));
  realm_builder.AddRoute(Route{.capabilities =
                                   {
                                       Config{.name = "fuchsia.config.MyFlag"},
                                       Config{.name = "fuchsia.config.MyUint8"},
                                       Config{.name = "fuchsia.config.MyUint16"},
                                       Config{.name = "fuchsia.config.MyUint32"},
                                       Config{.name = "fuchsia.config.MyUint64"},
                                       Config{.name = "fuchsia.config.MyInt8"},
                                       Config{.name = "fuchsia.config.MyInt16"},
                                       Config{.name = "fuchsia.config.MyInt32"},
                                       Config{.name = "fuchsia.config.MyInt64"},
                                       Config{.name = "fuchsia.config.MyString"},
                                       Config{.name = "fuchsia.config.MyVectorOfFlag"},
                                       Config{.name = "fuchsia.config.MyVectorOfUint8"},
                                       Config{.name = "fuchsia.config.MyVectorOfUint16"},
                                       Config{.name = "fuchsia.config.MyVectorOfUint32"},
                                       Config{.name = "fuchsia.config.MyVectorOfUint64"},
                                       Config{.name = "fuchsia.config.MyVectorOfInt8"},
                                       Config{.name = "fuchsia.config.MyVectorOfInt16"},
                                       Config{.name = "fuchsia.config.MyVectorOfInt32"},
                                       Config{.name = "fuchsia.config.MyVectorOfInt64"},
                                       Config{.name = "fuchsia.config.MyVectorOfString"},
                                   },
                               .source = SelfRef{},
                               .targets = {ChildRef{kEchoServerConfigCap}}});
  realm_builder.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                               .source = ChildRef{kEchoServerConfigCap},
                               .targets = {ParentRef()}});
  realm_builder.AddRoute(Route{.capabilities = {Protocol{fuchsia::logger::LogSink::Name_}},
                               .source = ParentRef(),
                               .targets = {ChildRef{kEchoServerConfigCap}}});
  auto realm = realm_builder.Build(dispatcher());
  auto echo = realm.component().ConnectSync<test::placeholders::Echo>();
  fidl::StringPtr response;
  ASSERT_EQ(echo->EchoString("hello", &response), ZX_OK);
  EXPECT_EQ(response, fidl::StringPtr("hello "
                                      "[0][1][1][1][1][-1][-1][-1][-1][foo][0,1,][1,1,][1,1,][1,1,]"
                                      "[1,1,][-1,1,][-1,1,][-1,1,][-1,1,][bar,foo,]"));
}

TEST_F(RealmBuilderTest, MixDefaultAndRoutedConfigCapabilities) {
  static constexpr char kEchoServerConfigCap[] = "echo_server_config_cap";

  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddChild(kEchoServerConfigCap, kEchoServerConfigCapUrl);
  std::vector<ConfigCapability> configurations;
  configurations.push_back({.name = "fuchsia.config.MyFlag", .value = ConfigValue::Bool(false)});
  configurations.push_back({.name = "fuchsia.config.MyUint8", .value = ConfigValue::Uint8(1)});
  configurations.push_back({.name = "fuchsia.config.MyUint16", .value = ConfigValue::Uint16(1)});
  configurations.push_back({.name = "fuchsia.config.MyUint32", .value = ConfigValue::Uint32(1)});
  configurations.push_back({.name = "fuchsia.config.MyUint64", .value = ConfigValue::Uint64(1)});
  configurations.push_back({.name = "fuchsia.config.MyInt8", .value = ConfigValue::Int8(-1)});
  configurations.push_back({.name = "fuchsia.config.MyInt16", .value = ConfigValue::Int16(-1)});
  configurations.push_back({.name = "fuchsia.config.MyInt32", .value = ConfigValue::Int32(-1)});
  configurations.push_back({.name = "fuchsia.config.MyInt64", .value = ConfigValue::Int64(-1)});
  configurations.push_back({.name = "fuchsia.config.MyString", .value = "foo"});
  realm_builder.AddConfiguration(std::move(configurations));
  realm_builder.AddRoute(Route{.capabilities =
                                   {
                                       Config{.name = "fuchsia.config.MyFlag"},
                                       Config{.name = "fuchsia.config.MyUint8"},
                                       Config{.name = "fuchsia.config.MyUint16"},
                                       Config{.name = "fuchsia.config.MyUint32"},
                                       Config{.name = "fuchsia.config.MyUint64"},
                                       Config{.name = "fuchsia.config.MyInt8"},
                                       Config{.name = "fuchsia.config.MyInt16"},
                                       Config{.name = "fuchsia.config.MyInt32"},
                                       Config{.name = "fuchsia.config.MyInt64"},
                                       Config{.name = "fuchsia.config.MyString"},
                                   },
                               .source = SelfRef{},
                               .targets = {ChildRef{kEchoServerConfigCap}}});
  realm_builder.AddRoute(
      Route{.capabilities = {Config{.name = "fuchsia.config.MyVectorOfFlag",
                                    .availability = fdecl::Availability::OPTIONAL}},
            .source = VoidRef{},
            .targets = {ChildRef{kEchoServerConfigCap}}});
  realm_builder.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                               .source = ChildRef{kEchoServerConfigCap},
                               .targets = {ParentRef()}});
  realm_builder.AddRoute(Route{.capabilities = {Protocol{fuchsia::logger::LogSink::Name_}},
                               .source = ParentRef(),
                               .targets = {ChildRef{kEchoServerConfigCap}}});
  auto realm = realm_builder.Build(dispatcher());
  auto echo = realm.component().ConnectSync<test::placeholders::Echo>();
  fidl::StringPtr response;
  ASSERT_EQ(echo->EchoString("hello", &response), ZX_OK);
  EXPECT_EQ(response, fidl::StringPtr("hello "
                                      "[0][1][1][1][1][-1][-1][-1][-1][foo][1,0,][1,2,][2,3,][3,4,]"
                                      "[4,5,][-1,-2,][-2,-3,][-3,-4,][-4,-5,][foo,bar,]"));
}

TEST_F(RealmBuilderTest, RoutesProtocolFromRelativeChild) {
  static constexpr char kEchoServer[] = "echo_server";

  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddChild(kEchoServer, kEchoServerFragmentOnlyUrl);
  realm_builder.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                               .source = ChildRef{kEchoServer},
                               .targets = {ParentRef()}});
  auto realm = realm_builder.Build(dispatcher());
  auto echo = realm.component().ConnectSync<test::placeholders::Echo>();
  fidl::StringPtr response;
  ASSERT_EQ(echo->EchoString("hello", &response), ZX_OK);
  EXPECT_EQ(response, fidl::StringPtr("hello"));
}

TEST_F(RealmBuilderTest, RoutesProtocolFromDictionary) {
  static constexpr char kEchoServer[] = "echo_server";

  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddChild(kEchoServer, kEchoServerFragmentOnlyUrl);
  fuchsia::component::decl::Dictionary dict;
  dict.set_name("dict");
  realm_builder.AddCapability(
      fuchsia::component::decl::Capability::WithDictionary(std::move(dict)));
  realm_builder.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                               .source = ChildRef{kEchoServer},
                               .targets = {DictionaryRef{.base_ref = SelfRef(), .path = "dict"}}});
  auto protocol = Protocol{.name = test::placeholders::Echo::Name_};
  realm_builder.AddRoute(Route{.capabilities = {protocol},
                               .source = DictionaryRef{.base_ref = SelfRef(), .path = "dict"},
                               .targets = {ParentRef()}});
  auto realm = realm_builder.Build(dispatcher());
  auto echo = realm.component().ConnectSync<test::placeholders::Echo>();
  fidl::StringPtr response;
  ASSERT_EQ(echo->EchoString("hello", &response), ZX_OK);
  EXPECT_EQ(response, fidl::StringPtr("hello"));
}

class LocalEchoServer : public test::placeholders::Echo, public LocalComponentImpl {
 public:
  explicit LocalEchoServer(async_dispatcher_t* dispatcher, fit::closure on_start = nullptr,
                           fit::closure on_destruct = nullptr, bool exit_after_serve = false)
      : dispatcher_(dispatcher),
        on_start_(std::move(on_start)),
        on_destruct_(std::move(on_destruct)),
        exit_after_serve_(exit_after_serve) {}

  ~LocalEchoServer() override {
    if (on_destruct_) {
      on_destruct_();
    }
  }

  void EchoString(fidl::StringPtr value, EchoStringCallback callback) override {
    callback(std::move(value));
    if (exit_after_serve_) {
      Exit(ZX_ERR_CANCELED);
    }
  }

  void OnStart() override {
    if (on_start_) {
      on_start_();
    }
    ASSERT_EQ(outgoing()->AddPublicService(bindings_.GetHandler(this, dispatcher_)), ZX_OK);
  }

  void OnStop() override {
    if (on_stop_) {
      on_stop_();
    }
  }

 private:
  async_dispatcher_t* dispatcher_;
  fit::closure on_start_;
  fit::closure on_stop_;
  fit::closure on_destruct_;
  fidl::BindingSet<test::placeholders::Echo> bindings_;
  bool exit_after_serve_;
};

// Demonstrates the recommended pattern for implementing a restartable
// LocalComponentImpl. A new LocalComponentImpl is returned when requested.
// After instance initialization, the LocalComponentRunner calls `OnStart()`,
// and the handles are accessible via ns(), svc(), and outgoing(). The instance
// should create and store the bindings.
//
// The component instance will continue to serve requests until the component is
// stopped, or until it is destroyed with the Realm.
TEST_F(RealmBuilderTest, RoutesProtocolUsesLocalComponentFactory) {
  static constexpr char kEchoServer[] = "echo_server";
  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddLocalChild(
      kEchoServer, [&]() { return std::make_unique<LocalEchoServer>(dispatcher()); },
      ChildOptions{});
  realm_builder.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                               .source = ChildRef{kEchoServer},
                               .targets = {ParentRef()}});
  auto realm = realm_builder.Build(dispatcher());
  auto cleanup = fit::defer([&]() {
    bool complete = false;
    realm.Teardown([&](fit::result<fuchsia::component::Error> result) { complete = true; });
    RunLoopUntil([&]() { return complete; });
  });
  test::placeholders::EchoPtr echo;
  ASSERT_EQ(realm.component().Connect(echo.NewRequest()), ZX_OK);
  bool was_called = false;
  echo->EchoString("hello", [&](const fidl::StringPtr& response) {
    was_called = true;
    ASSERT_EQ(response, "hello");
  });
  RunLoopUntil([&]() { return was_called; });
}

// Demonstrates a LocalComponentImpl can restart after exit.
//
// Note that a test flake was reported in https://fxbug.dev/42062533 in this test.
// This test can occasionally time out, which probably means that at least
// one of the expected events is not always occurring.
//
// I've added error handles on the proxies, and logging to track expected
// events. If the flake happens again, the logs will help me understand why.
TEST_F(RealmBuilderTest, ComponentCanStopAndBeRestarted) {
  static constexpr char kEchoServer[] = "echo_server";
  auto realm_builder = RealmBuilder::Create();
  bool started = false;
  bool got_response = false;
  bool destructed = false;
  bool got_peer_closed = false;
  realm_builder.AddLocalChild(kEchoServer, [&]() {
    return std::make_unique<LocalEchoServer>(
        dispatcher(),
        /*on_start=*/
        [&started]() {
          FX_LOGS(INFO) << "ComponentCanStopAndBeRestarted: started = true";
          started = true;
        },
        /*on_destruct=*/
        [&destructed]() {
          FX_LOGS(INFO) << "ComponentCanStopAndBeRestarted: destructed = true";
          destructed = true;
        },
        /*exit_after_serve=*/true);
  });
  realm_builder.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                               .source = ChildRef{kEchoServer},
                               .targets = {ParentRef()}});
  auto realm = realm_builder.Build(dispatcher());
  test::placeholders::EchoPtr echo;
  ASSERT_EQ(realm.component().Connect(echo.NewRequest()), ZX_OK);
  echo.set_error_handler([&](zx_status_t status) {
    FX_PLOGS(INFO, status) << "ComponentCanStopAndBeRestarted: Echo proxy error";
    if (status == ZX_ERR_PEER_CLOSED) {
      got_peer_closed = true;
    }
  });
  echo->EchoString("hello", [&](const fidl::StringPtr& response) {
    FX_LOGS(INFO) << "ComponentCanStopAndBeRestarted: got_response = true, response = " << response;
    got_response = true;
    ASSERT_EQ(response, "hello");
  });

  FX_LOGS(INFO) << "ComponentCanStopAndBeRestarted: waiting for started && got_response && "
                   "destructed && got_peer_closed";
  RunLoopUntil([&]() { return started && got_response && destructed && got_peer_closed; });

  // When the local component exits (before the `while` loop), the
  // `LocalComponentRunner` closes the component's `ComponentController`.
  //
  // It's possible (if rare) that a second call to `EchoString()` will not
  // connect to a new LocalComponent instance, and a second `ERR_PEER_CLOSED`
  // can occur, failing the `EchoString()` request. A theory is that component
  // manager did not have a chance to observe the `ComponentController` closed
  // event and/or fully stop the component in order to allow the component to
  // re-start.
  //
  // To work around this failure, re-try until it succeeds.
  //
  // This approach is expected to fix an automatically-reported flake bug
  // (https://fxbug.dev/42062533).

  got_response = false;
  got_peer_closed = false;
  while (!got_response && !got_peer_closed) {
    FX_LOGS(INFO) << "ComponentCanStopAndBeRestarted: resetting the flags to call again";
    started = false;
    destructed = false;

    // The component destructed, but it will start up again when another request
    // is made.
    ASSERT_EQ(realm.component().Connect(echo.NewRequest()), ZX_OK);
    echo->EchoString("You're back!", [&](const fidl::StringPtr& response) {
      FX_LOGS(INFO) << "ComponentCanStopAndBeRestarted: got_response = true, second response = "
                    << response;
      got_response = true;
      ASSERT_EQ(response, "You're back!");
    });

    FX_LOGS(INFO) << "ComponentCanStopAndBeRestarted: waiting again for started && got_response && "
                     "destructed";
    RunLoopUntil([&]() { return got_peer_closed || (started && got_response && destructed); });
  }
  FX_LOGS(INFO) << "ComponentCanStopAndBeRestarted: done";
}

TEST_F(RealmBuilderTest, StartAndStop) {
  static constexpr char kEchoServer[] = "echo_server";
  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddLocalChild(kEchoServer, [&]() {
    return std::make_unique<LocalEchoServer>(dispatcher(), /*on_start=*/
                                             nullptr,
                                             /*on_destruct=*/
                                             nullptr,
                                             /*exit_after_serve=*/true);
  });
  // A component controller can only be acquired for a direct descendant, so we
  // need to make the local component the root of the constructed realm. Before
  // we do that though, let's use the `AddRoute` call to cause the expose
  // declaration for the echo protocol to be added to the local component's
  // manifest.
  realm_builder.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                               .source = ChildRef{kEchoServer},
                               .targets = {ParentRef()}});
  auto child_decl = realm_builder.GetComponentDecl(kEchoServer);
  realm_builder.ReplaceRealmDecl(std::move(child_decl));
  realm_builder.StartOnBuild(false);
  auto realm = realm_builder.Build(dispatcher());

  // We can start a component and see it exit on its own.
  {
    auto execution_controller = realm.component().Start();
    auto stopped_payload = std::optional<fuchsia::component::StoppedPayload>{};
    execution_controller.OnStop([&](fuchsia::component::StoppedPayload payload) {
      stopped_payload = std::make_optional<fuchsia::component::StoppedPayload>(std::move(payload));
    });

    // Once we use the echo service, the local component will exit.
    test::placeholders::EchoPtr echo;
    ASSERT_EQ(realm.component().Connect(echo.NewRequest()), ZX_OK);
    echo->EchoString({"hello"}, [](fidl::StringPtr response) {});

    RunLoopUntil([&]() { return stopped_payload.has_value(); });
    ASSERT_EQ(stopped_payload->status(), ZX_ERR_CANCELED);
  }

  // We can start a component, cause it to stop, and observe that stop.
  {
    auto execution_controller = realm.component().Start();
    auto stopped_payload = std::optional<fuchsia::component::StoppedPayload>{};
    execution_controller.OnStop([&](fuchsia::component::StoppedPayload payload) {
      stopped_payload = std::make_optional<fuchsia::component::StoppedPayload>(std::move(payload));
    });
    execution_controller.Stop();
    RunLoopUntil([&]() { return stopped_payload.has_value(); });
    ASSERT_EQ(stopped_payload->status(), ZX_OK);
  }
}

// Tests and demonstrates a discouraged pattern for calling AddLocalChild()
// that pre-builds a component instance, saves a direct raw pointer, and
// std::moves the instance to the LocalComponentFactory. The instance
// will be returned by the factory when the first component instance starts,
// making the captured instance invalid for subsequent requests to start a new
// instance.
//
// Existing C++ RealmBuilder clients migrating from the deprecated
// AddLocalChild(LocalComponent*) function (which only supported a single
// instance) may want to follow this pattern for simpler migration to the
// new AddLocalChild() API.
TEST_F(RealmBuilderTest, RoutesProtocolFromPrebuiltLocalComponentInstance) {
  static constexpr char kEchoServer[] = "echo_server";
  auto local_echo_server = std::make_unique<LocalEchoServer>(dispatcher());
  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddLocalChild(
      kEchoServer,
      [&, local_echo_server = std::move(local_echo_server)]() mutable {
        // Note: This lambda does not create a new instance,
        // so the component can only be started once.
        return std::move(local_echo_server);
      },
      ChildOptions{});
  realm_builder.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                               .source = ChildRef{kEchoServer},
                               .targets = {ParentRef()}});
  auto realm = realm_builder.Build(dispatcher());
  auto cleanup = fit::defer([&]() {
    bool complete = false;
    realm.Teardown([&](fit::result<fuchsia::component::Error> result) { complete = true; });
    RunLoopUntil([&]() { return complete; });
  });
  test::placeholders::EchoPtr echo;
  ASSERT_EQ(realm.component().Connect(echo.NewRequest()), ZX_OK);
  bool was_called = false;
  echo->EchoString("hello", [&](const fidl::StringPtr& response) {
    was_called = true;
    ASSERT_EQ(response, "hello");
  });
  RunLoopUntil([&]() { return was_called; });
}

class EchoClientSyncLocalComponent : public LocalComponentImpl {
 public:
  explicit EchoClientSyncLocalComponent(fit::closure on_success)
      : on_success_(std::move(on_success)) {}

  void OnStart() override {
    test::placeholders::EchoSyncPtr echo;
    ASSERT_EQ(svc().Connect<test::placeholders::Echo>(echo.NewRequest()), ZX_OK);
    fidl::StringPtr response;
    ASSERT_EQ(echo->EchoString("milk", &response), ZX_OK);
    ASSERT_EQ(response, fidl::StringPtr("milk"));
    if (on_success_) {
      on_success_();
    }
    Exit();
  }

 private:
  fit::closure on_success_;
};

// Tests and demonstrates creating a local client-only component that can
// terminate immediately, after completing its work.
//
// Note: This client component uses a `SyncPtr` proxy binding, which invokes
// the call and (in the case of Echo::EchoString()) waits for the response
// synchronously.
TEST_F(RealmBuilderTest, RoutesProtocolToLocalComponentSync) {
  static constexpr char kEchoClient[] = "echo_client";
  static constexpr char kEchoServer[] = "echo_server";

  auto realm_builder = RealmBuilder::Create();
  bool success = false;
  realm_builder.AddLocalChild(
      kEchoClient,
      [&success]() {
        return std::make_unique<EchoClientSyncLocalComponent>([&success]() { success = true; });
      },
      ChildOptions{.startup_mode = StartupMode::EAGER});
  realm_builder.AddChild(kEchoServer, kEchoServerUrl);
  realm_builder.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                               .source = ChildRef{kEchoServer},
                               .targets = {ChildRef{kEchoClient}}});
  realm_builder.AddRoute(Route{.capabilities = {Protocol{fuchsia::logger::LogSink::Name_}},
                               .source = ParentRef(),
                               .targets = {ChildRef{kEchoServer}}});
  auto realm = realm_builder.Build(dispatcher());
  RunLoopUntil([&success]() { return success; });
}

class EchoClientLocalComponent : public LocalComponentImpl {
 public:
  explicit EchoClientLocalComponent(fit::closure on_success) : on_success_(std::move(on_success)) {}

  void OnStart() override {
    ASSERT_EQ(svc().Connect<test::placeholders::Echo>(echo_.NewRequest()), ZX_OK);
    echo_->EchoString("hello", [&](const fidl::StringPtr& response) {
      ASSERT_EQ("hello", response);
      if (on_success_) {
        on_success_();
      }
      Exit();
    });
  }

 private:
  test::placeholders::EchoPtr echo_;
  fit::closure on_success_;
};

// Tests and demonstrates creating a local client-only component that uses
// an asynchronous proxy binding to call a service and get a response. Since
// the call is asynchronous, the component must not call `Exit()`
// until it gets a response.
TEST_F(RealmBuilderTest, RoutesProtocolToLocalComponentAsync) {
  static constexpr char kEchoClient[] = "echo_client";
  static constexpr char kEchoServer[] = "echo_server";

  auto realm_builder = RealmBuilder::Create();
  bool success = false;
  realm_builder.AddLocalChild(
      kEchoClient,
      [&success]() {
        return std::make_unique<EchoClientLocalComponent>([&success]() { success = true; });
      },
      ChildOptions{.startup_mode = StartupMode::EAGER});
  realm_builder.AddChild(kEchoServer, kEchoServerUrl);
  realm_builder.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                               .source = ChildRef{kEchoServer},
                               .targets = {ChildRef{kEchoClient}}});
  realm_builder.AddRoute(Route{.capabilities = {Protocol{fuchsia::logger::LogSink::Name_}},
                               .source = ParentRef(),
                               .targets = {ChildRef{kEchoServer}}});
  auto realm = realm_builder.Build(dispatcher());
  RunLoopUntil([&success]() { return success; });
}

TEST_F(RealmBuilderTest, RoutesServiceFromChild) {
  static constexpr char kEchoServiceServer[] = "echo_service_server";

  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddChild(kEchoServiceServer, kEchoServiceServerUrl);
  realm_builder.AddRoute(Route{.capabilities = {Service{fuchsia::examples::EchoService::Name}},
                               .source = ChildRef{kEchoServiceServer},
                               .targets = {ParentRef()}});
  realm_builder.AddRoute(Route{.capabilities = {Protocol{fuchsia::logger::LogSink::Name_}},
                               .source = ParentRef(),
                               .targets = {ChildRef{kEchoServiceServer}}});
  auto realm = realm_builder.Build(dispatcher());

  auto default_service =
      sys::OpenServiceAt<fuchsia::examples::EchoService>(realm.component().CloneExposedDir());
  auto regular = default_service.regular_echo().Connect().Bind();

  constexpr char kMessage[] = "Ping!";
  bool was_called = false;
  regular->EchoString(kMessage,
                      [expected_reply = kMessage, &was_called](const fidl::StringPtr& value) {
                        was_called = true;
                        ASSERT_EQ(value, expected_reply);
                      });
  RunLoopUntil([&]() { return was_called; });
}

TEST_F(RealmBuilderTest, ConnectsToChannelDirectly) {
  static constexpr char kEchoServer[] = "echo_server";

  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddChild(kEchoServer, kEchoServerUrl);
  realm_builder.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                               .source = ChildRef{kEchoServer},
                               .targets = {ParentRef()}});
  auto realm = realm_builder.Build(dispatcher());

  zx::channel controller, request;
  ASSERT_EQ(zx::channel::create(0, &controller, &request), ZX_OK);
  fidl::SynchronousInterfacePtr<test::placeholders::Echo> echo;
  echo.Bind(std::move(controller));
  ASSERT_EQ(realm.component().Connect(test::placeholders::Echo::Name_, std::move(request)), ZX_OK);
  fidl::StringPtr response;
  ASSERT_EQ(echo->EchoString("hello", &response), ZX_OK);
  EXPECT_EQ(response, fidl::StringPtr("hello"));
}

TEST_F(RealmBuilderTest, RoutesProtocolFromLocalComponentInSubRealm) {
  static constexpr char kEchoServer[] = "echo_server";
  static constexpr char kSubRealm[] = "sub_realm";
  auto realm_builder = RealmBuilder::Create();
  auto sub_realm = realm_builder.AddChildRealm(kSubRealm);

  // Route test.placeholders.Echo from local Echo server impl to parent.
  sub_realm.AddLocalChild(kEchoServer,
                          [&]() { return std::make_unique<LocalEchoServer>(dispatcher()); });
  sub_realm.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                           .source = ChildRef{kEchoServer},
                           .targets = {ParentRef()}});

  // Route test.placeholders.Echo from sub_realm child to parent.
  realm_builder.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                               .source = ChildRef{kSubRealm},
                               .targets = {ParentRef()}});

  auto realm = realm_builder.Build(dispatcher());
  auto cleanup = fit::defer([&]() {
    bool complete = false;
    realm.Teardown([&](fit::result<fuchsia::component::Error> result) { complete = true; });
    RunLoopUntil([&]() { return complete; });
  });
  test::placeholders::EchoPtr echo;
  ASSERT_EQ(realm.component().Connect(echo.NewRequest()), ZX_OK);
  bool was_called = false;
  echo->EchoString("hello", [&](const fidl::StringPtr& response) {
    was_called = true;
    ASSERT_EQ("hello", response);
  });
  RunLoopUntil([&]() { return was_called; });
}

class FileReader : public LocalComponentImpl {
 public:
  void OnStart() override { started_ = true; }

  std::string GetContentsAt(std::string_view dirpath, std::string_view filepath) {
    ZX_ASSERT_MSG(started_, "FileReader/GetContentsAt called before FileReader was started.");

    constexpr static size_t kMaxBufferSize = 1024;
    static char kReadBuffer[kMaxBufferSize];

    int dirfd = fdio_ns_opendir(ns());
    ZX_ASSERT_MSG(dirfd > 0, "Failed to open root ns as a file descriptor: %s", strerror(errno));

    std::stringstream path_builder;
    path_builder << dirpath << '/' << filepath;
    auto path = path_builder.str();
    int filefd = openat(dirfd, path.c_str(), O_RDONLY);
    ZX_ASSERT_MSG(filefd > 0, "Failed to open path \"%s\": %s", path.c_str(), strerror(errno));

    size_t const bytes_read = read(filefd, reinterpret_cast<void*>(kReadBuffer), kMaxBufferSize);
    ZX_ASSERT_MSG(bytes_read > 0, "Read 0 bytes from file at \"%s\": %s", path.c_str(),
                  strerror(errno));

    return std::string(kReadBuffer, bytes_read);
  }

  bool HasStarted() const { return started_; }

 private:
  bool started_ = false;
};

TEST_F(RealmBuilderTest, RoutesReadOnlyDirectory) {
  static constexpr char kDirectoryName[] = "config";
  static constexpr char kFilename[] = "environment";
  static constexpr char kContent[] = "DEV";

  auto realm_builder = RealmBuilder::Create();

  std::unique_ptr file_reader = std::make_unique<FileReader>();
  FileReader* file_reader_ptr = file_reader.get();
  realm_builder.AddLocalChild(
      "file_reader",
      [file_reader = std::move(file_reader)]() mutable { return std::move(file_reader); },
      ChildOptions{.startup_mode = StartupMode::EAGER});
  realm_builder.RouteReadOnlyDirectory(kDirectoryName, {ChildRef{"file_reader"}},
                                       std::move(DirectoryContents().AddFile(kFilename, kContent)));
  auto realm = realm_builder.Build(dispatcher());
  auto cleanup = fit::defer([&]() {
    bool complete = false;
    realm.Teardown([&](fit::result<fuchsia::component::Error> result) { complete = true; });
    RunLoopUntil([&]() { return complete; });
  });

  RunLoopUntil([&]() { return file_reader_ptr->HasStarted(); });
  EXPECT_EQ(file_reader_ptr->GetContentsAt(kDirectoryName, kFilename), kContent);
}

// This test is similar to RealmBuilderTest.RoutesProtocolFromChild except
// that its setup is done by mutating the realm's root's decl. This is to
// assert that invoking |ReplaceRealmDecl| works as expected.
TEST_F(RealmBuilderTest, RealmDeclCanBeReplaced) {
  static constexpr char kEchoServer[] = "echo_server";

  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddChild(kEchoServer, kEchoServerUrl);

  auto decl = realm_builder.GetRealmDecl();
  fdecl::ExposeProtocol expose_protocol;
  expose_protocol.set_source(fdecl::Ref::WithChild(fdecl::ChildRef{.name = "echo_server"}));
  expose_protocol.set_target(fdecl::Ref::WithParent(fdecl::ParentRef{}));
  expose_protocol.set_source_name("test.placeholders.Echo");
  expose_protocol.set_target_name("test.placeholders.Echo");
  decl.mutable_exposes()->emplace_back(fdecl::Expose::WithProtocol(std::move(expose_protocol)));
  realm_builder.ReplaceRealmDecl(std::move(decl));
  auto realm = realm_builder.Build(dispatcher());

  auto echo = realm.component().ConnectSync<test::placeholders::Echo>();
  fidl::StringPtr response;
  ASSERT_EQ(echo->EchoString("hello", &response), ZX_OK);
  EXPECT_EQ(response, fidl::StringPtr("hello"));
}

// This test is similar to RealmBuilderTest.RoutesProtocolFromChild except
// that its setup is done statically via a manifest. This is to assert that
// invoking |CreateFromRelativeUrl| works as expected.
TEST_F(RealmBuilderTest, BuildsRealmFromFragmentOnlyUrl) {
  static constexpr char kPrePopulatedRealmUrl[] = "#meta/pre_populated_realm.cm";

  auto realm_builder = RealmBuilder::CreateFromRelativeUrl(kPrePopulatedRealmUrl);
  auto realm = realm_builder.Build(dispatcher());
  auto echo = realm.component().ConnectSync<test::placeholders::Echo>();
  fidl::StringPtr response;
  ASSERT_EQ(echo->EchoString("hello", &response), ZX_OK);
  EXPECT_EQ(response, fidl::StringPtr("hello"));
}

class SimpleComponent : public component_testing::LocalComponentImpl {
 public:
  SimpleComponent() = default;
  explicit SimpleComponent(fit::closure on_stop, fit::closure on_destruct)
      : on_stop_(std::move(on_stop)), on_destruct_(std::move(on_destruct)) {}

  ~SimpleComponent() override {
    if (on_destruct_) {
      on_destruct_();
    }
  }

  void OnStart() override { started_ = true; }

  void OnStop() override {
    stopping_ = true;
    if (on_stop_) {
      on_stop_();
    }
  }

  bool IsStarted() const { return started_; }

  bool IsStopping() const { return stopping_; }

 private:
  bool started_ = false;
  bool stopping_ = false;
  fit::closure on_stop_;
  fit::closure on_destruct_;
};

// This test asserts that the LocalComponents are started, not stopped, and
// are eventually destructed when destructing the realm.
TEST_F(RealmBuilderTest, LocalComponentGetsDestructedOnExit) {
  auto realm_builder = RealmBuilder::Create();

  size_t destructors_called = 0;

  // hold pointers to the LocalComponents owned by the realm
  std::vector<SimpleComponent*> components;
  for (size_t i = 0; i < 3; ++i) {
    std::string name = "numbered" + std::to_string(i);
    auto component = std::make_unique<SimpleComponent>(nullptr, [&]() { destructors_called++; });
    components.push_back(component.get());
    realm_builder.AddLocalChild(
        name, [component = std::move(component)]() mutable { return std::move(component); },
        ChildOptions{.startup_mode = StartupMode::EAGER});
  }

  auto realm = realm_builder.Build(dispatcher());
  auto cleanup = fit::defer([&]() {
    bool complete = false;
    realm.Teardown([&](fit::result<fuchsia::component::Error> result) { complete = true; });
    RunLoopUntil([&]() { return complete; });
  });

  for (auto& component : components) {
    ASSERT_FALSE(component->IsStarted());
    ASSERT_FALSE(component->IsStopping());
  }

  // Verify all components have started.
  RunLoopUntil([&]() {
    return std::all_of(components.begin(), components.end(),
                       [](auto& component) { return component->IsStarted(); });
  });

  for (auto& component : components) {
    ASSERT_FALSE(component->IsStopping());
  }

  cleanup.call();

  ASSERT_EQ(destructors_called, components.size());
}

// This test asserts that the LocalComponentImpl::OnStop() method is called when
// the component is stopped (which confirms that the ComponentController
// would have also been dropped).
TEST_F(RealmBuilderTest, LocalComponentGetsLifecycleControllerStop) {
  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddRoute(
      Route{.capabilities = {Protocol{fuchsia::sys2::LifecycleController::Name_}},
            .source = FrameworkRef(),
            .targets = {ParentRef{}}});

  size_t destructors_called = 0;

  std::vector<SimpleComponent*> components;
  for (size_t i = 0; i < 3; ++i) {
    std::string name = "numbered" + std::to_string(i);
    auto component = std::make_unique<SimpleComponent>(nullptr, [&]() { destructors_called++; });
    components.push_back(component.get());
    realm_builder.AddLocalChild(
        name, [component = std::move(component)]() mutable { return std::move(component); },
        ChildOptions{.startup_mode = StartupMode::EAGER});
  }

  auto realm = std::make_optional<RealmRoot>(realm_builder.Build(dispatcher()));
  for (auto& component : components) {
    ASSERT_FALSE(component->IsStarted());
    ASSERT_FALSE(component->IsStopping());
  }

  // Verify all components have started.
  for (auto& component : components) {
    RunLoopUntil([&]() { return component->IsStarted(); });
  }

  for (auto& component : components) {
    ASSERT_FALSE(component->IsStopping());
  }

  // Stop the components and verify all components have been asked to stop.
  auto lifecycle_controller = realm->component().Connect<fuchsia::sys2::LifecycleController>();
  for (size_t i = 0; i < components.size(); ++i) {
    size_t orig = destructors_called;
    std::string moniker = "./numbered" + std::to_string(i);
    lifecycle_controller->StopInstance(moniker,
                                       [](auto result) { ASSERT_TRUE(result.is_response()); });
    RunLoopUntil([&]() { return destructors_called == orig + 1; });
  }
}

// This test asserts that the `LocalComponentImpl::OnStop()` method is called
// when the RealmRoot is torn down (returned from `Realm::DestroyChild()`), and
// the `RealmRoot::Teardown` callback is called when the realm is destroyed.
TEST_F(RealmBuilderTest, LocalComponentGetsRealmRootTeardownStop) {
  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddRoute(
      Route{.capabilities = {Protocol{fuchsia::sys2::LifecycleController::Name_}},
            .source = FrameworkRef(),
            .targets = {ParentRef{}}});

  size_t components_stopped = 0;
  size_t destructors_called = 0;

  std::vector<SimpleComponent*> components;
  for (size_t i = 0; i < 3; ++i) {
    std::string name = "numbered" + std::to_string(i);
    auto component = std::make_unique<SimpleComponent>([&]() { components_stopped++; },
                                                       [&]() { destructors_called++; });
    components.push_back(component.get());
    realm_builder.AddLocalChild(
        name, [component = std::move(component)]() mutable { return std::move(component); },
        ChildOptions{.startup_mode = StartupMode::EAGER});
  }

  auto realm = realm_builder.Build(dispatcher());
  for (auto& component : components) {
    ASSERT_FALSE(component->IsStarted());
    ASSERT_FALSE(component->IsStopping());
  }

  // Verify all components have started.
  for (auto& component : components) {
    RunLoopUntil([&]() { return component->IsStarted(); });
  }

  for (auto& component : components) {
    ASSERT_FALSE(component->IsStopping());
  }

  ASSERT_EQ(components_stopped, 0u);
  ASSERT_EQ(destructors_called, 0u);

  bool realm_is_destroyed = false;
  realm.Teardown([&](fit::result<fuchsia::component::Error> result) {
    // Since the Realm owns the `unique_ptr`s to the SimpleComponents, the
    // realm should have stopped and destructed them before this callback is
    // invoked, so  do not try to use the `components` vector of
    // SimpleComponent* raw pointers!
    realm_is_destroyed = true;
  });
  RunLoopUntil([&]() { return realm_is_destroyed; });

  // Verify all components were stopped _and_ destructed.
  ASSERT_EQ(components_stopped, components.size());
  ASSERT_EQ(destructors_called, components.size());
}

// This test is nearly identically to the
// RealmBuilderTest.RoutesProtocolFromChild test case above. The only difference
// is that it provides a svc directory from the sys::Context singleton object to
// the Realm::Builder::Create method. If the test passes, it must follow that
// Realm::Builder supplied a Context object internally, otherwise the test
// component wouldn't be able to connect to fuchsia.component.Realm protocol.
TEST_F(RealmBuilderTest, UsesProvidedSvcDirectory) {
  auto context = sys::ComponentContext::Create();
  static constexpr char kEchoServer[] = "echo_server";

  auto realm_builder = RealmBuilder::Create(context->svc());
  realm_builder.AddChild(kEchoServer, kEchoServerUrl);
  realm_builder.AddRoute(Route{.capabilities = {Protocol{test::placeholders::Echo::Name_}},
                               .source = ChildRef{kEchoServer},
                               .targets = {ParentRef()}});
  auto realm = realm_builder.Build(dispatcher());
  auto echo = realm.component().ConnectSync<test::placeholders::Echo>();
  fidl::StringPtr response;
  ASSERT_EQ(echo->EchoString("hello", &response), ZX_OK);
  EXPECT_EQ(response, fidl::StringPtr("hello"));
}

TEST_F(RealmBuilderTest, UsesRandomChildName) {
  std::string child_name_1;
  {
    auto realm_builder = RealmBuilder::Create();
    auto realm = realm_builder.Build(dispatcher());
    child_name_1 = realm.component().GetChildName();
  }
  std::string child_name_2;
  {
    auto realm_builder = RealmBuilder::Create();
    auto realm = realm_builder.Build(dispatcher());
    child_name_2 = realm.component().GetChildName();
  }

  EXPECT_NE(child_name_1, child_name_2);
}

TEST_F(RealmBuilderTest, CanCreateLongChildName) {
  std::string child_name_1;
  {
    auto realm_builder = RealmBuilder::Create();
    {
      const std::string long_child_name(fuchsia::component::MAX_NAME_LENGTH + 1, 'a');
      // AddChild should not panic.
      realm_builder.AddChild(long_child_name, kEchoServerUrl);
    }
    {
      const std::string long_child_name(fuchsia::component::MAX_CHILD_NAME_LENGTH, 'a');
      // AddChild should not panic.
      realm_builder.AddChild(long_child_name, kEchoServerUrl);
    }
    {
      ASSERT_DEATH(
          {
            const std::string too_long_child_name(fuchsia::component::MAX_CHILD_NAME_LENGTH + 1,
                                                  'a');
            realm_builder.AddChild(too_long_child_name, kEchoServerUrl);
          },
          "");
    }
  }
}

TEST_F(RealmBuilderTest, PanicsWhenUsingHandlesFromConstructor) {
  class UseNsTooEarly : public LocalComponentImpl {
   public:
    UseNsTooEarly() { ns(); }

    void OnStart() override {}
  };

  ASSERT_DEATH(
      {
        auto realm_builder = RealmBuilder::Create();
        realm_builder.AddLocalChild(
            "child", []() { return std::make_unique<UseNsTooEarly>(); },
            ChildOptions{.startup_mode = StartupMode::EAGER});
        auto realm = realm_builder.Build(dispatcher());
        RunLoop();
      },
      "LocalComponentImplBase::ns\\(\\) cannot be called until RealmBuilder calls Initialize\\(\\)");

  class UseSvcTooEarly : public LocalComponentImpl {
   public:
    UseSvcTooEarly() { svc(); }

    void OnStart() override {}
  };

  ASSERT_DEATH(
      {
        auto realm_builder = RealmBuilder::Create();
        realm_builder.AddLocalChild(
            "child", []() { return std::make_unique<UseSvcTooEarly>(); },
            ChildOptions{.startup_mode = StartupMode::EAGER});
        auto realm = realm_builder.Build(dispatcher());
        RunLoop();
      },
      "LocalHlcppComponent::svc\\(\\) cannot be called until RealmBuilder calls Initialize\\(\\)");

  class UseOutgoingTooEarly : public LocalComponentImpl {
   public:
    UseOutgoingTooEarly() { outgoing(); }

    void OnStart() override {}
  };

  ASSERT_DEATH(
      {
        auto realm_builder = RealmBuilder::Create();
        realm_builder.AddLocalChild(
            "child", []() { return std::make_unique<UseOutgoingTooEarly>(); },
            ChildOptions{.startup_mode = StartupMode::EAGER});
        auto realm = realm_builder.Build(dispatcher());
        RunLoop();
      },
      "LocalHlcppComponent::outgoing\\(\\) cannot be called until RealmBuilder calls Initialize\\(\\)");

  class CallExitTooEarly : public LocalComponentImpl {
   public:
    CallExitTooEarly() { Exit(); }

    void OnStart() override {}
  };

  ASSERT_DEATH(
      {
        auto realm_builder = RealmBuilder::Create();
        realm_builder.AddLocalChild(
            "child", []() { return std::make_unique<CallExitTooEarly>(); },
            ChildOptions{.startup_mode = StartupMode::EAGER});
        auto realm = realm_builder.Build(dispatcher());
        RunLoop();
      },
      "LocalComponentImplBase::Exit\\(\\) cannot be called until RealmBuilder calls Initialize\\(\\)");

  class UseCppSvcTooEarly : public LocalCppComponent {
   public:
    UseCppSvcTooEarly() { std::ignore = svc(); }

    void OnStart() override {}
  };

  ASSERT_DEATH(
      {
        auto realm_builder = RealmBuilder::Create();
        realm_builder.AddLocalChild(
            "child", []() { return std::make_unique<UseCppSvcTooEarly>(); },
            ChildOptions{.startup_mode = StartupMode::EAGER});
        auto realm = realm_builder.Build(dispatcher());
        RunLoop();
      },
      "LocalCppComponent::svc\\(\\) cannot be called until RealmBuilder calls Initialize\\(\\)");

  class UseCppOutgoingTooEarly : public LocalCppComponent {
   public:
    UseCppOutgoingTooEarly() { std::ignore = outgoing(); }

    void OnStart() override {}
  };

  ASSERT_DEATH(
      {
        auto realm_builder = RealmBuilder::Create();
        realm_builder.AddLocalChild(
            "child", []() { return std::make_unique<UseCppOutgoingTooEarly>(); },
            ChildOptions{.startup_mode = StartupMode::EAGER});
        auto realm = realm_builder.Build(dispatcher());
        RunLoop();
      },
      "LocalCppComponent::outgoing\\(\\) cannot be called until RealmBuilder calls Initialize\\(\\)");
}

class LocalEchoServerCpp : public fidl::Server<test_placeholders::Echo>, public LocalCppComponent {
 public:
  explicit LocalEchoServerCpp(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {}

  void EchoString(EchoStringRequest& request, EchoStringCompleter::Sync& completer) override {
    completer.Reply({{.response = request.value()}});
  }

  void OnStart() override {
    zx::result result = outgoing()->AddUnmanagedProtocol<test_placeholders::Echo>(
        bindings_.CreateHandler(this, dispatcher_, fidl::kIgnoreBindingClosure));
    ASSERT_TRUE(result.is_ok()) << result.status_string();
  }

 private:
  async_dispatcher_t* dispatcher_;
  fidl::ServerBindingGroup<test_placeholders::Echo> bindings_;
};

class LocalEchoClientCpp : public LocalCppComponent {
 public:
  LocalEchoClientCpp(async_dispatcher_t* dispatcher, fit::closure on_success)
      : dispatcher_(dispatcher), on_success_(std::move(on_success)) {}

  void OnStart() override {
    // Verify svc() + component::ConnectAt<Protocol>.
    zx::result<fidl::ClientEnd<fuchsia_io::Directory>> svc_dir = svc();
    ASSERT_TRUE(svc_dir.is_ok()) << svc_dir.status_string();
    zx::result<fidl::ClientEnd<test_placeholders::Echo>> echo_from_svc =
        component::ConnectAt<test_placeholders::Echo>(*svc_dir);
    ASSERT_TRUE(echo_from_svc.is_ok()) << echo_from_svc.status_string();

    // Verify LocalCppComponent::Connect<Protocol>().
    zx::result<fidl::ClientEnd<test_placeholders::Echo>> echo_end =
        Connect<test_placeholders::Echo>();
    ASSERT_TRUE(echo_end.is_ok()) << echo_end.status_string();
    connect_client_.Bind(std::move(*echo_end), dispatcher_);
    svc_client_.Bind(std::move(*echo_from_svc), dispatcher_);
    connect_client_->EchoString({{.value = "natural-hello"}})
        .ThenExactlyOnce([this](fidl::Result<test_placeholders::Echo::EchoString>& result) {
          ASSERT_TRUE(result.is_ok()) << result.error_value().FormatDescription();
          ASSERT_EQ(result->response(), "natural-hello");

          svc_client_->EchoString({{.value = "svc-hello"}})
              .ThenExactlyOnce([this](fidl::Result<test_placeholders::Echo::EchoString>& second) {
                ASSERT_TRUE(second.is_ok()) << second.error_value().FormatDescription();
                ASSERT_EQ(second->response(), "svc-hello");
                if (on_success_) {
                  on_success_();
                }
                Exit();
              });
        });
  }

 private:
  async_dispatcher_t* dispatcher_;
  fidl::Client<test_placeholders::Echo> connect_client_;
  fidl::Client<test_placeholders::Echo> svc_client_;
  fit::closure on_success_;
};

TEST_F(RealmBuilderTest, RoutesProtocolBetweenLocalCppComponents) {
  static constexpr char kEchoServer[] = "echo_server";
  static constexpr char kEchoClient[] = "echo_client";

  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddLocalChild(
      kEchoServer, [this]() { return std::make_unique<LocalEchoServerCpp>(dispatcher()); });
  bool client_succeeded = false;
  realm_builder.AddLocalChild(
      kEchoClient,
      [this, &client_succeeded]() {
        return std::make_unique<LocalEchoClientCpp>(
            dispatcher(), [&client_succeeded]() { client_succeeded = true; });
      },
      ChildOptions{.startup_mode = StartupMode::EAGER});
  realm_builder.AddRoute(
      Route{.capabilities = {Protocol{fidl::DiscoverableProtocolName<test_placeholders::Echo>}},
            .source = ChildRef{kEchoServer},
            .targets = {ChildRef{kEchoClient}, ParentRef()}});

  auto realm = realm_builder.Build(dispatcher());
  auto cleanup = fit::defer([&]() {
    bool complete = false;
    realm.Teardown([&](fit::result<fuchsia_component::Error> result) {
      EXPECT_TRUE(result.is_ok());
      complete = true;
    });
    RunLoopUntil([&]() { return complete; });
  });

  RunLoopUntil([&]() { return client_succeeded; });
}

TEST_F(RealmBuilderTest, LocalCppComponentSvcReturnsErrorWhenUnrouted) {
  class UnroutedClientCpp : public LocalCppComponent {
   public:
    explicit UnroutedClientCpp(fit::function<void(zx_status_t, zx_status_t)> on_done)
        : on_done_(std::move(on_done)) {}

    void OnStart() override {
      zx::result svc_dir = svc();
      zx::result echo_end = Connect<test_placeholders::Echo>();
      on_done_(svc_dir.status_value(), echo_end.status_value());
      Exit();
    }

   private:
    fit::function<void(zx_status_t, zx_status_t)> on_done_;
  };

  std::optional<std::pair<zx_status_t, zx_status_t>> statuses;
  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddLocalChild(
      "unrouted",
      [&statuses]() {
        return std::make_unique<UnroutedClientCpp>(
            [&statuses](zx_status_t svc_status, zx_status_t connect_status) {
              statuses = std::make_pair(svc_status, connect_status);
            });
      },
      ChildOptions{.startup_mode = StartupMode::EAGER});
  auto realm = realm_builder.Build(dispatcher());
  auto cleanup = fit::defer([&]() {
    bool complete = false;
    realm.Teardown([&](auto result) {
      static_assert(std::is_same_v<decltype(result), fit::result<fuchsia::component::Error>>);
      EXPECT_TRUE(result.is_ok());
      complete = true;
    });
    RunLoopUntil([&]() { return complete; });
  });

  RunLoopUntil([&]() { return statuses.has_value(); });
  EXPECT_EQ(statuses->first, ZX_ERR_NOT_FOUND);
  EXPECT_EQ(statuses->second, ZX_ERR_NOT_FOUND);
}

TEST_F(RealmBuilderTest, UsesProvidedNaturalSvcDirectory) {
  static constexpr char kEchoServer[] = "echo_server";
  static constexpr char kPrePopulatedRealmUrl[] = "#meta/pre_populated_realm.cm";

  {
    zx::result svc = component::OpenServiceRoot();
    ASSERT_TRUE(svc.is_ok()) << svc.status_string();
    auto realm_builder = RealmBuilder::Create(std::move(*svc));
    realm_builder.AddChild(kEchoServer, kEchoServerUrl);
    realm_builder.AddRoute(
        Route{.capabilities = {Protocol{fidl::DiscoverableProtocolName<test_placeholders::Echo>}},
              .source = ChildRef{kEchoServer},
              .targets = {ParentRef()}});
    auto realm = realm_builder.Build(dispatcher());
    zx::result echo_end = realm.component().Connect<test_placeholders::Echo>();
    ASSERT_TRUE(echo_end.is_ok()) << echo_end.status_string();
    fidl::SyncClient<test_placeholders::Echo> echo(std::move(*echo_end));
    auto response = echo->EchoString({{.value = "hello"}});
    ASSERT_TRUE(response.is_ok()) << response.error_value().FormatDescription();
    EXPECT_EQ(response->response(), "hello");
  }

  {
    zx::result svc = component::OpenServiceRoot();
    ASSERT_TRUE(svc.is_ok()) << svc.status_string();
    auto realm_builder =
        RealmBuilder::CreateFromRelativeUrl(kPrePopulatedRealmUrl, std::move(*svc));
    auto realm = realm_builder.Build(dispatcher());
    zx::result echo_end = realm.component().Connect<test_placeholders::Echo>();
    ASSERT_TRUE(echo_end.is_ok()) << echo_end.status_string();
    fidl::SyncClient<test_placeholders::Echo> echo(std::move(*echo_end));
    auto response = echo->EchoString({{.value = "from-relative"}});
    ASSERT_TRUE(response.is_ok()) << response.error_value().FormatDescription();
    EXPECT_EQ(response->response(), "from-relative");
  }
}

TEST_F(RealmBuilderTest, ScopedChildNaturalApis) {
  // 1. ScopedChild::New with ClientEnd<fuchsia_io::Directory> (explicit name + random name).
  {
    zx::result svc = component::OpenServiceRoot();
    ASSERT_TRUE(svc.is_ok()) << svc.status_string();
    auto child =
        ScopedChild::New(kDefaultCollection, "natural_named_echo", kEchoServerUrl, std::move(*svc));
    EXPECT_EQ(child.GetChildName(), "natural_named_echo");

    // Verify exposed_dir().
    zx::result from_exposed = component::ConnectAt<test_placeholders::Echo>(child.exposed_dir());
    ASSERT_TRUE(from_exposed.is_ok()) << from_exposed.status_string();
    fidl::SyncClient<test_placeholders::Echo> echo1(std::move(*from_exposed));
    auto res1 = echo1->EchoString({{.value = "via-exposed-dir"}});
    ASSERT_TRUE(res1.is_ok()) << res1.error_value().FormatDescription();
    EXPECT_EQ(res1->response(), "via-exposed-dir");

    // Verify CloneExposedDir(ServerEnd<fuchsia_io::Directory>).
    auto [cloned_client, cloned_server] = fidl::Endpoints<fuchsia_io::Directory>::Create();
    child.CloneExposedDir(std::move(cloned_server));
    zx::result from_cloned = component::ConnectAt<test_placeholders::Echo>(cloned_client);
    ASSERT_TRUE(from_cloned.is_ok()) << from_cloned.status_string();
    fidl::SyncClient<test_placeholders::Echo> echo2(std::move(*from_cloned));
    auto res2 = echo2->EchoString({{.value = "via-cloned-dir"}});
    ASSERT_TRUE(res2.is_ok()) << res2.error_value().FormatDescription();
    EXPECT_EQ(res2->response(), "via-cloned-dir");

    // Verify Connect<Protocol>(ServerEnd<Protocol>).
    auto [echo_client, echo_server] = fidl::Endpoints<test_placeholders::Echo>::Create();
    ASSERT_EQ(child.Connect<test_placeholders::Echo>(std::move(echo_server)), ZX_OK);
    fidl::SyncClient<test_placeholders::Echo> echo3(std::move(echo_client));
    auto res3 = echo3->EchoString({{.value = "via-server-end"}});
    ASSERT_TRUE(res3.is_ok()) << res3.error_value().FormatDescription();
    EXPECT_EQ(res3->response(), "via-server-end");

    // Verify natural ScopedChild::Teardown.
    bool torn_down = false;
    child.Teardown(dispatcher(), [&](fit::result<fuchsia_component::Error> result) {
      EXPECT_TRUE(result.is_ok());
      torn_down = true;
    });
    RunLoopUntil([&]() { return torn_down; });
  }

  // 2. ScopedChild::New with ClientEnd<fuchsia_io::Directory> (random name) + generic auto
  // Teardown.
  {
    zx::result svc = component::OpenServiceRoot();
    ASSERT_TRUE(svc.is_ok()) << svc.status_string();
    auto child = ScopedChild::New(kDefaultCollection, kEchoServerUrl, std::move(*svc));
    EXPECT_FALSE(child.GetChildName().empty());

    bool torn_down = false;
    child.Teardown(dispatcher(), [&](auto result) {
      static_assert(std::is_same_v<decltype(result), fit::result<fuchsia::component::Error>>);
      EXPECT_TRUE(result.is_ok());
      torn_down = true;
    });
    RunLoopUntil([&]() { return torn_down; });
  }

  // 3. ScopedChild::New with ClientEnd<fuchsia_component::Realm> (explicit name + random name).
  {
    zx::result realm_client = component::Connect<fuchsia_component::Realm>();
    ASSERT_TRUE(realm_client.is_ok()) << realm_client.status_string();
    auto named_child = ScopedChild::New(std::move(*realm_client), kDefaultCollection,
                                        "natural_realm_named_echo", kEchoServerUrl);
    EXPECT_EQ(named_child.GetChildName(), "natural_realm_named_echo");
    zx::result echo_end = named_child.Connect<test_placeholders::Echo>();
    ASSERT_TRUE(echo_end.is_ok()) << echo_end.status_string();
    fidl::SyncClient<test_placeholders::Echo> echo(std::move(*echo_end));
    auto res = echo->EchoString({{.value = "from-realm-client"}});
    ASSERT_TRUE(res.is_ok()) << res.error_value().FormatDescription();
    EXPECT_EQ(res->response(), "from-realm-client");
  }

  {
    zx::result realm_client = component::Connect<fuchsia_component::Realm>();
    ASSERT_TRUE(realm_client.is_ok()) << realm_client.status_string();
    auto auto_child =
        ScopedChild::New(std::move(*realm_client), kDefaultCollection, kEchoServerUrl);
    EXPECT_FALSE(auto_child.GetChildName().empty());
  }
}

TEST_F(RealmBuilderTest, NaturalDictionaryCapabilitiesCollectionsAndEnvironments) {
  static constexpr char kEchoServer[] = "echo_server";

  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddChild(kEchoServer, kEchoServerFragmentOnlyUrl);
  realm_builder.AddDictionary("root_dict");
  realm_builder.AddCapability(
      fuchsia_component_decl::Capability::WithProtocol(fuchsia_component_decl::Protocol{{
          .name = "custom.RootProtocol",
          .source_path = "/svc/custom.RootProtocol",
      }}));
  realm_builder.AddCollection(fuchsia_component_decl::Collection{{
      .name = "custom_col",
      .durability = fuchsia_component_decl::Durability::kTransient,
  }});
  realm_builder.AddEnvironment(fuchsia_component_decl::Environment{{
      .name = "custom_env",
      .extends = fuchsia_component_decl::EnvironmentExtends::kRealm,
  }});

  auto sub_realm = realm_builder.AddChildRealm("sub");
  sub_realm.AddDictionary("sub_dict");
  sub_realm.AddCapability(
      fuchsia_component_decl::Capability::WithProtocol(fuchsia_component_decl::Protocol{{
          .name = "custom.SubProtocol",
          .source_path = "/svc/custom.SubProtocol",
      }}));
  sub_realm.AddCollection(fuchsia_component_decl::Collection{{
      .name = "sub_col",
      .durability = fuchsia_component_decl::Durability::kTransient,
  }});
  sub_realm.AddEnvironment(fuchsia_component_decl::Environment{{
      .name = "sub_env",
      .extends = fuchsia_component_decl::EnvironmentExtends::kNone,
      .stop_timeout_ms = 1000,
  }});

  auto root_decl = realm_builder.GetRealmDecl();
  EXPECT_EQ(root_decl.capabilities().size(), 2u);
  EXPECT_EQ(root_decl.collections().size(), 1u);
  EXPECT_EQ(root_decl.collections()[0].name(), "custom_col");
  EXPECT_EQ(root_decl.environments().size(), 1u);
  EXPECT_EQ(root_decl.environments()[0].name(), "custom_env");

  auto sub_decl = sub_realm.GetRealmDecl();
  EXPECT_EQ(sub_decl.capabilities().size(), 2u);
  EXPECT_EQ(sub_decl.collections().size(), 1u);
  EXPECT_EQ(sub_decl.collections()[0].name(), "sub_col");
  EXPECT_EQ(sub_decl.environments().size(), 1u);
  EXPECT_EQ(sub_decl.environments()[0].name(), "sub_env");

  realm_builder.AddRoute(
      Route{.capabilities = {Protocol{fidl::DiscoverableProtocolName<test_placeholders::Echo>}},
            .source = ChildRef{kEchoServer},
            .targets = {DictionaryRef{.base_ref = SelfRef(), .path = "root_dict"}}});
  realm_builder.AddRoute(
      Route{.capabilities = {Protocol{fidl::DiscoverableProtocolName<test_placeholders::Echo>}},
            .source = DictionaryRef{.base_ref = SelfRef(), .path = "root_dict"},
            .targets = {ParentRef()}});

  auto realm = realm_builder.Build(dispatcher());
  zx::result echo_end = realm.component().Connect<test_placeholders::Echo>();
  ASSERT_TRUE(echo_end.is_ok()) << echo_end.status_string();
  fidl::SyncClient<test_placeholders::Echo> echo(std::move(*echo_end));
  auto response = echo->EchoString({{.value = "dict-hello"}});
  ASSERT_TRUE(response.is_ok()) << response.error_value().FormatDescription();
  EXPECT_EQ(response->response(), "dict-hello");
}

TEST_F(RealmBuilderTest, NaturalAddChildRealmFromDeclAndReplaceDecls) {
  static constexpr char kEchoServer[] = "echo_server";
  static constexpr char kSubRealm[] = "sub_realm";
  static constexpr char kNestedRealm[] = "nested_realm";

  auto make_expose_from_child = [](std::string_view child_name) {
    return std::vector<fuchsia_component_decl::Expose>{
        fuchsia_component_decl::Expose::WithProtocol(fuchsia_component_decl::ExposeProtocol{{
            .source = fuchsia_component_decl::Ref::WithChild(
                fuchsia_component_decl::ChildRef{{.name = std::string(child_name)}}),
            .source_name = std::string(fidl::DiscoverableProtocolName<test_placeholders::Echo>),
            .target = fuchsia_component_decl::Ref::WithParent({}),
            .target_name = std::string(fidl::DiscoverableProtocolName<test_placeholders::Echo>),
        }}),
    };
  };

  fuchsia_component_decl::Component nested_decl{{
      .children =
          std::vector<fuchsia_component_decl::Child>{
              fuchsia_component_decl::Child{{
                  .name = kEchoServer,
                  .url = kEchoServerUrl,
                  .startup = fuchsia_component_decl::StartupMode::kLazy,
              }},
          },
  }};

  auto realm_builder = RealmBuilder::Create();
  auto sub_realm =
      realm_builder.AddChildRealmFromDecl(kSubRealm, fuchsia_component_decl::Component{});
  auto nested_realm = sub_realm.AddChildRealmFromDecl(kNestedRealm, std::move(nested_decl));

  // Mutate nested_realm's decl via Realm::ReplaceRealmDecl, then also via
  // Realm::ReplaceComponentDecl on sub_realm.
  fuchsia_component_decl::Component updated_nested_decl{{
      .exposes = make_expose_from_child(kEchoServer),
      .children =
          std::vector<fuchsia_component_decl::Child>{
              fuchsia_component_decl::Child{{
                  .name = kEchoServer,
                  .url = kEchoServerUrl,
                  .startup = fuchsia_component_decl::StartupMode::kLazy,
              }},
          },
  }};
  nested_realm.ReplaceRealmDecl(updated_nested_decl);
  sub_realm.ReplaceComponentDecl(kNestedRealm, std::move(updated_nested_decl));

  // Mutate kSubRealm's decl via RealmBuilder::ReplaceComponentDecl (realm_builder_server merges
  // child realms added via AddChildRealm* into `children` during Build()).
  realm_builder.ReplaceComponentDecl(
      kSubRealm,
      fuchsia_component_decl::Component{{.exposes = make_expose_from_child(kNestedRealm)}});

  // Mutate root realm's decl via RealmBuilder::ReplaceRealmDecl.
  realm_builder.ReplaceRealmDecl(
      fuchsia_component_decl::Component{{.exposes = make_expose_from_child(kSubRealm)}});

  auto realm = realm_builder.Build(dispatcher());
  zx::result echo_end = realm.component().Connect<test_placeholders::Echo>();
  ASSERT_TRUE(echo_end.is_ok()) << echo_end.status_string();
  fidl::SyncClient<test_placeholders::Echo> echo(std::move(*echo_end));
  auto response = echo->EchoString({{.value = "decl-hello"}});
  ASSERT_TRUE(response.is_ok()) << response.error_value().FormatDescription();
  EXPECT_EQ(response->response(), "decl-hello");
}

TEST_F(RealmBuilderTest, PanicsWhenBuildCalledMultipleTimes) {
  ASSERT_DEATH(
      {
        auto realm_builder = RealmBuilder::Create();
        realm_builder.Build(dispatcher());
        realm_builder.Build(dispatcher());
      },
      "");
}

TEST(RealmBuilderUnittest, PanicsIfChildNameIsEmpty) {
  ASSERT_DEATH(
      {
        auto realm_builder = RealmBuilder::Create();
        realm_builder.AddChild("", kEchoServerUrl);
      },
      "");

  class BasicLocalImpl : public LocalComponentImpl {
    void OnStart() override {}
  };
  ASSERT_DEATH(
      {
        auto realm_builder = RealmBuilder::Create();
        realm_builder.AddLocalChild(
            "", []() -> std::unique_ptr<LocalComponentImpl> { return nullptr; });
      },
      "");
}

TEST(RealmBuilderUnittest, PanicsIfUrlIsEmpty) {
  ASSERT_DEATH(
      {
        auto realm_builder = RealmBuilder::Create();
        realm_builder.AddChild("some_valid_name", "");
      },
      "");
}

TEST(RealmBuilderUnittest, PanicsWhenArgsAreNullptr) {
  ASSERT_DEATH(
      {
        auto realm_builder = RealmBuilder::Create();
        // Should panic because |async_get_default_dispatcher| was not configured
        // to return nullptr.
        realm_builder.Build(nullptr);
      },
      "");
}

TEST(DirectoryContentsUnittest, PanicWhenGivenInvalidPath) {
  ASSERT_DEATH(
      {
        auto directory_contents = DirectoryContents();
        directory_contents.AddFile("/foo/bar.txt", "Hello World!");
      },
      "");

  ASSERT_DEATH(
      {
        auto directory_contents = DirectoryContents();
        directory_contents.AddFile("foo/bar/", "Hello World!");
      },
      "");

  ASSERT_DEATH(
      {
        auto directory_contents = DirectoryContents();
        directory_contents.AddFile("", "Hello World!");
      },
      "");
}

class PlaceholderComponent : public LocalComponentImpl {
 public:
  PlaceholderComponent() = default;

  void OnStart() override {}
};

constexpr char kRoutingTestChildName[] = "foobar";

class RealmBuilderRoutingParameterizedFixture
    : public testing::TestWithParam<std::pair<Capability, std::shared_ptr<fdecl::Offer>>> {};

TEST_P(RealmBuilderRoutingParameterizedFixture, RoutedCapabilitiesYieldExpectedOfferClauses) {
  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddLocalChild(kRoutingTestChildName,
                              []() { return std::make_unique<PlaceholderComponent>(); });

  auto param = GetParam();
  auto capability = param.first;
  realm_builder.AddRoute(Route{.capabilities = {capability},
                               .source = ParentRef{},
                               .targets = {ChildRef{kRoutingTestChildName}}});

  auto root_decl = realm_builder.GetRealmDecl();

  ASSERT_EQ(root_decl.offers().size(), 1ul);

  const fdecl::Offer& actual = root_decl.offers().at(0);
  const fdecl::Offer& expected = *param.second;

  EXPECT_TRUE(fidl::Equals(actual, expected));
}

INSTANTIATE_TEST_SUITE_P(
    RealmBuilderRoutingTest, RealmBuilderRoutingParameterizedFixture,
    testing::Values(
        std::make_pair(Protocol{.name = "foo", .as = "bar"},
                       component::tests::CreateFidlProtocolOfferDecl(
                           /*source_name=*/"foo",
                           /*source=*/component::tests::CreateFidlParentRef(),
                           /*target_name=*/"bar",
                           /*target=*/component::tests::CreateFidlChildRef(kRoutingTestChildName))),
        std::make_pair(Service{.name = "foo", .as = "bar"},
                       component::tests::CreateFidlServiceOfferDecl(
                           /*source_name=*/"foo",
                           /*source=*/component::tests::CreateFidlParentRef(),
                           /*target_name=*/"bar",
                           /*target=*/component::tests::CreateFidlChildRef(kRoutingTestChildName))),
        std::make_pair(Directory{.name = "foo",
                                 .as = "bar",
                                 .subdir = "sub",
                                 .rights = fuchsia::io::RW_STAR_DIR,
                                 .path = "/foo"},
                       component::tests::CreateFidlDirectoryOfferDecl(
                           /*source_name=*/"foo",
                           /*source=*/component::tests::CreateFidlParentRef(),
                           /*target_name=*/"bar",
                           /*target=*/component::tests::CreateFidlChildRef(kRoutingTestChildName),
                           /*subdir=*/"sub",
                           /*rights=*/fuchsia::io::RW_STAR_DIR)),
        std::make_pair(
            Storage{.name = "foo", .as = "bar", .path = "/foo"},
            component::tests::CreateFidlStorageOfferDecl(
                /*source_name=*/"foo", /*source=*/component::tests::CreateFidlParentRef(),
                /*target_name=*/"bar",
                /*target=*/component::tests::CreateFidlChildRef(kRoutingTestChildName)))));
}  // namespace
