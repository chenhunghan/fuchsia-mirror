// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_TESTING_CPP_DRIVER_TEST_H_
#define LIB_DRIVER_TESTING_CPP_DRIVER_TEST_H_

#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/driver/testing/cpp/health_test_utils.h>
#include <lib/driver/testing/cpp/internal/internals.h>

// This library provides RAII-style classes for writing driver unit tests. It contains two classes,
// |ForegroundDriverTest| and |BackgroundDriverTest| that contain all the logic for a driver test.
// These classes can also be used directly in a test as a test fixture field, or as a local in a
// non-fixture based test method. This should be the first field or local that is being created.
//
// The choice between foreground and background driver tests lies in how the test plans to
// communicate with the driver-under-test. If the test will be calling public methods on the driver
// a lot, the foreground driver test should be chosen. If the test will be calling through the
// driver's exposed FIDL more often, then the background driver test should be chosen.
//
// Both test kinds can be configured through a struct/class provided through a template parameter.
// This configuration must define two types through using statements:
//
//   DriverType: The type of the driver under test.
//     If using a test-specific driver, ensure the DriverType contains a static function
//     in the format below. This registration is what the test uses to manage the driver.
//       `static DriverRegistration GetDriverRegistration()`
//     If the driver type is not known to the test, or is not needed by the test, then use the
//     |EmptyDriverType| class as a placeholder.
//
//   EnvironmentType: A class that contains environment dependencies of the driver-under-test.
//     This must inherit from the |Environment| base class and provide the |Serve| method.
//     It must also contain a default constructor.
//     The environment will live on a background dispatcher during the lifetime of the test.
namespace fdf_testing {

// The configuration must be given a non-void DriverType, but not all tests need access to a
// driver type (eg. they don't need to access or call anything on the driver class). This can be
// used as a placeholder to avoid build errors. The driver lifecycle is managed separately, through
// the `__fuchsia_driver_registration__` symbol, or through its custom registration provided with
// GetDriverRegistration.
class EmptyDriverType {};

// The EnvironmentType must implement this class.
class Environment {
 public:
  virtual ~Environment() = default;
  // This function is called on the dispatcher context of the environment. The class should serve
  // its elements (eg. compat::DeviceServer, FIDL servers, etc...) to the |to_driver_vfs|.
  // This object is serving the incoming directory of the driver under test.
  virtual zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) = 0;
};

namespace internal {
// Common logic for both background and foreground driver tests.
//
// Both foreground and background test classes inherit from this common class and provide
// workflows for their specific threading-model.
//
// This class is not meant for direct use so its in the internal namespace.
template <typename Configuration>
#if __cplusplus >= 202002l
  requires(HasDriverType<Configuration> && HasEnvironmentType<Configuration>)
#endif
class DriverTestCommon {
 public:
  using DriverType = typename Configuration::DriverType;
  using EnvironmentType = typename Configuration::EnvironmentType;

  DriverTestCommon()
      : env_dispatcher_(runtime_.StartBackgroundDispatcher()),
        env_wrapper_(env_dispatcher_->async_dispatcher(), std::in_place) {}

  virtual ~DriverTestCommon() = default;

  // Access the driver runtime object. This can be used to create new background dispatchers
  // or to run the foreground dispatcher.
  fdf_testing::DriverRuntime& runtime() { return runtime_; }

  // Connects to a service member that the driver under test provides.
  template <typename ServiceMember,
            typename = std::enable_if_t<fidl::IsServiceMemberV<ServiceMember>>>
  zx::result<fidl::internal::ClientEndType<typename ServiceMember::ProtocolType>> Connect(
      std::string_view instance = component::kDefaultInstance) {
    if constexpr (std::is_same_v<typename ServiceMember::ProtocolType::Transport,
                                 fidl::internal::ChannelTransport>) {
      return component::ConnectAtMember<ServiceMember>(ConnectToDriverSvcDir(), instance);
    } else if constexpr (std::is_same_v<typename ServiceMember::ProtocolType::Transport,
                                        fidl::internal::DriverTransport>) {
      return fdf::internal::DriverTransportConnect<ServiceMember>(ConnectToDriverSvcDir(),
                                                                  instance);
    } else {
      static_assert(false);
    }
  }

