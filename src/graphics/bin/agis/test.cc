// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <arpa/inet.h>
#include <fidl/fuchsia.gpu.agis/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/process.h>
#include <lib/zxio/zxio.h>

#include <thread>

#include <gtest/gtest.h>
namespace {
std::atomic<int> outstanding = 0;

zx_koid_t ProcessKoid() {
  zx::unowned<zx::process> process = zx::process::self();
  zx_info_handle_basic_t info;
  zx_status_t status = zx_object_get_info(process->get(), ZX_INFO_HANDLE_BASIC, &info, sizeof(info),
                                          nullptr /* actual */, nullptr /* avail */);
  EXPECT_EQ(status, ZX_OK);
  return info.koid;
}

std::string ProcessName() {
  zx::unowned<zx::process> process = zx::process::self();
  char process_name[ZX_MAX_NAME_LEN];
  process->get_property(ZX_PROP_NAME, process_name, sizeof(process_name));
  return process_name;
}

uint64_t TimeMS() {
  return std::chrono::duration_cast<std::chrono::milliseconds>(
             std::chrono::system_clock::now().time_since_epoch())
      .count();
}

// Event handler that logs and quits |loop| if the connection errors out, and fails the test
// when |fail_test| is set.
template <typename Protocol>
class ErrorHandler : public fidl::AsyncEventHandler<Protocol> {
 public:
  ErrorHandler(const char *name, async::Loop *loop, bool fail_test)
      : name_(name), loop_(loop), fail_test_(fail_test) {}

  void on_fidl_error(fidl::UnbindInfo info) override {
    FX_LOG_KV(ERROR, "agis-test: error handler", FX_KV("client", name_),
              FX_KV("status", info.status()));
    if (loop_) {
      loop_->Quit();
    }
    if (fail_test_) {
      ADD_FAILURE() << name_ << " disconnected: " << info;
    }
  }

 private:
  const char *name_;
  async::Loop *loop_;
  bool fail_test_;
};

// Connects |client| to |Protocol| in the test's namespace, dispatching on |dispatcher|.
template <typename Protocol>
void Connect(fidl::Client<Protocol> &client, async_dispatcher_t *dispatcher,
             fidl::AsyncEventHandler<Protocol> *event_handler) {
  zx::result client_end = component::Connect<Protocol>();
  ASSERT_TRUE(client_end.is_ok()) << client_end.status_string();
  client.Bind(std::move(*client_end), dispatcher, event_handler);
}

// Returns the domain error carried by |result|, failing the test if there is none.
template <typename Method>
std::optional<fuchsia_gpu_agis::Error> DomainError(const fidl::Result<Method> &result) {
  if (result.is_ok()) {
    ADD_FAILURE() << "expected an error, got success";
    return std::nullopt;
  }
  if (!result.error_value().is_domain_error()) {
    ADD_FAILURE() << "expected a domain error, got " << result.error_value();
    return std::nullopt;
  }
  return result.error_value().domain_error();
}

}  // namespace

class AgisTest : public testing::Test {
 protected:
  void SetUp() override {
    num_vtcs_ = 0;
    loop_ = std::make_unique<async::Loop>(&kAsyncLoopConfigAttachToCurrentThread);

    component_registry_handler_.emplace("component_registry", loop_.get(), /*fail_test=*/true);
    Connect(component_registry_, loop_->dispatcher(), &*component_registry_handler_);

    observer_handler_.emplace("observer", loop_.get(), /*fail_test=*/true);
    Connect(observer_, loop_->dispatcher(), &*observer_handler_);

    connector_handler_.emplace("connector", loop_.get(), /*fail_test=*/true);
    Connect(connector_, loop_->dispatcher(), &*connector_handler_);

    process_koid_ = ProcessKoid();
    process_name_ = ProcessName();
    client_id_ = TimeMS();
  }

  void TearDown() override {
    if (loop_) {
      LoopWait();
      loop_->Quit();
    }
  }

  // LoopWait() guarantees that the callback supplied to any interface in the agis protocol
  // library has been called and completed.  The callbacks compute / verify return results
  // from the protocol methods.
  void LoopWait(const uint64_t timeout_secs = 0) {
    uint64_t elapsed_millis = 0;
    const uint64_t timeout_millis = timeout_secs * 1000;
    while (outstanding && (timeout_secs == 0 || elapsed_millis < timeout_millis)) {
      loop_->RunUntilIdle();
      std::this_thread::sleep_for(std::chrono::milliseconds(50));
      elapsed_millis += 50;
    }
  }

