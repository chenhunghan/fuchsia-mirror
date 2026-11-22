// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_manager/dictionary_util.h"

#include <fidl/fuchsia.component.runtime/cpp/test_base.h>
#include <fidl/fuchsia.io/cpp/test_base.h>
#include <lib/zx/eventpair.h>

#include <map>
#include <memory>
#include <optional>
#include <string>
#include <unordered_map>
#include <vector>

#include <gtest/gtest.h>

#include "src/lib/testing/loop_fixture/test_loop_fixture.h"

namespace {

namespace fruntime = fuchsia_component_runtime;
namespace fio = fuchsia_io;

zx_info_handle_basic_t GetBasicInfo(zx_handle_t handle) {
  zx_info_handle_basic_t info;
  ZX_ASSERT(zx_object_get_info(handle, ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr,
                               nullptr) == ZX_OK);
  return info;
}

// A capability known to FakeRegistry.
struct FakeCapability {
  fruntime::CapabilityType type;
  // Entries of a dictionary capability.
  std::map<std::string, std::shared_ptr<FakeCapability>> entries;
};
using FakeCapabilityPtr = std::shared_ptr<FakeCapability>;

FakeCapabilityPtr NewCapability(fruntime::CapabilityType type) {
  return std::make_shared<FakeCapability>(FakeCapability{.type = type});
}

// Models component manager's registry of runtime capabilities. As in component manager, a
// capability is registered under the koid of the handle given to component manager, and the
// holder of that handle's peer refers to the capability. The registry is shared by every
// connection.
class FakeRegistry {
 public:
  void Register(const zx::eventpair& handle, FakeCapabilityPtr capability) {
    capabilities_[GetBasicInfo(handle.get()).koid] = std::move(capability);
  }

  FakeCapabilityPtr Lookup(const zx::eventpair& handle) const {
    auto it = capabilities_.find(GetBasicInfo(handle.get()).related_koid);
    return it == capabilities_.end() ? nullptr : it->second;
  }

  // Registers |capability| and returns a handle that references it.
  zx::eventpair Mint(FakeCapabilityPtr capability) {
    zx::eventpair handle, handle_peer;
    ZX_ASSERT(zx::eventpair::create(0, &handle, &handle_peer) == ZX_OK);
    Register(handle_peer, std::move(capability));
    return handle;
  }

 private:
  std::unordered_map<zx_koid_t, FakeCapabilityPtr> capabilities_;
};

// Serves one fuchsia.component.runtime.Capabilities connection on top of a FakeRegistry, and
// records which methods were called on it.
class FakeCapabilities : public fidl::testing::TestBase<fruntime::Capabilities> {
 public:
  struct DirConnectorOpenCall {
    FakeCapabilityPtr dir_connector;
    std::optional<fio::Flags> flags;
    fidl::ServerEnd<fio::Directory> channel;
  };

  explicit FakeCapabilities(std::shared_ptr<FakeRegistry> registry)
      : registry_(std::move(registry)) {}

  const std::vector<std::string>& calls() const { return calls_; }
  std::vector<DirConnectorOpenCall>& dir_connector_opens() { return dir_connector_opens_; }

  void set_copy_error(fruntime::CapabilitiesError error) { copy_error_ = error; }
  void set_route_result(fit::result<zx_status_t, fruntime::RouterResponse> result) {
    route_result_ = result;
  }

  void DictionaryCreate(DictionaryCreateRequest& request,
                        DictionaryCreateCompleter::Sync& completer) override {
    calls_.emplace_back("DictionaryCreate");
    registry_->Register(request.dictionary(), NewCapability(fruntime::CapabilityType::kDictionary));
    completer.Reply(fit::ok());
  }

  void DictionaryCopy(DictionaryCopyRequest& request,
                      DictionaryCopyCompleter::Sync& completer) override {
    calls_.emplace_back("DictionaryCopy");
    if (copy_error_.has_value()) {
      completer.Reply(fit::error(copy_error_.value()));
      return;
    }
    FakeCapabilityPtr dictionary = registry_->Lookup(request.dictionary());
    if (!dictionary || dictionary->type != fruntime::CapabilityType::kDictionary) {
      completer.Reply(fit::error(fruntime::CapabilitiesError::kInvalidCapabilityType));
      return;
    }
    FakeCapabilityPtr copy = NewCapability(fruntime::CapabilityType::kDictionary);
    copy->entries = dictionary->entries;
    registry_->Register(request.copy(), std::move(copy));
    completer.Reply(fit::ok());
  }