  // Runs a task on the dispatcher context of the EnvironmentType. This will be a different thread
  // than the main test thread, so be careful when capturing and returning pointers to objects that
  // live on different dispatchers like test fixture properties, or the driver.
  //
  // Returns the result of the given task once it has completed.
  template <typename T>
  T RunInEnvironmentTypeContext(fit::callback<T(EnvironmentType&)> task) {
    return env_wrapper_.SyncCall(
        [env_task = std::move(task)](EnvWrapper<EnvironmentType>* env_ptr) mutable {
          return env_task(env_ptr->user_env());
        });
  }

  // Runs a task on the dispatcher context of the EnvironmentType. This will be a different thread
  // than the main test thread, so be careful when capturing and returning pointers to objects that
  // live on different dispatchers like test fixture properties, or the driver.
  //
  // Returns when the given task has completed.
  void RunInEnvironmentTypeContext(fit::callback<void(EnvironmentType&)> task) {
    env_wrapper_.SyncCall(
        [env_task = std::move(task)](EnvWrapper<EnvironmentType>* env_ptr) mutable {
          env_task(env_ptr->user_env());
        });
  }

  // Runs a task on the dispatcher context of the TestNode. This will be a different thread than
  // the main test thread, so be careful when capturing and returning pointers to objects that live
  // on different dispatchers like test fixture properties, or the driver.
  //
  // Returns the result of the given task once it has completed.
  template <typename T>
  T RunInNodeContext(fit::callback<T(fdf_testing::TestNode&)> task) {
    return env_wrapper_.SyncCall(
        [node_task = std::move(task)](EnvWrapper<EnvironmentType>* env_ptr) mutable {
          return node_task(env_ptr->node_server());
        });
  }

  // Runs a task on the dispatcher context of the TestNode. This will be a different thread than
  // the main test thread, so be careful when capturing and returning pointers to objects that live
  // on different dispatchers like test fixture properties, or the driver.
  //
  // Returns when the given task has completed.
  void RunInNodeContext(fit::callback<void(fdf_testing::TestNode&)> task) {
    env_wrapper_.SyncCall(
        [node_task = std::move(task)](EnvWrapper<EnvironmentType>* env_ptr) mutable {
          node_task(env_ptr->node_server());
        });
  }

  // Connect to a zircon transport based protocol through a devfs node that the driver under test
  // exports. The |devfs_node_name| is the name of the created node with the 'devfs_args'. This
  // node must have been created through the driver's immediate node. If the devfs node is nested,
  // use the variant that takes a vector of strings.
  template <typename ProtocolType, typename = std::enable_if_t<fidl::IsProtocolV<ProtocolType>>>
  zx::result<fidl::ClientEnd<ProtocolType>> ConnectThroughDevfs(std::string_view devfs_node_name) {
    return ConnectThroughDevfs<ProtocolType>(std::vector{std::string(devfs_node_name)});
  }

  // Connect to a zircon transport based protocol through a devfs node that the driver under test
  // exports. The |devfs_node_name_path| is a list of node names that should be traversed to reach
  // the devfs node. The last element in the vector is the name of the created node with the
  // 'devfs_args'.
  template <typename ProtocolType, typename = std::enable_if_t<fidl::IsProtocolV<ProtocolType>>>
  zx::result<fidl::ClientEnd<ProtocolType>> ConnectThroughDevfs(
      std::vector<std::string> devfs_node_name_path) {
    zx::result<zx::channel> raw_channel_result =
        env_wrapper_.SyncCall([devfs_node_name_path = std::move(devfs_node_name_path)](
                                  EnvWrapper<EnvironmentType>* env_ptr) mutable {
          fdf_testing::TestNode* current = &env_ptr->node_server();
          for (auto& node : devfs_node_name_path) {
            current = &current->children().at(node);
          }

          return current->ConnectToDevice();
        });
    if (raw_channel_result.is_error()) {
      return raw_channel_result.take_error();
    }

    return zx::ok(fidl::ClientEnd<ProtocolType>(std::move(raw_channel_result.value())));
  }