  void Register(uint64_t id, zx_koid_t process_koid, std::string process_name) {
    outstanding++;
    component_registry_
        ->Register(
            {{.id = id, .process_koid = process_koid, .process_name = std::move(process_name)}})
        .Then([&](fidl::Result<fuchsia_gpu_agis::ComponentRegistry::Register> &result) {
          EXPECT_TRUE(result.is_ok()) << result.error_value();
          outstanding--;
        });
    LoopWait();
  }

  void Unregister(uint64_t client_id) {
    outstanding++;
    component_registry_->Unregister({{.id = client_id}})
        .Then([&](fidl::Result<fuchsia_gpu_agis::ComponentRegistry::Unregister> &result) {
          EXPECT_TRUE(result.is_ok()) << result.error_value();
          outstanding--;
        });
  }

  void Vtcs() {
    outstanding++;
    observer_->Vtcs().Then([&](fidl::Result<fuchsia_gpu_agis::Observer::Vtcs> &result) {
      EXPECT_TRUE(result.is_ok()) << result.error_value();
      num_vtcs_ = result.is_ok() ? result->vtcs().size() : 0;
      outstanding--;
    });
  }

  //
  // Ivars.
  //
  std::unique_ptr<async::Loop> loop_;
  std::optional<ErrorHandler<fuchsia_gpu_agis::ComponentRegistry>> component_registry_handler_;
  std::optional<ErrorHandler<fuchsia_gpu_agis::Observer>> observer_handler_;
  std::optional<ErrorHandler<fuchsia_gpu_agis::Connector>> connector_handler_;
  fidl::Client<fuchsia_gpu_agis::ComponentRegistry> component_registry_;
  fidl::Client<fuchsia_gpu_agis::Observer> observer_;
  fidl::Client<fuchsia_gpu_agis::Connector> connector_;
  size_t num_vtcs_;
  zx_koid_t process_koid_;
  std::string process_name_;
  uint64_t client_id_;
};

// Test register.
TEST_F(AgisTest, Register) {
  Register(client_id_, process_koid_, process_name_);
  outstanding++;
  component_registry_
      ->Register({{.id = client_id_, .process_koid = process_koid_, .process_name = process_name_}})
      .Then([&](fidl::Result<fuchsia_gpu_agis::ComponentRegistry::Register> &result) {
        EXPECT_EQ(DomainError(result), fuchsia_gpu_agis::Error::kAlreadyRegistered);
        outstanding--;
      });
  LoopWait();
  Unregister(client_id_);
  LoopWait();
}

// Test unregister.
TEST_F(AgisTest, Unregister) {
  Register(client_id_, process_koid_, process_name_);
  Unregister(client_id_);
  LoopWait();
  outstanding++;
  component_registry_->Unregister({{.id = client_id_}})
      .Then([&](fidl::Result<fuchsia_gpu_agis::ComponentRegistry::Unregister> &result) {
        EXPECT_TRUE(result.is_error());
        EXPECT_EQ(DomainError(result), fuchsia_gpu_agis::Error::kNotFound);
        outstanding--;
      });
  LoopWait();
}

// Test vtc list cardinality.
TEST_F(AgisTest, Vtcs) {
  Register(client_id_, process_koid_, process_name_);
  Register(client_id_ + 1, process_koid_, process_name_ + "+1");
  Vtcs();
  LoopWait();
  EXPECT_EQ(num_vtcs_, 2ul);
  Unregister(client_id_);
  Unregister(client_id_ + 1);
  LoopWait();
  Vtcs();
  LoopWait();
  EXPECT_EQ(num_vtcs_, 0ul);
}

// Test registry overflow.
TEST_F(AgisTest, MaxVtcs) {
  uint32_t i = 0;
  for (i = 0; i < fuchsia_gpu_agis::kMaxVtcs; i++) {
    Register(client_id_ + i, process_koid_, process_name_ + "+" + std::to_string(i));
  }
  outstanding++;
  component_registry_
      ->Register({{.id = client_id_ + i,
                   .process_koid = process_koid_,
                   .process_name = process_name_ + "+" + std::to_string(i)}})
      .Then([&](fidl::Result<fuchsia_gpu_agis::ComponentRegistry::Register> &result) {
        EXPECT_EQ(DomainError(result), fuchsia_gpu_agis::Error::kVtcsExceeded);
        outstanding--;
      });
  LoopWait();
  for (i = 0; i < fuchsia_gpu_agis::kMaxVtcs; i++) {
    Unregister(client_id_ + i);
  }
  LoopWait();
}