  void DictionaryInsert(DictionaryInsertRequest& request,
                        DictionaryInsertCompleter::Sync& completer) override {
    calls_.emplace_back("DictionaryInsert");
    FakeCapabilityPtr dictionary = registry_->Lookup(request.dictionary());
    FakeCapabilityPtr value = registry_->Lookup(request.value());
    if (!dictionary || !value) {
      completer.Reply(fit::error(fruntime::CapabilitiesError::kHandleDoesNotReferenceCapability));
      return;
    }
    dictionary->entries[request.key()] = std::move(value);
    completer.Reply(fit::ok());
  }

  void DictionaryGet(DictionaryGetRequest& request,
                     DictionaryGetCompleter::Sync& completer) override {
    calls_.emplace_back("DictionaryGet");
    FakeCapabilityPtr dictionary = registry_->Lookup(request.dictionary());
    if (!dictionary) {
      completer.Reply(fit::error(fruntime::CapabilitiesError::kHandleDoesNotReferenceCapability));
      return;
    }
    auto entry = dictionary->entries.find(request.key());
    if (entry == dictionary->entries.end()) {
      completer.Reply(fit::error(fruntime::CapabilitiesError::kNoSuchCapability));
      return;
    }
    registry_->Register(request.value(), entry->second);
    completer.Reply(fit::ok(fruntime::CapabilitiesDictionaryGetResponse{{
        .capability_type = entry->second->type,
    }}));
  }

  void DirConnectorCreate(DirConnectorCreateRequest& request,
                          DirConnectorCreateCompleter::Sync& completer) override {
    calls_.emplace_back("DirConnectorCreate");
    registry_->Register(request.dir_connector(),
                        NewCapability(fruntime::CapabilityType::kDirConnector));
    completer.Reply(fit::ok());
  }

  void DirConnectorOpen(DirConnectorOpenRequest& request,
                        DirConnectorOpenCompleter::Sync& completer) override {
    calls_.emplace_back("DirConnectorOpen");
    if (!request.dir_connector().has_value() || !request.channel().has_value()) {
      completer.Reply(fit::error(fruntime::CapabilitiesError::kInvalidArgs));
      return;
    }
    FakeCapabilityPtr dir_connector = registry_->Lookup(request.dir_connector().value());
    if (!dir_connector || dir_connector->type != fruntime::CapabilityType::kDirConnector) {
      completer.Reply(fit::error(fruntime::CapabilitiesError::kInvalidCapabilityType));
      return;
    }
    dir_connector_opens_.push_back(DirConnectorOpenCall{
        .dir_connector = std::move(dir_connector),
        .flags = request.flags(),
        .channel = std::move(request.channel().value()),
    });
    completer.Reply(fit::ok());
  }

  void InstanceTokenCreate(InstanceTokenCreateRequest& request,
                           InstanceTokenCreateCompleter::Sync& completer) override {
    calls_.emplace_back("InstanceTokenCreate");
    registry_->Register(request.instance_token(),
                        NewCapability(fruntime::CapabilityType::kInstanceToken));
    completer.Reply(fit::ok());
  }

  void DirConnectorRouterRoute(DirConnectorRouterRouteRequest& request,
                               DirConnectorRouterRouteCompleter::Sync& completer) override {
    calls_.emplace_back("DirConnectorRouterRoute");
    FakeCapabilityPtr router = registry_->Lookup(request.router());
    FakeCapabilityPtr instance_token = registry_->Lookup(request.instance_token());
    if (!router || router->type != fruntime::CapabilityType::kDirConnectorRouter ||
        !instance_token || instance_token->type != fruntime::CapabilityType::kInstanceToken) {
      completer.Reply(fit::error(ZX_ERR_INVALID_ARGS));
      return;
    }
    if (route_result_.is_error()) {
      completer.Reply(fit::error(route_result_.error_value()));
      return;
    }
    if (route_result_.value() == fruntime::RouterResponse::kSuccess) {
      registry_->Register(request.dir_connector(),
                          NewCapability(fruntime::CapabilityType::kDirConnector));
    }
    completer.Reply(fit::ok(fruntime::CapabilitiesDirConnectorRouterRouteResponse{{
        .response = route_result_.value(),
    }}));
  }