  // Start the driver.
  zx::result<> StartDriver() {
    ZX_ASSERT_MSG(
        !start_result_.has_value(),
        "Cannot call |StartDriver| multiple times in a row. If multiple starts are needed, "
        "ensure to go through |StopDriver| and |ShutdownAndDestroyDriver| first.");

    fuchsia_driver_framework::DriverStartArgs start_args =
        env_wrapper_.SyncCall(&EnvWrapper<EnvironmentType>::Init);
    outgoing_directory_client_ =
        env_wrapper_.SyncCall(&EnvWrapper<EnvironmentType>::TakeOutgoingClient);

    AddServiceValidation(start_args);

    start_result_ = StartDriverInner(std::move(start_args));
    return *start_result_;
  }

  // Start the driver with modified DriverStartArgs. This is done through the |args_modifier|
  // which is called with a reference to the start args that will be used to start the driver.
  // Modifications can happen in-place with this reference.
  zx::result<> StartDriverWithCustomStartArgs(
      fit::callback<void(fuchsia_driver_framework::DriverStartArgs&)> args_modifier) {
    ZX_ASSERT_MSG(
        !start_result_.has_value(),
        "Cannot call |StartDriver| multiple times in a row. If multiple starts are needed, "
        "ensure to go through |StopDriver| and |ShutdownAndDestroyDriver| first.");

    fuchsia_driver_framework::DriverStartArgs start_args =
        env_wrapper_.SyncCall(&EnvWrapper<EnvironmentType>::Init);
    outgoing_directory_client_ =
        env_wrapper_.SyncCall(&EnvWrapper<EnvironmentType>::TakeOutgoingClient);
    args_modifier(start_args);

    AddServiceValidation(start_args);

    start_result_ = StartDriverInner(std::move(start_args));
    return *start_result_;
  }

  // Stops the driver by calling the driver's PrepareStop hook and waiting for it to complete.
  zx::result<> StopDriver() {
    ZX_ASSERT_MSG(start_result_.has_value(), "Cannot stop without having started.");
    ZX_ASSERT_MSG(!prepare_stop_result_.has_value(),
                  "Ensure |StopDriver| is only called once after a |StartDriver| call.");

    if (StartedSuccessfully()) {
#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
      if (power_state_ == PowerState::kSuspended) {
        std::ignore = ResumeDriver();
      }
#endif
      prepare_stop_result_ = StopDriverInner();
    } else {
      // Drivers that failed to stop don't receive a PrepareStop call. Set the result as ok so
      // that the teardown continues successfully.
      prepare_stop_result_ = zx::ok();
    }

    return *prepare_stop_result_;
  }

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
  // Suspends the driver's dispatchers and calls the driver's SystemSuspend hook, waiting for it
  // to complete.
  zx::result<> SuspendDriver() {
    ZX_ASSERT_MSG(StartedSuccessfully(), "Cannot suspend without having started successfully.");
    ZX_ASSERT_MSG(!prepare_stop_result_.has_value(), "Cannot suspend after |StopDriver|.");
    ZX_ASSERT_MSG(power_state_ == PowerState::kResumed,
                  "Cannot call |SuspendDriver| when the driver is not in the resumed state.");
    zx::result<> result = SuspendDriverInner();
    power_state_ = result.is_ok() ? PowerState::kSuspended : PowerState::kTransitionFailed;
    return result;
  }

  // Calls the driver's SystemResume hook, waits for it to complete, and then resumes the driver's
  // dispatchers.
  zx::result<> ResumeDriver(
      std::optional<fuchsia_power_broker::LeaseToken> power_element_lease = std::nullopt) {
    ZX_ASSERT_MSG(StartedSuccessfully(), "Cannot resume without having started successfully.");
    ZX_ASSERT_MSG(!prepare_stop_result_.has_value(), "Cannot resume after |StopDriver|.");
    ZX_ASSERT_MSG(power_state_ == PowerState::kSuspended,
                  "Cannot call |ResumeDriver| when the driver is not suspended.");
    zx::result<> result = ResumeDriverInner(std::move(power_element_lease));
    power_state_ = result.is_ok() ? PowerState::kResumed : PowerState::kTransitionFailed;
    return result;
  }
#endif