// Validate retrieved socket.
TEST_F(AgisTest, UsableSocket) {
  outstanding++;
  component_registry_
      ->Register({{.id = client_id_, .process_koid = process_koid_, .process_name = process_name_}})
      .Then([&](fidl::Result<fuchsia_gpu_agis::ComponentRegistry::Register> &result) {
        EXPECT_TRUE(result.is_ok()) << result.error_value();
        outstanding--;
      });
  LoopWait();

  // Issue hanging get for the vulkan-end socket.  Notice there is no following LoopWait() call.
  zx::socket vulkan_socket;
  component_registry_->GetVulkanSocket({{.id = client_id_}})
      .Then([&](fidl::Result<fuchsia_gpu_agis::ComponentRegistry::GetVulkanSocket> &result) {
        EXPECT_TRUE(result.is_ok()) << result.error_value();
        if (result.is_ok()) {
          vulkan_socket = std::move(result->socket());
          EXPECT_TRUE(vulkan_socket.is_valid());
        }
      });
  std::this_thread::sleep_for(std::chrono::milliseconds(50));
  loop_->RunUntilIdle();
  EXPECT_FALSE(vulkan_socket.is_valid());

  uint32_t global_id = 0;
  std::string process_name;
  zx_koid_t process_koid = 0;
  outstanding++;
  observer_->Vtcs().Then([&](fidl::Result<fuchsia_gpu_agis::Observer::Vtcs> &result) {
    EXPECT_TRUE(result.is_ok()) << result.error_value();
    if (result.is_ok()) {
      const std::vector<fuchsia_gpu_agis::Vtc> &vtcs = result->vtcs();
      EXPECT_EQ(vtcs.size(), 1ul);
      if (!vtcs.empty()) {
        global_id = vtcs.front().global_id().value();
        process_koid = vtcs.front().process_koid().value();
        process_name = vtcs.front().process_name().value();
      }
    }
    outstanding--;
  });
  LoopWait();
  EXPECT_EQ(process_name, process_name_);
  EXPECT_EQ(process_koid, process_koid_);
  EXPECT_NE(global_id, 0u);

  // Explicitly retrieve the ffx-end socket and implicitly satisfy the hanging GetVulkanSocket.
  outstanding++;
  zx::socket ffx_socket;
  connector_->GetSocket({{.global_id = global_id}})
      .Then([&](fidl::Result<fuchsia_gpu_agis::Connector::GetSocket> &result) {
        EXPECT_TRUE(result.is_ok()) << result.error_value();
        if (result.is_ok()) {
          ffx_socket = std::move(result->socket());
        }
        outstanding--;
      });

  while (!vulkan_socket.is_valid() || !ffx_socket.is_valid()) {
    LoopWait(30);
  }

  // Send the message from the vulkan end.
  const char message[] = "AGIS Server Message";
  size_t actual = 0;
  zx_status_t status = vulkan_socket.write(0u, message, sizeof(message), &actual);
  ASSERT_EQ(status, ZX_OK);
  EXPECT_EQ(actual, sizeof(message));

  // Read the message from the ffx end.
  char buffer[sizeof(message)];
  actual = 0;
  status = ffx_socket.read(0u, &buffer, sizeof(buffer), &actual);
  ASSERT_EQ(status, ZX_OK);
  EXPECT_EQ(actual, sizeof(message));
  EXPECT_EQ(strcmp(message, buffer), 0);
}