 private:
  void handle_unknown_method(fidl::UnknownMethodMetadata<fruntime::Capabilities> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    ADD_FAILURE() << "Unexpected call to Capabilities::" << name;
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  std::shared_ptr<FakeRegistry> registry_;
  std::vector<std::string> calls_;
  std::vector<DirConnectorOpenCall> dir_connector_opens_;
  std::optional<fruntime::CapabilitiesError> copy_error_;
  fit::result<zx_status_t, fruntime::RouterResponse> route_result_ =
      fit::ok(fruntime::RouterResponse::kSuccess);
};

class DictionaryUtilTest : public gtest::TestLoopFixture {
 protected:
  void SetUp() override {
    auto [client, server] = fidl::Endpoints<fruntime::Capabilities>::Create();
    main_bindings_.AddBinding(dispatcher(), std::move(server), &main_connection_,
                              fidl::kIgnoreBindingClosure);
    dictionary_util_.emplace(
        std::move(client), dispatcher(),
        [this]() -> zx::result<fidl::ClientEnd<fruntime::Capabilities>> {
          route_connections_++;
          if (connect_status_ != ZX_OK) {
            return zx::error(connect_status_);
          }
          auto [route_client, route_server] = fidl::Endpoints<fruntime::Capabilities>::Create();
          route_bindings_.AddBinding(dispatcher(), std::move(route_server), &route_connection_,
                                     fidl::kIgnoreBindingClosure);
          return zx::ok(std::move(route_client));
        });
  }

  // Creates a dictionary containing |entries| and returns a handle to it.
  zx::eventpair MintDictionary(std::map<std::string, FakeCapabilityPtr> entries) {
    FakeCapabilityPtr dictionary = NewCapability(fruntime::CapabilityType::kDictionary);
    dictionary->entries = std::move(entries);
    return registry_->Mint(std::move(dictionary));
  }

  zx::result<fidl::ClientEnd<fio::Directory>> DictionaryDirConnectorOpen(
      const zx::eventpair& dictionary, std::string_view key) {
    std::optional<zx::result<fidl::ClientEnd<fio::Directory>>> result;
    dictionary_util_->DictionaryDirConnectorOpen(
        dictionary, key, [&result](zx::result<fidl::ClientEnd<fio::Directory>> open_result) {
          result.emplace(std::move(open_result));
        });
    RunLoopUntilIdle();
    ZX_ASSERT_MSG(result.has_value(), "DictionaryDirConnectorOpen did not complete");
    return std::move(result.value());
  }