  // Shuts down the driver dispatchers fully, and then calls the driver's destroy hook.
  // Can be called multiple times, but it will only do the required work the first time.
  //
  // If the driver running was started successfully, this must be called after |StopDriver|.
  void ShutdownAndDestroyDriver() {
    // Allow for calling this method multiple times, but only do this work the first time.
    if (DriverExists()) {
      if (StartedSuccessfully()) {
        ZX_ASSERT_MSG(prepare_stop_result_.has_value(),
                      "Ensure |ShutdownAndDestroyDriver| is only called once after "
                      "a |StopDriver| call when start was successful.");
      }

      // This will shut down the driver dispatcher (and sub-dispatchers) and call the driver's
      // destroy hook.
      ShutdownAndDestroyDriverInner();

      // Reset the start_result_ and prepare_stop_result_ to allow another iteration of the driver.
      start_result_.reset();
      prepare_stop_result_.reset();
#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
      power_state_ = PowerState::kResumed;
#endif
    }
  }

  fidl::ClientEnd<fuchsia_io::Directory> ConnectToDriverSvcDir() {
    auto [client_end, server_end] = fidl::Endpoints<fuchsia_io::Directory>::Create();
    zx_status_t status = fdio_open3_at(
        outgoing_directory_client_.handle()->get(), "/svc",
        uint64_t{fuchsia_io::wire::kPermReadable | fuchsia_io::wire::Flags::kProtocolDirectory},
        server_end.TakeChannel().release());
    ZX_ASSERT_MSG(ZX_OK == status, "Failed to fdio_open3_at '/svc' on the driver's outgoing: %s.",
                  zx_status_get_string(status));
    return std::move(client_end);
  }

  // This will enable the service instance validation on the driver. Equivalent to setting
  // `service_connect_validation: "true"` in the cml of the driver.
  // This must be called before |StartDriver|/|StartDriverWithCustomStartArgs| as it will only
  // take affect on the next start of the driver and cannot modify an already started driver.
  void SetServiceValidator(bool enable) { enable_service_connect_validation_ = enable; }

  // Sends a `HealthCheck` request over the `Driver` channel to the driver under test and waits for
  // its reply. Must be called after `StartDriver` succeeds and before `StopDriver`.
  zx::result<fit::result<fuchsia_driver_framework::HealthCheckError,
                         fuchsia_driver_framework::HealthCheckResponse>>
  CheckHealth(fuchsia_driver_framework::HealthCheckRequest request =
                  fuchsia_driver_framework::HealthCheckRequest::kPing) {
    ZX_ASSERT_MSG(StartedSuccessfully(),
                  "Cannot call |CheckHealth| before |StartDriver| succeeds.");
    ZX_ASSERT_MSG(!prepare_stop_result_.has_value(),
                  "Cannot call |CheckHealth| after |StopDriver|.");
    return CheckHealthInner(request);
  }

  // Drains and returns all `OnHealthAlert` events emitted by the driver under test in FIFO order.
  // Pending dispatcher work is pumped first so alerts already queued on the driver dispatcher have
  // arrived at the test client before draining.
  std::vector<fuchsia_driver_framework::HealthAlert> TakeHealthAlerts() {
    ZX_ASSERT_MSG(DriverExists(),
                  "Cannot call |TakeHealthAlerts| after |ShutdownAndDestroyDriver|.");
    return TakeHealthAlertsInner();
  }

  // Returns the number of `OnHealthAlert` events currently queued without draining them.
  size_t PendingHealthAlertCount() {
    ZX_ASSERT_MSG(DriverExists(),
                  "Cannot call |PendingHealthAlertCount| after |ShutdownAndDestroyDriver|.");
    return PendingHealthAlertCountInner();
  }

 private:
#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
  enum class PowerState {
    kResumed,
    kSuspended,
    kTransitionFailed,
  };
#endif

  virtual zx::result<> StartDriverInner(fuchsia_driver_framework::DriverStartArgs start_args) = 0;
  virtual zx::result<> StopDriverInner() = 0;
#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
  virtual zx::result<> SuspendDriverInner() = 0;
  virtual zx::result<> ResumeDriverInner(
      std::optional<fuchsia_power_broker::LeaseToken> power_element_lease) = 0;
#endif
  virtual bool DriverExists() = 0;
  virtual void ShutdownAndDestroyDriverInner() = 0;
  virtual zx::result<fit::result<fuchsia_driver_framework::HealthCheckError,
                                 fuchsia_driver_framework::HealthCheckResponse>>
  CheckHealthInner(fuchsia_driver_framework::HealthCheckRequest request) = 0;
  virtual std::vector<fuchsia_driver_framework::HealthAlert> TakeHealthAlertsInner() = 0;
  virtual size_t PendingHealthAlertCountInner() = 0;

