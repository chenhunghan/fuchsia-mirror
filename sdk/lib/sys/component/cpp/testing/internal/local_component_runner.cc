// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.component.runner/cpp/fidl.h>
#include <fidl/fuchsia.component.test/cpp/fidl.h>
#include <fidl/fuchsia.data/cpp/fidl.h>
#include <fidl/fuchsia.io/cpp/fidl.h>
#include <lib/async/dispatcher.h>
#include <lib/fdio/namespace.h>
#include <lib/fidl/cpp/wire/channel.h>
#include <lib/sys/component/cpp/testing/internal/errors.h>
#include <lib/sys/component/cpp/testing/internal/local_component_runner.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <zircon/assert.h>
#include <zircon/availability.h>
#include <zircon/status.h>

#include <cstddef>
#include <memory>
#include <optional>
#include <string>
#include <utility>
#include <variant>

namespace component_testing {
namespace internal {

namespace {

std::string ExtractLocalComponentName(const fuchsia_data::Dictionary& program) {
  ZX_ASSERT_MSG(program.entries().has_value(), "Received empty program from Component Manager");
  for (const auto& entry : *program.entries()) {
    if (entry.key() == fuchsia_component_test::kLocalComponentNameKey) {
      ZX_ASSERT_MSG(entry.value() && entry.value()->str().has_value(),
                    "Received local component key of wrong type");
      return entry.value()->str().value();
    }
  }

  ZX_PANIC("Received program without local component key");
}

}  // namespace

std::shared_ptr<LocalComponentInstance> LocalComponentInstance::Create(
    fidl::ServerEnd<fuchsia_component_runner::ComponentController> controller,
    async_dispatcher_t* dispatcher, LocalComponentFactory local_component_factory,
    fuchsia_component_runner::ComponentStartInfo start_info,
    fit::function<void()> on_instance_exit) {
  std::shared_ptr<LocalComponentInstance> instance(
      new LocalComponentInstance(std::move(on_instance_exit)));
  instance->Init(std::move(controller), dispatcher, std::move(local_component_factory),
                 std::move(start_info));
  return instance;
}

LocalComponentInstance::LocalComponentInstance(fit::function<void()> on_instance_exit)
    : starting_(false), started_(false), on_exit_(std::move(on_instance_exit)) {}

void LocalComponentInstance::Init(
    fidl::ServerEnd<fuchsia_component_runner::ComponentController> controller,
    async_dispatcher_t* dispatcher, LocalComponentFactory local_component_factory,
    fuchsia_component_runner::ComponentStartInfo start_info) {
  binding_ = fidl::BindServer(dispatcher, std::move(controller), shared_from_this());
  local_component_ = local_component_factory();
  fdio_ns_t* ns;
  ZX_COMPONENT_ASSERT_STATUS_OK("CreateHandlesFromStartInfo", fdio_ns_create(&ns));
  if (start_info.ns().has_value()) {
    for (auto& entry : *start_info.ns()) {
      ZX_ASSERT_MSG(entry.path().has_value() && entry.directory().has_value(),
                    "Namespace entry missing path or directory");
      ZX_COMPONENT_ASSERT_STATUS_OK(
          "CreateHandlesFromStartInfo",
          fdio_ns_bind(ns, entry.path()->c_str(), entry.directory()->TakeChannel().release()));
    }
  }
  zx::channel outgoing_dir;
  if (start_info.outgoing_dir().has_value()) {
    outgoing_dir = start_info.outgoing_dir()->TakeChannel();
  }
  ZX_COMPONENT_ASSERT_STATUS_OK(
      "Initialize namespace and outgoing directory",
      local_component_->Initialize(ns, std::move(outgoing_dir), dispatcher,
                                   [this](zx_status_t status) { Exit(status); }));
}

void LocalComponentInstance::Start() {
  starting_ = true;
  local_component_->OnStart();
  started_ = true;
  starting_ = false;
  if (pending_exit_status_) {
    // `ComponentInstance::Exit()` (which calls the
    // `ComponentInstance::on_exit_` callback) must not be called before the
    // `on_start_` callback completes.
    //
    // A LocalComponentImplBase may call `Exit()` during the
    // `LocalComponentImplBase::OnStart()` method. This is a legitimate use case, if
    // the component can complete its work via synchronous calls. See the
    // `RoutesProtocolToLocalComponentSync` test in `realm_builder_test.cc`, for
    // example. This test's component uses an `EchoSyncPtr` to invoke a client
    // request and get a response, before the `OnStart()` method completes.
    // Since the work is done, the client component is safe to terminate, by
    // calling `Exit()`.
    //
    // Note that calling `Exit()` before `on_start_` saves the provided status
    // (see above), but delays the call to `LocalComponentInstance::Exit()`
    // until here, after `on_start_` has completed.
    Exit(*pending_exit_status_);
  }
}

bool LocalComponentInstance::IsRunning() const {
  return (starting_ || started_) && !pending_exit_status_ && binding_.has_value();
}

void LocalComponentInstance::Stop(StopCompleter::Sync& completer) {
  if (local_component_) {
    local_component_->OnStop();
  }

  Exit(ZX_OK);
  // The component should exit the loop, if any, or should have already
  // terminated. When it terminates, it should close the ComponentController.
  // If it doesn't, component manager will call Kill(), which can force close
  // the ComponentController.
}

void LocalComponentInstance::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_component_runner::ComponentController> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {}

void LocalComponentInstance::Exit(zx_status_t epitaph_value) {
  // Don't actually exit during Start. Start will check pending_exit_status_ at the end.
  if (!started_) {
    pending_exit_status_ = epitaph_value;
    return;
  }

  if (binding_.has_value()) {
    binding_->Close(epitaph_value);
    binding_.reset();
  }
  local_component_.reset();
  if (on_exit_) {
    auto on_exit = std::move(on_exit_);
    on_exit_ = nullptr;
    on_exit();
  }
}

class LocalComponentRunner::Server final
    : public fidl::Server<fuchsia_component_runner::ComponentRunner> {
 public:
  Server(LocalComponents components, async_dispatcher_t* dispatcher)
      : ready_components_(std::move(components)), dispatcher_(dispatcher) {}

  void Start(StartRequest& request, StartCompleter::Sync& completer) override {
    ZX_ASSERT_MSG(request.start_info().program().has_value(),
                  "Component manager sent start_info without program");
    std::string const name = ExtractLocalComponentName(*request.start_info().program());

    auto local_component_factory = [this, name]() { return SetComponentToRunning(name); };
    auto on_exit = [this, name]() { SetComponentToReady(name); };

    auto instance = LocalComponentInstance::Create(
        std::move(request.controller()), dispatcher_, std::move(local_component_factory),
        std::move(request.start_info()), std::move(on_exit));
    running_component_instances_[name] = instance;
    // Start the component instance.
    instance->Start();
  }

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_component_runner::ComponentRunner> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {}

 private:
  std::unique_ptr<LocalComponentImplBase> SetComponentToRunning(const std::string& name) {
    ZX_ASSERT_MSG(ready_components_.find(name) != ready_components_.cend(),
                  "Component manager requested a named LocalComponent that is unregistered, "
                  "already running, or not restartable. Component name: %s",
                  name.c_str());
    running_components_[name] = std::move(ready_components_[name]);
    ZX_ASSERT_MSG(ready_components_.erase(name) == 1, "ready component not erased");
    return std::get<LocalComponentFactory>(running_components_[name])();
  }

  void SetComponentToReady(const std::string& name) {
    ZX_ASSERT_MSG(running_components_.find(name) != running_components_.cend(),
                  "Component manager requested a named LocalComponent that is unregistered, "
                  "already running, or not restartable. Component name: %s",
                  name.c_str());
    // Return the factory back to the list of components that can be restarted.
    ready_components_[name] = std::move(running_components_[name]);
    ZX_ASSERT_MSG(running_components_.erase(name) == 1, "running component not erased");
    // Drop the ComponentInstance.
    ZX_ASSERT_MSG(running_component_instances_.erase(name) == 1,
                  "running component instance not erased");
  }

  LocalComponents ready_components_;
  LocalComponents running_components_;
  LocalComponentInstances running_component_instances_;
  async_dispatcher_t* dispatcher_;
};

LocalComponentRunner::LocalComponentRunner(LocalComponents components,
                                           async_dispatcher_t* dispatcher)
    : server_(std::make_shared<Server>(std::move(components), dispatcher)),
      dispatcher_(dispatcher) {}

LocalComponentRunner::~LocalComponentRunner() {
  if (binding_.has_value()) {
    binding_->Unbind();
  }
}

fidl::ClientEnd<fuchsia_component_runner::ComponentRunner> LocalComponentRunner::NewBinding() {
  auto [client, server] = fidl::Endpoints<fuchsia_component_runner::ComponentRunner>::Create();
  binding_ = fidl::BindServer(dispatcher_, std::move(server), server_);
  return std::move(client);
}

std::unique_ptr<LocalComponentRunner> LocalComponentRunner::Builder::Build(
    async_dispatcher_t* dispatcher) {
  return std::make_unique<LocalComponentRunner>(std::move(components_), dispatcher);
}

void LocalComponentRunner::Builder::Register(std::string name, LocalComponentKind mock) {
  ZX_ASSERT_MSG(!Contains(name), "Local component with same name being added: %s", name.c_str());
  components_[name] = std::move(mock);
}

bool LocalComponentRunner::Builder::Contains(std::string name) const {
  return components_.find(name) != components_.cend();
}

}  // namespace internal
}  // namespace component_testing