// Test multiple retrievals of the vulkan socket from the same registration.
TEST_F(AgisTest, Reget) {
  Register(client_id_, process_koid_, process_name_);

  // Issue hanging get for the vulkan-end socket.  Notice there is no following LoopWait() call.
  zx::socket vulkan_socket;
  component_registry_->GetVulkanSocket({{.id = client_id_}})
      .Then([&](fidl::Result<fuchsia_gpu_agis::ComponentRegistry::GetVulkanSocket> &result) {
        EXPECT_TRUE(result.is_ok()) << result.error_value();
        if (result.is_ok()) {
          vulkan_socket = std::move(result->socket());
        }
      });
  std::this_thread::sleep_for(std::chrono::milliseconds(50));
  loop_->RunUntilIdle();
  EXPECT_FALSE(vulkan_socket.is_valid());

  outstanding++;
  uint32_t global_id = 0;
  std::string process_name;
  zx_koid_t process_koid = 0;
  observer_->Vtcs().Then([&](fidl::Result<fuchsia_gpu_agis::Observer::Vtcs> &result) {
    EXPECT_TRUE(result.is_ok()) << result.error_value();
    if (result.is_ok()) {
      const std::vector<fuchsia_gpu_agis::Vtc> &vtcs = result->vtcs();
      EXPECT_EQ(vtcs.size(), 1ul);
      if (!vtcs.empty()) {
        global_id = vtcs.front().global_id().value();
        process_name = vtcs.front().process_name().value();
        process_koid = vtcs.front().process_koid().value();
      }
    }
    outstanding--;
  });
  LoopWait();

  // Explicitly retrieve the ffx-end socket and implicitly satisfy the hanging GetVulkanSocket.
  outstanding++;
  zx::socket ffx_socket;
  connector_->GetSocket({{.global_id = global_id}})
      .Then([&](fidl::Result<fuchsia_gpu_agis::Connector::GetSocket> &result) {
        EXPECT_TRUE(result.is_ok()) << result.error_value();
        if (result.is_ok()) {
          ffx_socket = std::move(result->socket());
        }
        outstanding--;
      });

  while (!vulkan_socket.is_valid() || !ffx_socket.is_valid()) {
    LoopWait(30);
  }

  // Re-get the socket.
  vulkan_socket.reset();
  component_registry_->GetVulkanSocket({{.id = client_id_}})
      .Then([&](fidl::Result<fuchsia_gpu_agis::ComponentRegistry::GetVulkanSocket> &result) {
        EXPECT_TRUE(result.is_ok()) << result.error_value();
        if (result.is_ok()) {
          vulkan_socket = std::move(result->socket());
        }
      });

  outstanding++;
  connector_->GetSocket({{.global_id = global_id}})
      .Then([&](fidl::Result<fuchsia_gpu_agis::Connector::GetSocket> &result) {
        EXPECT_TRUE(result.is_ok()) << result.error_value();
        if (result.is_ok()) {
          ffx_socket = std::move(result->socket());
        }
        outstanding--;
      });

  while (!vulkan_socket.is_valid() || !ffx_socket.is_valid()) {
    LoopWait(30);
  }

  // Send the message from the vulkan end.
  const char message[] = "AGIS Server Message";
  size_t actual = 0;
  auto status = vulkan_socket.write(0u, message, sizeof(message), &actual);
  ASSERT_EQ(status, ZX_OK);
  EXPECT_EQ(actual, sizeof(message));

  // Read the message from the ffx end.
  char buffer[sizeof(message)];
  actual = 0;
  status = ffx_socket.read(0u, &buffer, sizeof(buffer), &actual);
  ASSERT_EQ(status, ZX_OK);
  EXPECT_EQ(actual, sizeof(message));
  EXPECT_EQ(strcmp(message, buffer), 0);

  Unregister(client_id_);
  LoopWait();
}

// Test GetSocket followed by GetVulkanSocket retrieval.
TEST_F(AgisTest, ReverseGet) {
  Register(client_id_, process_koid_, process_name_);

  outstanding++;
  uint32_t global_id = 0;
  std::string process_name;
  zx_koid_t process_koid = 0;
  observer_->Vtcs().Then([&](fidl::Result<fuchsia_gpu_agis::Observer::Vtcs> &result) {
    EXPECT_TRUE(result.is_ok()) << result.error_value();
    if (result.is_ok()) {
      const std::vector<fuchsia_gpu_agis::Vtc> &vtcs = result->vtcs();
      EXPECT_EQ(vtcs.size(), 1ul);
      if (!vtcs.empty()) {
        global_id = vtcs.front().global_id().value();
        process_name = vtcs.front().process_name().value();
        process_koid = vtcs.front().process_koid().value();
      }
    }
    outstanding--;
  });
  LoopWait();

  // Retrieve the ffx socket end first.
  outstanding++;
  zx::socket ffx_socket;
  connector_->GetSocket({{.global_id = global_id}})
      .Then([&](fidl::Result<fuchsia_gpu_agis::Connector::GetSocket> &result) {
        EXPECT_TRUE(result.is_ok()) << result.error_value();
        if (result.is_ok()) {
          ffx_socket = std::move(result->socket());
        }
        outstanding--;
      });

  // Retrieve the vulkan socket end second.
  outstanding++;
  zx::socket vulkan_socket;
  component_registry_->GetVulkanSocket({{.id = client_id_}})
      .Then([&](fidl::Result<fuchsia_gpu_agis::ComponentRegistry::GetVulkanSocket> &result) {
        EXPECT_TRUE(result.is_ok()) << result.error_value();
        if (result.is_ok()) {
          vulkan_socket = std::move(result->socket());
        }
        outstanding--;
      });

  while (!vulkan_socket.is_valid() || !ffx_socket.is_valid()) {
    LoopWait(30);
  }

  // Send the message from the vulkan end.
  const char message[] = "AGIS Server Message";
  size_t actual = 0;
  auto status = vulkan_socket.write(0u, message, sizeof(message), &actual);
  ASSERT_EQ(status, ZX_OK);
  EXPECT_EQ(actual, sizeof(message));

  // Read the message from the ffx end.
  char buffer[sizeof(message)];
  actual = 0;
  status = ffx_socket.read(0u, &buffer, sizeof(buffer), &actual);
  ASSERT_EQ(status, ZX_OK);
  EXPECT_EQ(actual, sizeof(message));
  EXPECT_EQ(strcmp(message, buffer), 0);

  Unregister(client_id_);
  LoopWait();
}