  bool StartedSuccessfully() const { return start_result_.has_value() && start_result_->is_ok(); }

  void AddServiceValidation(fuchsia_driver_framework::DriverStartArgs& start_args) {
    if (enable_service_connect_validation_) {
      if (start_args.program() == std::nullopt) {
        start_args.program(fuchsia_data::Dictionary{});
      }

      if (start_args.program()->entries() == std::nullopt) {
        start_args.program().value().entries(std::vector<fuchsia_data::DictionaryEntry>{});
      }

      start_args.program()->entries()->push_back(fuchsia_data::DictionaryEntry(
          "service_connect_validation", std::make_unique<fuchsia_data::DictionaryValue>(
                                            fuchsia_data::DictionaryValue::WithStr("true"))));
    }
  }

  fdf_testing::DriverRuntime runtime_;
  fdf::UnownedSynchronizedDispatcher env_dispatcher_;

  async_patterns::TestDispatcherBound<EnvWrapper<EnvironmentType>> env_wrapper_;

  fidl::ClientEnd<fuchsia_io::Directory> outgoing_directory_client_;
  std::optional<zx::result<>> start_result_;
  std::optional<zx::result<>> prepare_stop_result_;
#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
  PowerState power_state_ = PowerState::kResumed;
#endif

  bool enable_service_connect_validation_ = false;
};

}  // namespace internal

// Background driver tests have the driver-under-test executing on a background driver dispatcher.
// This allows for tests to use sync FIDL clients directly from their main test thread.
// This is good for unit tests that more heavily exercise the driver-under-test through its exposed
// FIDL services, rather than its public methods.
//
// The test can run tasks on the driver context using the |RunInDriverContext()| methods, but sync
// client tasks can be run directly on the main test thread.
template <typename Configuration>
class BackgroundDriverTest final : public internal::DriverTestCommon<Configuration> {
  using DriverType = typename Configuration::DriverType;

 public:
  ~BackgroundDriverTest() { this->ShutdownAndDestroyDriver(); }

  // Runs a task on the dispatcher context of the driver under test. This will be a different
  // thread than the main test thread, so be careful when capturing and returning pointers to
  // objects that live on different dispatchers like test fixture properties, or the environment.
  //
  // Returns the result of the given task once it has completed.
  template <typename T>
  T RunInDriverContext(fit::callback<T(DriverType&)> task) {
    ZX_ASSERT_MSG(dut_.has_value(),
                  "Cannot call RunInDriverContext after |ShutdownAndDestroyDriver|.");
    return dut_->SyncCall([driver_task = std::move(task)](
                              fdf_testing::internal::DriverUnderTest<DriverType>* dut_ptr) mutable {
      return driver_task(***dut_ptr);
    });
  }

  // Runs a task on the dispatcher context of the driver under test. This will be a different
  // thread than the main test thread, so be careful when capturing and returning pointers to
  // objects that live on different dispatchers like test fixture properties, or the environment.
  //
  // Returns when the given task has completed.
  void RunInDriverContext(fit::callback<void(DriverType&)> task) {
    ZX_ASSERT_MSG(dut_.has_value(),
                  "Cannot call RunInDriverContext after |ShutdownAndDestroyDriver|.");
    dut_->SyncCall([driver_task = std::move(task)](
                       fdf_testing::internal::DriverUnderTest<DriverType>* dut_ptr) mutable {
      driver_task(***dut_ptr);
    });
  }

 private:
  zx::result<> StartDriverInner(fuchsia_driver_framework::DriverStartArgs start_args) override {
    DriverRegistration symbol;
    if constexpr (internal::HasGetDriverRegistration<DriverType>) {
      symbol = DriverType::GetDriverRegistration();
    } else {
      symbol = __fuchsia_driver_registration__;
    }

    dut_dispatcher_ = this->runtime().StartBackgroundDispatcher();
#if FUCHSIA_API_LEVEL_AT_LEAST(32)
    async_dispatcher_t* dut_async_dispatcher =
        dut_dispatcher_->GetAlwaysOnDispatcher()->async_dispatcher();
#else
    async_dispatcher_t* dut_async_dispatcher = dut_dispatcher_->async_dispatcher();
#endif
    dut_.emplace(dut_async_dispatcher, std::in_place, symbol);

    return this->runtime().RunToCompletion(dut_->SyncCall(
        &fdf_testing::internal::DriverUnderTest<DriverType>::Start, std::move(start_args)));
  }

