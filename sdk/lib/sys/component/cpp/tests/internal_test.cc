// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.component.decl/cpp/fidl.h>
#include <fidl/fuchsia.component.runner/cpp/fidl.h>
#include <fidl/fuchsia.component.test/cpp/fidl.h>
#include <fidl/fuchsia.component/cpp/fidl.h>
#include <fidl/fuchsia.data/cpp/fidl.h>
#include <fidl/fuchsia.io/cpp/fidl.h>
#include <fuchsia/component/decl/cpp/fidl.h>
#include <fuchsia/io/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/loop.h>
#include <lib/async/dispatcher.h>
#include <lib/fdio/namespace.h>
#include <lib/fidl/cpp/binding.h>
#include <lib/fidl/cpp/client.h>
#include <lib/fidl/cpp/interface_request.h>
#include <lib/fidl/cpp/string.h>
#include <lib/fit/function.h>
#include <lib/sys/component/cpp/testing/internal/convert.h>
#include <lib/sys/component/cpp/testing/internal/local_component_runner.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <lib/sys/component/cpp/tests/utils.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/channel.h>
#include <zircon/status.h>

#include <cstddef>
#include <memory>
#include <utility>
#include <vector>

#include <gtest/gtest.h>
#include <src/lib/testing/loop_fixture/real_loop_fixture.h>
#include <test/placeholders/cpp/fidl.h>

namespace {

// NOLINTNEXTLINE
using namespace component_testing;
// NOLINTNEXTLINE
using namespace component_testing::internal;

namespace fcdecl = fuchsia::component::decl;
namespace fio = fuchsia::io;

/*
 * Tests for |component_testing::internal::LocalComponentRunner| and
 * |component_testing::internal::LocalComponentRunner::Builder|.
 */

class TestComponent : public LocalComponentImpl {
 public:
  explicit TestComponent(fit::closure quit_loop) : quit_loop_(std::move(quit_loop)) {}

  void OnStart() override {
    called_ = true;
    quit_loop_();
  }

  bool WasCalled() const { return called_; }

 private:
  fit::closure quit_loop_;
  bool called_ = false;
};

class LocalComponentRunnerTest : public gtest::RealLoopFixture {
 public:
  void SetUp() override {
    auto builder = LocalComponentRunner::Builder();
    builder.Register(kTestComponentName, [this]() {
      auto test_component = std::make_unique<TestComponent>(QuitLoopClosure());
      FX_CHECK(test_component_ == nullptr);
      test_component_ = test_component.get();
      return test_component;
    });
    local_component_runner_ = builder.Build(dispatcher());
    runner_proxy_.Bind(local_component_runner_->NewBinding(), dispatcher());
  }

  void TearDown() override {
    local_component_runner_.reset();
    default_component_controller_ = {};
    runner_proxy_ = {};
  }

 protected:
  static constexpr char kTestComponentName[] = "test_component";

  void CallStart(fuchsia_component_runner::ComponentStartInfo start_info,
                 fidl::ServerEnd<fuchsia_component_runner::ComponentController> controller) {
    ASSERT_TRUE(
        runner_proxy_
            ->Start({{.start_info = std::move(start_info), .controller = std::move(controller)}})
            .is_ok());
  }

  void CallStart(fuchsia_component_runner::ComponentStartInfo start_info) {
    auto [client, server] =
        fidl::Endpoints<fuchsia_component_runner::ComponentController>::Create();
    default_component_controller_.Bind(std::move(client), dispatcher());
    CallStart(std::move(start_info), std::move(server));
  }

  TestComponent* GetTestComponent() { return test_component_; }

  static fuchsia_component_runner::ComponentStartInfo CreateValidStartInfo() {
    return fuchsia_component_runner::ComponentStartInfo{{
        .program = CreateValidProgram(),
    }};
  }

  static fuchsia_data::Dictionary CreateValidProgram() {
    fuchsia_data::DictionaryEntry entry{{
        .key = fuchsia_component_test::kLocalComponentNameKey,
        .value = std::make_unique<fuchsia_data::DictionaryValue>(
            fuchsia_data::DictionaryValue::WithStr(kTestComponentName)),
    }};
    return fuchsia_data::Dictionary{{
        .entries = std::vector<fuchsia_data::DictionaryEntry>{std::move(entry)},
    }};
  }