  std::shared_ptr<FakeRegistry> registry_ = std::make_shared<FakeRegistry>();
  FakeCapabilities main_connection_{registry_};
  FakeCapabilities route_connection_{registry_};
  fidl::ServerBindingGroup<fruntime::Capabilities> main_bindings_;
  fidl::ServerBindingGroup<fruntime::Capabilities> route_bindings_;
  int route_connections_ = 0;
  zx_status_t connect_status_ = ZX_OK;
  std::optional<driver_manager::DictionaryUtil> dictionary_util_;
};

TEST_F(DictionaryUtilTest, CopyDictionary) {
  FakeCapabilityPtr entry = NewCapability(fruntime::CapabilityType::kDirConnector);
  zx::eventpair dictionary = MintDictionary({{"svc", entry}});
  FakeCapabilityPtr original = registry_->Lookup(dictionary);

  std::optional<zx::result<zx::eventpair>> result;
  dictionary_util_->CopyDictionary(
      dictionary, [&result](zx::result<zx::eventpair> copy) { result.emplace(std::move(copy)); });
  RunLoopUntilIdle();

  ASSERT_TRUE(result.has_value());
  ASSERT_TRUE(result->is_ok()) << result->status_string();
  FakeCapabilityPtr copy = registry_->Lookup(result->value());
  ASSERT_NE(copy, nullptr);
  EXPECT_NE(copy, original);
  EXPECT_EQ(copy->type, fruntime::CapabilityType::kDictionary);
  EXPECT_EQ(copy->entries, original->entries);
  // The caller's handle is still usable.
  EXPECT_EQ(registry_->Lookup(dictionary), original);
  EXPECT_EQ(main_connection_.calls(), std::vector<std::string>{"DictionaryCopy"});
}

TEST_F(DictionaryUtilTest, CopyDictionaryError) {
  zx::eventpair dictionary = MintDictionary({});
  main_connection_.set_copy_error(fruntime::CapabilitiesError::kInvalidCapabilityType);

  std::optional<zx::result<zx::eventpair>> result;
  dictionary_util_->CopyDictionary(
      dictionary, [&result](zx::result<zx::eventpair> copy) { result.emplace(std::move(copy)); });
  RunLoopUntilIdle();

  ASSERT_TRUE(result.has_value());
  EXPECT_EQ(result->status_value(), ZX_ERR_INTERNAL);
}

TEST_F(DictionaryUtilTest, CopyDictionaryRequiresDuplicateRight) {
  zx::eventpair dictionary = MintDictionary({});
  zx::eventpair restricted;
  ASSERT_EQ(ZX_OK, dictionary.replace(ZX_RIGHT_TRANSFER, &restricted));

  std::optional<zx::result<zx::eventpair>> result;
  dictionary_util_->CopyDictionary(
      restricted, [&result](zx::result<zx::eventpair> copy) { result.emplace(std::move(copy)); });
  RunLoopUntilIdle();

  ASSERT_TRUE(result.has_value());
  EXPECT_EQ(result->status_value(), ZX_ERR_ACCESS_DENIED);
  EXPECT_TRUE(main_connection_.calls().empty());
}

TEST_F(DictionaryUtilTest, DictionaryDirConnectorOpenDirConnector) {
  FakeCapabilityPtr dir_connector = NewCapability(fruntime::CapabilityType::kDirConnector);
  zx::eventpair dictionary = MintDictionary({{"svc", dir_connector}});

  zx::result directory = DictionaryDirConnectorOpen(dictionary, "svc");

  ASSERT_TRUE(directory.is_ok()) << directory.status_string();
  ASSERT_EQ(main_connection_.dir_connector_opens().size(), 1u);
  const auto& open = main_connection_.dir_connector_opens()[0];
  EXPECT_EQ(open.dir_connector, dir_connector);
  EXPECT_EQ(open.flags, fio::Flags::kProtocolDirectory | fio::kPermReadable);
  EXPECT_EQ(GetBasicInfo(directory->channel().get()).koid,
            GetBasicInfo(open.channel.channel().get()).related_koid);
  EXPECT_EQ(route_connections_, 0);
  EXPECT_EQ(main_connection_.calls(),
            (std::vector<std::string>{"DictionaryGet", "DirConnectorOpen"}));
}

TEST_F(DictionaryUtilTest, DictionaryDirConnectorOpenRoutesOnDedicatedConnection) {
  zx::eventpair dictionary =
      MintDictionary({{"svc", NewCapability(fruntime::CapabilityType::kDirConnectorRouter)}});

  zx::result directory = DictionaryDirConnectorOpen(dictionary, "svc");

  ASSERT_TRUE(directory.is_ok()) << directory.status_string();
  EXPECT_EQ(route_connections_, 1);
  EXPECT_EQ(route_connection_.calls(),
            (std::vector<std::string>{"InstanceTokenCreate", "DirConnectorRouterRoute"}));
  EXPECT_EQ(main_connection_.calls(),
            (std::vector<std::string>{"DictionaryGet", "DirConnectorOpen"}));
  ASSERT_EQ(main_connection_.dir_connector_opens().size(), 1u);
  EXPECT_EQ(main_connection_.dir_connector_opens()[0].dir_connector->type,
            fruntime::CapabilityType::kDirConnector);
}

TEST_F(DictionaryUtilTest, DictionaryDirConnectorOpenRouteUnavailable) {
  zx::eventpair dictionary =
      MintDictionary({{"svc", NewCapability(fruntime::CapabilityType::kDirConnectorRouter)}});
  route_connection_.set_route_result(fit::ok(fruntime::RouterResponse::kUnavailable));

  zx::result directory = DictionaryDirConnectorOpen(dictionary, "svc");

  EXPECT_EQ(directory.status_value(), ZX_ERR_UNAVAILABLE);
  EXPECT_TRUE(main_connection_.dir_connector_opens().empty());
}

TEST_F(DictionaryUtilTest, DictionaryDirConnectorOpenRouteError) {
  zx::eventpair dictionary =
      MintDictionary({{"svc", NewCapability(fruntime::CapabilityType::kDirConnectorRouter)}});
  route_connection_.set_route_result(fit::error(ZX_ERR_NOT_FOUND));

  zx::result directory = DictionaryDirConnectorOpen(dictionary, "svc");

  EXPECT_EQ(directory.status_value(), ZX_ERR_INTERNAL);
  EXPECT_TRUE(main_connection_.dir_connector_opens().empty());
}

TEST_F(DictionaryUtilTest, DictionaryDirConnectorOpenConnectFailure) {
  zx::eventpair dictionary =
      MintDictionary({{"svc", NewCapability(fruntime::CapabilityType::kDirConnectorRouter)}});
  connect_status_ = ZX_ERR_PEER_CLOSED;

  zx::result directory = DictionaryDirConnectorOpen(dictionary, "svc");

  EXPECT_EQ(directory.status_value(), ZX_ERR_PEER_CLOSED);
  EXPECT_TRUE(route_connection_.calls().empty());
}

TEST_F(DictionaryUtilTest, DictionaryDirConnectorOpenUnexpectedType) {
  zx::eventpair dictionary =
      MintDictionary({{"svc", NewCapability(fruntime::CapabilityType::kData)}});

  zx::result directory = DictionaryDirConnectorOpen(dictionary, "svc");

  EXPECT_EQ(directory.status_value(), ZX_ERR_WRONG_TYPE);
  EXPECT_EQ(route_connections_, 0);
}

TEST_F(DictionaryUtilTest, DictionaryDirConnectorOpenMissingKey) {
  zx::eventpair dictionary = MintDictionary({});

  zx::result directory = DictionaryDirConnectorOpen(dictionary, "svc");

  EXPECT_EQ(directory.status_value(), ZX_ERR_INTERNAL);
}

TEST_F(DictionaryUtilTest, CreateDictionaryWith) {
  std::unordered_map<std::string, fidl::ClientEnd<fruntime::DirReceiver>> receivers;
  std::vector<fidl::ServerEnd<fruntime::DirReceiver>> receiver_servers;
  for (const char* name : {"a", "b"}) {
    auto [client, server] = fidl::Endpoints<fruntime::DirReceiver>::Create();
    receivers.emplace(name, std::move(client));
    receiver_servers.push_back(std::move(server));
  }

  std::optional<zx::result<zx::eventpair>> result;
  dictionary_util_->CreateDictionaryWith(
      std::move(receivers),
      [&result](zx::result<zx::eventpair> dictionary) { result.emplace(std::move(dictionary)); });
  RunLoopUntilIdle();

  ASSERT_TRUE(result.has_value());
  ASSERT_TRUE(result->is_ok()) << result->status_string();
  FakeCapabilityPtr dictionary = registry_->Lookup(result->value());
  ASSERT_NE(dictionary, nullptr);
  ASSERT_EQ(dictionary->entries.size(), 2u);
  EXPECT_EQ(dictionary->entries["a"]->type, fruntime::CapabilityType::kDirConnector);
  EXPECT_EQ(dictionary->entries["b"]->type, fruntime::CapabilityType::kDirConnector);
}

TEST_F(DictionaryUtilTest, CreateDictionaryWithNoReceivers) {
  std::optional<zx::result<zx::eventpair>> result;
  dictionary_util_->CreateDictionaryWith({}, [&result](zx::result<zx::eventpair> dictionary) {
    result.emplace(std::move(dictionary));
  });
  RunLoopUntilIdle();

  ASSERT_TRUE(result.has_value());
  ASSERT_TRUE(result->is_ok()) << result->status_string();
  FakeCapabilityPtr dictionary = registry_->Lookup(result->value());
  ASSERT_NE(dictionary, nullptr);
  EXPECT_TRUE(dictionary->entries.empty());
}

// Records the Open requests sent to a directory.
class RecordingDirectory : public fidl::testing::TestBase<fio::Directory> {
 public:
  struct OpenCall {
    std::string path;
    fio::Flags flags;
  };