  zx::result<> StopDriverInner() override {
    return this->runtime().RunToCompletion(
        dut_->SyncCall(&fdf_testing::internal::DriverUnderTest<DriverType>::PrepareStop));
  }

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
  zx::result<> SuspendDriverInner() override {
    return this->runtime().RunToCompletion(
        dut_->SyncCall(&fdf_testing::internal::DriverUnderTest<DriverType>::Suspend));
  }

  zx::result<> ResumeDriverInner(
      std::optional<fuchsia_power_broker::LeaseToken> power_element_lease) override {
    return this->runtime().RunToCompletion(
        dut_->SyncCall(&fdf_testing::internal::DriverUnderTest<DriverType>::Resume,
                       std::move(power_element_lease)));
  }
#endif

  bool DriverExists() override { return dut_.has_value(); }

  void ShutdownAndDestroyDriverInner() override {
    this->runtime().ShutdownBackgroundDispatcher(dut_dispatcher_->get(),
                                                 [this]() { dut_.reset(); });
  }

  zx::result<fit::result<fuchsia_driver_framework::HealthCheckError,
                         fuchsia_driver_framework::HealthCheckResponse>>
  CheckHealthInner(fuchsia_driver_framework::HealthCheckRequest request) override {
    return this->runtime().RunToCompletion(
        dut_->SyncCall(&fdf_testing::internal::DriverUnderTest<DriverType>::CheckHealth, request));
  }

  std::vector<fuchsia_driver_framework::HealthAlert> TakeHealthAlertsInner() override {
    DrainPendingHealthAlerts();
    return dut_->SyncCall(&fdf_testing::internal::DriverUnderTest<DriverType>::TakeHealthAlerts);
  }

  size_t PendingHealthAlertCountInner() override {
    DrainPendingHealthAlerts();
    return dut_->SyncCall(
        &fdf_testing::internal::DriverUnderTest<DriverType>::PendingHealthAlertCount);
  }

  // Pumps dut_dispatcher_ until no more `OnHealthAlert` events arrive. Each SyncCall barriers
  // behind the work already queued on dut_dispatcher_ (tasks that call ReportHealthAlert, and the
  // channel read that delivers the resulting FIDL event). The driver transport reads one message
  // per channel-read callback and only then re-arms the wait, so when several alerts are in flight
  // a single barrier would observe only the first one; iterate until the count stops changing.
  void DrainPendingHealthAlerts() {
    // A driver that emits alerts continuously would never let the count settle; bound the pump so
    // such a test fails loudly instead of hanging.
    constexpr size_t kMaxDrainIterations = 1000;
    size_t prev = dut_->SyncCall(
        &fdf_testing::internal::DriverUnderTest<DriverType>::PendingHealthAlertCount);
    for (size_t i = 0; i < kMaxDrainIterations; ++i) {
      size_t curr = dut_->SyncCall(
          &fdf_testing::internal::DriverUnderTest<DriverType>::PendingHealthAlertCount);
      if (curr == prev) {
        return;
      }
      prev = curr;
    }
    ZX_PANIC(
        "Health alerts kept arriving for %zu dispatcher rounds; is the driver emitting them "
        "continuously?",
        kMaxDrainIterations);
  }

  fdf::UnownedSynchronizedDispatcher dut_dispatcher_;
  std::optional<
      async_patterns::TestDispatcherBound<fdf_testing::internal::DriverUnderTest<DriverType>>>
      dut_;
};

// Foreground driver tests have the driver-under-test executing on the foreground (main) test
// thread. This allows for the test to directly reach into the driver to call methods on it.
// This is good for unit tests that more heavily test a driver through its public methods,
// rather than its exposed FIDL services.
//
// The test can access the driver under test using the |driver()| method and directly make calls
// into it, but sync client tasks must go through |RunOnBackgroundDispatcherSync()|.
template <typename Configuration>
class ForegroundDriverTest final : public internal::DriverTestCommon<Configuration> {
  using DriverType = typename Configuration::DriverType;

 public:
  ~ForegroundDriverTest() { this->ShutdownAndDestroyDriver(); }

