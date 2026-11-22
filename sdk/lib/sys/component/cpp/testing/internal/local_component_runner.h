// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_SYS_COMPONENT_CPP_TESTING_INTERNAL_LOCAL_COMPONENT_RUNNER_H_
#define LIB_SYS_COMPONENT_CPP_TESTING_INTERNAL_LOCAL_COMPONENT_RUNNER_H_

#include <fidl/fuchsia.component.runner/cpp/fidl.h>
#include <fidl/fuchsia.component.test/cpp/fidl.h>
#include <lib/async/dispatcher.h>
#include <lib/fidl/cpp/wire/channel.h>
#include <lib/fit/function.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>

#include <map>
#include <memory>
#include <optional>
#include <string>

namespace component_testing {
namespace internal {

class LocalComponentRunner;

class LocalComponentInstance final
    : public fidl::Server<fuchsia_component_runner::ComponentController>,
      public std::enable_shared_from_this<LocalComponentInstance> {
 public:
  // Constructed by the LocalComponentRunner when the runner receives a request
  // to start a local component.
  static std::shared_ptr<LocalComponentInstance> Create(
      fidl::ServerEnd<fuchsia_component_runner::ComponentController> controller,
      async_dispatcher_t* dispatcher, LocalComponentFactory component_factory,
      fuchsia_component_runner::ComponentStartInfo start_info,
      fit::function<void()> on_instance_exit);

  LocalComponentInstance(LocalComponentInstance&& other) = delete;
  LocalComponentInstance& operator=(LocalComponentInstance&& other) = delete;

  LocalComponentInstance(const LocalComponentInstance& other) = delete;
  LocalComponentInstance& operator=(const LocalComponentInstance& other) = delete;

  // Called after constructing the |LocalComponentInstance|, to start the
  // component.
  void Start();

  // Returns true after Start() and before Exit().
  bool IsRunning() const;

 private:
  explicit LocalComponentInstance(fit::function<void()> on_instance_exit);

  void Init(fidl::ServerEnd<fuchsia_component_runner::ComponentController> controller,
            async_dispatcher_t* dispatcher, LocalComponentFactory component_factory,
            fuchsia_component_runner::ComponentStartInfo start_info);

  // fidl::Server<fuchsia_component_runner::ComponentController>
  void Stop(StopCompleter::Sync& completer) override;
  void Kill(KillCompleter::Sync& completer) override { Exit(ZX_ERR_CANCELED); }
  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_component_runner::ComponentController> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;

  // Close the ComponentController and call the given on_exit function.
  void Exit(zx_status_t);

  std::optional<fidl::ServerBindingRef<fuchsia_component_runner::ComponentController>> binding_;

  // If a |LocalComponentImplBase| calls `Exit()` during
  // `LocalComponentImplBase::OnStart()`, the LocalComponentInstance will _not_
  // immediately call `LocalComponentInstance::Exit()`. It will save the
  // provided status, and call `LocalComponentInstance::Exit()` after the
  // component has `started_`.
  std::optional<zx_status_t> pending_exit_status_;

  // Set to true at the beginning of `Start()`, and false at the completion
  // of `Start()`.
  bool starting_;

  // Set to true at the completion of `Start()`.
  bool started_;

  // Called when the component is exiting, purposefully or as a result of a
  // ComponentController::Kill().
  fit::closure on_exit_;

  std::unique_ptr<LocalComponentImplBase> local_component_;
};

using LocalComponents = std::map<std::string, LocalComponentKind>;
using LocalComponentInstances = std::map<std::string, std::shared_ptr<LocalComponentInstance>>;

class LocalComponentRunner final {
 public:
  LocalComponentRunner(LocalComponents components, async_dispatcher_t* dispatcher);
  ~LocalComponentRunner();

  LocalComponentRunner(LocalComponentRunner&& other) = delete;
  LocalComponentRunner& operator=(LocalComponentRunner&& other) = delete;

  LocalComponentRunner(const LocalComponentRunner& other) = delete;
  LocalComponentRunner& operator=(const LocalComponentRunner& other) = delete;

  fidl::ClientEnd<fuchsia_component_runner::ComponentRunner> NewBinding();

  class Builder;

 private:
  class Server;

  std::shared_ptr<Server> server_;
  std::optional<fidl::ServerBindingRef<fuchsia_component_runner::ComponentRunner>> binding_;
  async_dispatcher_t* dispatcher_;
};

class LocalComponentRunner::Builder final {
 public:
  Builder() = default;

  Builder(LocalComponentRunner::Builder&& other) = default;
  Builder& operator=(LocalComponentRunner::Builder&& other) = default;

  Builder(const LocalComponentRunner::Builder& other) = delete;
  Builder& operator=(const LocalComponentRunner::Builder& other) = delete;

  std::unique_ptr<LocalComponentRunner> Build(async_dispatcher_t* dispatcher);

  void Register(std::string name, LocalComponentKind mock);

 private:
  bool Contains(std::string name) const;

  LocalComponents components_;
};

}  // namespace internal
}  // namespace component_testing

#endif  // LIB_SYS_COMPONENT_CPP_TESTING_INTERNAL_LOCAL_COMPONENT_RUNNER_H_