 private:
  std::unique_ptr<LocalComponentRunner> local_component_runner_ = nullptr;
  TestComponent* test_component_ = nullptr;
  fidl::Client<fuchsia_component_runner::ComponentController> default_component_controller_;
  fidl::Client<fuchsia_component_runner::ComponentRunner> runner_proxy_;
};

TEST_F(LocalComponentRunnerTest, RunnerStartsOnStartRequest) {
  auto start_info = CreateValidStartInfo();
  auto [outgoing_client, outgoing_server] = fidl::Endpoints<fuchsia_io::Directory>::Create();
  start_info.outgoing_dir(std::move(outgoing_server));
  CallStart(std::move(start_info));

  RunLoop();

  EXPECT_TRUE(GetTestComponent()->WasCalled());
}

class EchoImpl : public test::placeholders::Echo {
 public:
  void EchoString(::fidl::StringPtr value, EchoStringCallback callback) override {
    callback(std::move(value));
  }
};

TEST_F(LocalComponentRunnerTest, RunnerGivesComponentItsOutgoingDir) {
  auto start_info = CreateValidStartInfo();
  auto [outgoing_client, outgoing_server] = fidl::Endpoints<fuchsia_io::Directory>::Create();
  start_info.outgoing_dir(std::move(outgoing_server));
  EchoImpl echo_impl;
  test::placeholders::EchoPtr echo_proxy;
  fidl::Binding<test::placeholders::Echo> echo_binding(&echo_impl);

  CallStart(std::move(start_info));
  RunLoop();

  ASSERT_EQ(GetTestComponent()->outgoing()->AddPublicService<test::placeholders::Echo>(
                [&](fidl::InterfaceRequest<test::placeholders::Echo> request) {
                  echo_binding.Bind(std::move(request));
                }),
            ZX_OK);
  echo_proxy.Bind(echo_binding.NewBinding(dispatcher()));

  bool echoed = false;
  echo_proxy->EchoString("hello", [&](fidl::StringPtr _) { echoed = true; });
  RunLoopUntil([&]() { return echoed; });
  EXPECT_TRUE(echoed);
}

TEST_F(LocalComponentRunnerTest, RunnerGivesComponentItsNamespace) {
  static constexpr char kNamespacePath[] = "/test/path";

  auto start_info = CreateValidStartInfo();
  auto [outgoing_client, outgoing_server] = fidl::Endpoints<fuchsia_io::Directory>::Create();
  start_info.outgoing_dir(std::move(outgoing_server));
  auto [ns_client, ns_server] = fidl::Endpoints<fuchsia_io::Directory>::Create();
  // We'll provide a valid channel and verify that it's present down below.
  fuchsia_component_runner::ComponentNamespaceEntry ns_entry{{
      .path = kNamespacePath,
      .directory = std::move(ns_client),
  }};
  std::vector<fuchsia_component_runner::ComponentNamespaceEntry> ns_entries;
  ns_entries.push_back(std::move(ns_entry));
  start_info.ns(std::move(ns_entries));

  CallStart(std::move(start_info));
  RunLoop();

  EXPECT_TRUE(fdio_ns_is_bound(GetTestComponent()->ns(), kNamespacePath));
}

/*
 * Tests for |component_testing::internal::Convert*| functions.
 */

template <typename InputType, typename OutputType>
class ConvertParameterizedFixture
    : public testing::TestWithParam<std::pair<InputType, std::shared_ptr<OutputType>>> {
 protected:
  InputType GetInput(const InputType& input) { return input; }
};

#define ZX_COMPONENT_TYPED_TEST_P(test_suite_name, method_name) \
  TEST_P(test_suite_name, method_name) {                        \
    auto param = GetParam();                                    \
    auto actual = ConvertToFidl(GetInput(param.first));         \
    auto expected = std::move(*param.second);                   \
    EXPECT_EQ(actual, expected);                                \
  }

// The parameter type must be a copyable type per
// http://google.github.io/googletest/advanced.html#how-to-write-value-parameterized-tests.
// Since at least one member of `ChildOptions` is not copyable, we need to work around this.
class ConvertChildOptionsParameterizedFixture
    : public ConvertParameterizedFixture<std::shared_ptr<ChildOptions>,
                                         fuchsia_component_test::ChildOptions> {
 protected:
  const ChildOptions& GetInput(std::shared_ptr<ChildOptions> input) { return *input; }
};

std::vector<fcdecl::ConfigOverride> GetTestConfigOverrides() {
  std::vector<fuchsia::component::decl::ConfigOverride> result;
  result.emplace_back();
  result.back().set_key("baz");
  result.back().set_value(
      fcdecl::ConfigValue::WithSingle(::fcdecl::ConfigSingleValue::WithUint64(1234u)));
  result.emplace_back();
  result.back().set_key("bat");
  result.back().set_value(
      fcdecl::ConfigValue::WithSingle(::fcdecl::ConfigSingleValue::WithInt8(-100)));

  // Add the same key with a different value. There are no checks for duplicates.
  result.emplace_back();
  result.back().set_key(result[0].key());
  result.back().set_value(
      fcdecl::ConfigValue::WithSingle(::fcdecl::ConfigSingleValue::WithString("hello")));

  return result;
}

std::vector<std::pair<std::string, fuchsia_component_decl::ConfigValue>>
GetExpectedConfigOverrides() {
  std::vector<std::pair<std::string, fuchsia_component_decl::ConfigValue>> result;
  result.emplace_back(std::string("baz"),
                      fuchsia_component_decl::ConfigValue::WithSingle(
                          fuchsia_component_decl::ConfigSingleValue::WithUint64(1234u)));
  result.emplace_back(std::string("bat"),
                      fuchsia_component_decl::ConfigValue::WithSingle(
                          fuchsia_component_decl::ConfigSingleValue::WithInt8(-100)));
  result.emplace_back(std::string("baz"),
                      fuchsia_component_decl::ConfigValue::WithSingle(
                          fuchsia_component_decl::ConfigSingleValue::WithString("hello")));
  return result;
}

ZX_COMPONENT_TYPED_TEST_P(ConvertChildOptionsParameterizedFixture, ConvertsAllFields)
INSTANTIATE_TEST_SUITE_P(
    ConvertTest, ConvertChildOptionsParameterizedFixture,
    testing::Values(std::make_pair(std::shared_ptr<ChildOptions>(new ChildOptions{
                                       .startup_mode = StartupMode::EAGER,
                                       .environment = "foo",
                                       .config_overrides = GetTestConfigOverrides()}),
                                   component::tests::CreateFidlChildOptions(
                                       fuchsia_component_decl::StartupMode::kEager, "foo",
                                       GetExpectedConfigOverrides())),
                    std::make_pair(std::shared_ptr<ChildOptions>(new ChildOptions{
                                       .startup_mode = StartupMode::LAZY, .environment = "bar"}),
                                   component::tests::CreateFidlChildOptions(
                                       fuchsia_component_decl::StartupMode::kLazy, "bar", {}))));

class ConvertRefParameterizedFixture
    : public ConvertParameterizedFixture<std::pair<Ref, RefContext>, RefPathPair> {};

#define ZX_COMPONENT_REF_TEST_P(test_suite_name, method_name)  \
  TEST_P(test_suite_name, method_name) {                       \
    auto param = GetParam();                                   \
    const auto& input = param.first;                           \
    auto actual = ConvertRefToFidl(input.first, input.second); \
    auto expected = std::move(*param.second);                  \
    EXPECT_EQ(actual.first, expected.first);                   \
    EXPECT_EQ(actual.second, expected.second);                 \
  }

ZX_COMPONENT_REF_TEST_P(ConvertRefParameterizedFixture, ConvertsAllFields)
INSTANTIATE_TEST_SUITE_P(
    ConvertTest, ConvertRefParameterizedFixture,
    testing::Values(
        std::make_pair(std::make_pair(Ref{ParentRef{}}, RefContext::SOURCE),
                       std::make_shared<RefPathPair>(fuchsia_component_decl::Ref::WithParent(
                                                         fuchsia_component_decl::ParentRef{}),
                                                     ".")),
        std::make_pair(std::make_pair(Ref{ChildRef{.name = "foo"}}, RefContext::SOURCE),
                       std::make_shared<RefPathPair>(
                           fuchsia_component_decl::Ref::WithChild(fuchsia_component_decl::ChildRef{
                               {.name = "foo"}}),
                           "."))));

class ConvertCapabilityParameterizedFixture
    : public ConvertParameterizedFixture<Capability, fuchsia_component_test::Capability> {};

ZX_COMPONENT_TYPED_TEST_P(ConvertCapabilityParameterizedFixture, ConvertsAllField)
INSTANTIATE_TEST_SUITE_P(
    ConvertTest, ConvertCapabilityParameterizedFixture,
    testing::Values(std::make_pair(Protocol{"foo"},
                                   component::tests::CreateFidlProtocolCapability(/*name=*/"foo")),
                    std::make_pair(
                        Protocol{
                            .name = "foo",
                            .as = "bar",
                            .type = fcdecl::DependencyType::WEAK,
                            .path = "/foo/bar/baz",
                        },
                        component::tests::CreateFidlProtocolCapability(
                            /*name=*/"foo", /*as=*/"bar",
                            /*type=*/fuchsia_component_decl::DependencyType::kWeak,
                            /*path=*/"/foo/bar/baz")),
                    std::make_pair(Service{"foo"},
                                   component::tests::CreateFidlServiceCapability(/*name=*/"foo")),
                    std::make_pair(
                        Service{
                            .name = "foo",
                            .as = "bar",
                            .path = "/foo/bar/baz",
                        },
                        component::tests::CreateFidlServiceCapability(
                            /*name=*/"foo", /*as=*/"bar",
                            /*path=*/"/foo/bar/baz")),
                    std::make_pair(Directory{"foo"},
                                   component::tests::CreateFidlDirectoryCapability(/*name=*/"foo")),
                    std::make_pair(
                        Directory{
                            .name = "foo",
                            .as = "bar",
                            .type = fcdecl::DependencyType::WEAK,
                            .subdir = "baz",
                            .rights = fio::Operations::EXECUTE,
                            .path = "/foo/bar/baz",
                        },
                        component::tests::CreateFidlDirectoryCapability(
                            /*name=*/"foo", /*as=*/"bar",
                            /*type=*/fuchsia_component_decl::DependencyType::kWeak,
                            /*subdir=*/"baz",
                            /*rights=*/fuchsia_io::Operations::kExecute,
                            /*path=*/"/foo/bar/baz"))));

class ConvertTest : public testing::Test {};

TEST_F(ConvertTest, ToFidlVec) {
  std::vector<Capability> input{Protocol{.name = "foo"}, Protocol{.name = "bar"}};
  auto actual_values = ConvertToFidlVec<Capability, fuchsia_component_test::Capability>(input);
  std::vector<std::shared_ptr<fuchsia_component_test::Capability>> expected_values{
      component::tests::CreateFidlProtocolCapability(/*name=*/"foo"),
      component::tests::CreateFidlProtocolCapability(/*name=*/"bar")};
  ZX_ASSERT(actual_values.size() == expected_values.size());
  for (size_t i = 0; i < expected_values.size(); ++i) {
    const auto& actual = actual_values[i];
    const auto& expected = *expected_values[i];
    EXPECT_EQ(actual, expected);
  }
}

TEST_F(ConvertTest, RefToFidlVec) {
  std::vector<Ref> input{ChildRef{"foo"}, ChildRef{"bar"}};
  std::vector<RefPathPair> actual_values = ConvertRefToFidlVec(input, RefContext::SOURCE);
  std::vector<std::shared_ptr<RefPathPair>> expected_values{
      std::make_shared<RefPathPair>(
          fuchsia_component_decl::Ref::WithChild(fuchsia_component_decl::ChildRef{{.name = "foo"}}),
          "."),
      std::make_shared<RefPathPair>(
          fuchsia_component_decl::Ref::WithChild(fuchsia_component_decl::ChildRef{{.name = "bar"}}),
          ".")};

  ZX_ASSERT(actual_values.size() == expected_values.size());
  for (size_t i = 0; i < expected_values.size(); ++i) {
    const auto& actual = actual_values[i];
    const auto& expected = *expected_values[i];
    EXPECT_EQ(actual.first, expected.first);
    EXPECT_EQ(actual.second, expected.second);
  }
}

}  // namespace