  // Runs a task in a background context while running the foreground driver. This must be used
  // if calling synchronously into the driver (eg. a fidl call through a SyncClient).
  //
  // Returns the result of the given task once it has completed.
  template <typename T>
  zx::result<T> RunOnBackgroundDispatcherSync(fit::callback<T()> task) {
    if (!bg_task_dispatcher_.has_value()) {
      bg_task_dispatcher_.emplace(this->runtime().StartBackgroundDispatcher());
    }

    libsync::Completion completion;
    std::optional<T> result_container;
    zx_status_t status =
        async::PostTask(bg_task_dispatcher_.value()->async_dispatcher(), [&]() mutable {
          result_container.emplace(task());
          completion.Signal();
        });

    if (status != ZX_OK) {
      return zx::error(status);
    }

    while (!completion.signaled()) {
      this->runtime().RunUntilIdle();
    }

    return zx::ok(std::move(result_container.value()));
  }

  // Runs a task in a background context while running the foreground driver. This must be used
  // if calling synchronously into the driver (eg. a fidl call through a SyncClient).
  //
  // Returns when the given task has completed.
  zx::result<> RunOnBackgroundDispatcherSync(fit::callback<void()> task) {
    if (!bg_task_dispatcher_.has_value()) {
      bg_task_dispatcher_.emplace(this->runtime().StartBackgroundDispatcher());
    }

    libsync::Completion completion;
    zx_status_t status =
        async::PostTask(bg_task_dispatcher_.value()->async_dispatcher(), [&]() mutable {
          task();
          completion.Signal();
        });

    if (status != ZX_OK) {
      return zx::error(status);
    }

    while (!completion.signaled()) {
      this->runtime().RunUntilIdle();
    }

    return zx::ok();
  }

  // Access the driver under test. Can only be called while the driver is active, after
  // |StartDriver| has been called, but before |ShutdownAndDestroyDriver|.
  DriverType* driver() {
    ZX_ASSERT_MSG(dut_.has_value(), "Cannot call |driver| after |ShutdownAndDestroyDriver|.");
    return *dut_.value();
  }

 private:
  zx::result<> StartDriverInner(fuchsia_driver_framework::DriverStartArgs start_args) override {
    DriverRegistration symbol;
    if constexpr (internal::HasGetDriverRegistration<DriverType>) {
      symbol = DriverType::GetDriverRegistration();
    } else {
      symbol = __fuchsia_driver_registration__;
    }

    dut_.emplace(symbol);
    return this->runtime().RunToCompletion(dut_->Start(std::move(start_args)));
  }

  zx::result<> StopDriverInner() override {
    return this->runtime().RunToCompletion(dut_->PrepareStop());
  }

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
  zx::result<> SuspendDriverInner() override {
    return this->runtime().RunToCompletion(dut_->Suspend());
  }

  zx::result<> ResumeDriverInner(
      std::optional<fuchsia_power_broker::LeaseToken> power_element_lease) override {
    return this->runtime().RunToCompletion(dut_->Resume(std::move(power_element_lease)));
  }
#endif

  bool DriverExists() override { return dut_.has_value(); }
  void ShutdownAndDestroyDriverInner() override {
    this->runtime().ResetForegroundDispatcher([this]() { dut_.reset(); });
  }

  zx::result<fit::result<fuchsia_driver_framework::HealthCheckError,
                         fuchsia_driver_framework::HealthCheckResponse>>
  CheckHealthInner(fuchsia_driver_framework::HealthCheckRequest request) override {
    return this->runtime().RunToCompletion(dut_->CheckHealth(request));
  }

  std::vector<fuchsia_driver_framework::HealthAlert> TakeHealthAlertsInner() override {
    this->runtime().RunUntilIdle();
    return dut_->TakeHealthAlerts();
  }

  size_t PendingHealthAlertCountInner() override {
    this->runtime().RunUntilIdle();
    return dut_->PendingHealthAlertCount();
  }

  std::optional<fdf::UnownedSynchronizedDispatcher> bg_task_dispatcher_;
  std::optional<fdf_testing::internal::DriverUnderTest<DriverType>> dut_;
};

}  // namespace fdf_testing

#endif  // LIB_DRIVER_TESTING_CPP_DRIVER_TEST_H_