  const std::vector<OpenCall>& opens() const { return opens_; }

  void Open(OpenRequest& request, OpenCompleter::Sync& completer) override {
    opens_.push_back(OpenCall{.path = request.path(), .flags = request.flags()});
  }

 private:
  void handle_unknown_method(fidl::UnknownMethodMetadata<fio::Directory> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    ADD_FAILURE() << "Unexpected call to Directory::" << name;
  }

  std::vector<OpenCall> opens_;
};

class DirReceiverImplTest : public gtest::TestLoopFixture {
 protected:
  // Starts a DirReceiverImpl that serves a single primary directory backed by |directory_|.
  void StartReceiver(std::unordered_map<std::string, std::string> instance_mapping) {
    auto [dir_client, dir_server] = fidl::Endpoints<fio::Directory>::Create();
    directory_bindings_.AddBinding(dispatcher(), std::move(dir_server), &directory_,
                                   fidl::kIgnoreBindingClosure);
    std::vector<driver_manager::DirInfo> dir_infos;
    dir_infos.push_back(driver_manager::DirInfo{
        .dir = std::move(dir_client),
        .target_service_name = "fuchsia.example.Service",
        .target_to_source_instance_mapping = std::move(instance_mapping),
        .is_primary = true,
    });
    auto [receiver_client, receiver_server] = fidl::Endpoints<fruntime::DirReceiver>::Create();
    receiver_.emplace(std::move(receiver_server), std::move(dir_infos), dispatcher());
    receiver_client_.Bind(std::move(receiver_client), dispatcher());
  }