// Test unexpected disconnects / shutdowns.
TEST(AgisDisconnect, Main) {
  zx_koid_t process_koid = ProcessKoid();
  std::string process_name = ProcessName();
  uint64_t client_id = TimeMS();
  bool disconnect_outstanding = false;
  auto loop = std::make_unique<async::Loop>(&kAsyncLoopConfigAttachToCurrentThread);
  auto loop_wait = [&disconnect_outstanding, &loop]() {
    while (disconnect_outstanding) {
      EXPECT_EQ(loop->RunUntilIdle(), ZX_OK);
      std::this_thread::sleep_for(std::chrono::milliseconds(50));
    }
  };

  // Create a component_registry, register |client_id| and verify its presence.
  {
    ErrorHandler<fuchsia_gpu_agis::ComponentRegistry> component_registry_handler(
        "Register Disconnect ComponentRegistry", loop.get(), /*fail_test=*/false);
    fidl::Client<fuchsia_gpu_agis::ComponentRegistry> component_registry;
    Connect(component_registry, loop->dispatcher(), &component_registry_handler);

    disconnect_outstanding = true;
    component_registry
        ->Register({{.id = client_id, .process_koid = process_koid, .process_name = process_name}})
        .Then([&](fidl::Result<fuchsia_gpu_agis::ComponentRegistry::Register> &result) {
          EXPECT_TRUE(result.is_ok()) << result.error_value();
          disconnect_outstanding = false;
        });
    loop_wait();

    ErrorHandler<fuchsia_gpu_agis::Observer> observer_handler("Register Disconnect Observer",
                                                              loop.get(), /*fail_test=*/false);
    fidl::Client<fuchsia_gpu_agis::Observer> observer;
    Connect(observer, loop->dispatcher(), &observer_handler);
    disconnect_outstanding = true;
    observer->Vtcs().Then([&](fidl::Result<fuchsia_gpu_agis::Observer::Vtcs> &result) {
      EXPECT_TRUE(result.is_ok()) << result.error_value();
      if (result.is_ok()) {
        const std::vector<fuchsia_gpu_agis::Vtc> &vtcs = result->vtcs();
        EXPECT_EQ(vtcs.size(), 1ul);
        bool found = false;
        for (const auto &vtc : vtcs) {
          if (vtc.process_koid() == process_koid) {
            EXPECT_EQ(vtc.process_name(), process_name);
            found = true;
            break;
          }
        }
        EXPECT_TRUE(found);
      }
      disconnect_outstanding = false;
    });
    loop_wait();
  }

  // Create a new observer and verify that |process_koid| is no longer registered.
  ErrorHandler<fuchsia_gpu_agis::Observer> observer_handler("Verify Disconnect Observer",
                                                            loop.get(), /*fail_test=*/false);
  fidl::Client<fuchsia_gpu_agis::Observer> observer;
  Connect(observer, loop->dispatcher(), &observer_handler);

  bool found = true;
  while (found) {
    disconnect_outstanding = true;
    observer->Vtcs().Then([&](fidl::Result<fuchsia_gpu_agis::Observer::Vtcs> &result) {
      EXPECT_TRUE(result.is_ok()) << result.error_value();
      bool component_found = false;
      if (result.is_ok()) {
        for (const auto &vtc : result->vtcs()) {
          if (vtc.process_koid() == process_koid) {
            EXPECT_EQ(vtc.process_name(), process_name);
            component_found = true;
            break;
          }
        }
      }
      found = component_found;
      disconnect_outstanding = false;
    });
    loop_wait();
  }

  // Self-documenting no-op.
  EXPECT_FALSE(found);

  loop->Quit();
}

int main(int argc, char **argv) {
  ::testing::InitGoogleTest(&argc, argv);
  fuchsia_logging::LogSettingsBuilder builder;
  builder.WithTags({"agis-test"}).BuildAndInitialize();
  return RUN_ALL_TESTS();
}