  void Receive(std::string_view path, fio::Flags rights) {
    auto [client, server] = fidl::Endpoints<fio::Directory>::Create();
    ASSERT_TRUE(
        receiver_client_->Receive(std::move(server), fidl::StringView::FromExternal(path), rights)
            .ok());
    RunLoopUntilIdle();
  }

  RecordingDirectory directory_;
  fidl::ServerBindingGroup<fio::Directory> directory_bindings_;
  std::optional<driver_manager::DirReceiverImpl> receiver_;
  fidl::WireClient<fruntime::DirReceiver> receiver_client_;
};

TEST_F(DirReceiverImplTest, OpensServiceMemberInMappedInstance) {
  StartReceiver({{"default", "instance-1"}});
  const fio::Flags rights = fio::Flags::kProtocolService | fio::kPermReadable;

  Receive("default/device", rights);

  ASSERT_EQ(directory_.opens().size(), 1u);
  EXPECT_EQ(directory_.opens()[0].path, "instance-1/device");
  EXPECT_EQ(directory_.opens()[0].flags, rights);
}

TEST_F(DirReceiverImplTest, OpensRootForEmptyPath) {
  StartReceiver({{"default", "default"}});
  const fio::Flags rights =
      fio::kPermReadable | fio::Flags::kPermInheritWrite | fio::Flags::kPermInheritExecute;

  Receive("", rights);
  Receive(".", rights);

  ASSERT_EQ(directory_.opens().size(), 2u);
  EXPECT_EQ(directory_.opens()[0].flags, rights);
  EXPECT_EQ(directory_.opens()[1].flags, rights);
}

TEST_F(DirReceiverImplTest, IgnoresUnknownInstance) {
  StartReceiver({{"default", "default"}});

  Receive("other/device", fio::kPermReadable);

  EXPECT_TRUE(directory_.opens().empty());
}

}  // namespace
