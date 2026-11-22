// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/display/coordinator_proxy/coordinator_proxy.h"

#include <fidl/fuchsia.hardware.display.types/cpp/fidl.h>
#include <fidl/fuchsia.hardware.display/cpp/fidl.h>
#include <fidl/fuchsia.math/cpp/fidl.h>
#include <fidl/fuchsia.sysmem2/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async/cpp/task.h>
#include <lib/fit/defer.h>
#include <lib/inspect/cpp/hierarchy.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/inspect/cpp/reader.h>
#include <lib/sync/cpp/completion.h>
#include <lib/zx/event.h>
#include <lib/zx/time.h>

#include <memory>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

#include <gtest/gtest.h>

#include "src/lib/testing/loop_fixture/real_loop_fixture.h"
#include "src/ui/scenic/lib/display/tests/mock_display_coordinator.h"
#include "src/ui/scenic/lib/utils/helpers.h"

namespace display::test {

class CoordinatorProxyTest : public gtest::RealLoopFixture {
 public:
  static constexpr uint32_t kMaxDisplayLayersCount = 2;

  static constexpr std::array<WireDisplayMode, 2> kSupportedDisplayModes = {
      fuchsia_hardware_display_types::wire::Mode{.active_area = {.width = 1024, .height = 768},
                                                 .refresh_rate_millihertz = 60000,
                                                 .flags = {}},
      fuchsia_hardware_display_types::wire::Mode{.active_area = {.width = 1920, .height = 1080},
                                                 .refresh_rate_millihertz = 60000,
                                                 .flags = {}}};

  // Used to construct the default `mock_coordinator_` that is available in all tests.  It is also
  // available to tests that need to create their own mock coordinator (and `CoordinatorProxy`).
  std::pair<std::unique_ptr<MockDisplayCoordinator>, std::unique_ptr<CoordinatorProxy>>
  MakeMockDisplayCoordinatorAndCoordinatorProxy(
      CoordinatorProxy::CheckConfigHeuristics check_config_heuristics = {},
      async_dispatcher_t* client_dispatcher = nullptr,
      inspect::Node inspect_node = inspect::Node()) {
    zx::result endpoints_result = fidl::CreateEndpoints<fuchsia_hardware_display::Coordinator>();
    FX_CHECK(endpoints_result.is_ok())
        << "Failed to create FIDL endpoints for the display coordinator: "
        << endpoints_result.status_string();
    auto [coordinator_client, coordinator_server] = std::move(endpoints_result).value();

    fuchsia_hardware_display::wire::Info display_info = {
        .modes = fidl::VectorView<fuchsia_hardware_display_types::wire::Mode>::FromExternal(
            const_cast<WireDisplayMode*>(kSupportedDisplayModes.data()),
            kSupportedDisplayModes.size()),
        .max_layer_count = kMaxDisplayLayersCount,
    };

    auto mock_coordinator = std::make_unique<MockDisplayCoordinator>(display_info);
    // The fidl::Server requires the binding and teardown to occur on the
    // same thread where the FIDL server runs.
    libsync::Completion completion;
    async::PostTask(display_coordinator_loop_.dispatcher(),
                    [this, &mock_coordinator, &completion,
                     coordinator_server = std::move(coordinator_server)]() mutable {
                      mock_coordinator->Bind(std::move(coordinator_server), {},
                                             display_coordinator_loop_.dispatcher());
                      completion.Signal();
                    });
    completion.Wait();

    auto coordinator_proxy = std::make_unique<CoordinatorProxy>(
        std::move(coordinator_client), client_dispatcher ? client_dispatcher : dispatcher(),
        check_config_heuristics, std::move(inspect_node));

    return std::pair{std::move(mock_coordinator), std::move(coordinator_proxy)};
  }

  // RAII helper for async tests that bind `CoordinatorProxy` to a separate reply loop, ensuring
  // `mock` is always destroyed on `display_coordinator_loop_` even when an `ASSERT_*` returns
  // early, and tolerating `proxy` already having been reset by the test.
  struct AsyncTestHarness {
    explicit AsyncTestHarness(CoordinatorProxyTest& fixture,
                              inspect::Node inspect_node = inspect::Node())
        : fixture(fixture), reply_loop(&kAsyncLoopConfigNeverAttachToThread) {
      auto [created_mock, created_proxy] = fixture.MakeMockDisplayCoordinatorAndCoordinatorProxy(
          {}, reply_loop.dispatcher(), std::move(inspect_node));
      mock = std::move(created_mock);
      proxy = std::move(created_proxy);
    }

    ~AsyncTestHarness() {
      proxy.reset();
      reply_loop.RunUntilIdle();
      libsync::Completion mock_destroyed;
      async::PostTask(fixture.display_coordinator_loop_.dispatcher(),
                      [mock = std::move(mock), &mock_destroyed]() mutable {
                        mock.reset();
                        mock_destroyed.Signal();
                      });
      mock_destroyed.Wait();
    }

    // Runs an in-flight request issuer while the server loop is blocked, then tears down the server
    // binding to send an epitaph, waits until server-side teardown has finished, and drains the
    // client reply loop. This guarantees that server binding teardown has finished and written the
    // epitaph before the client reads the channel.
    template <typename Fn>
    void RunWithInFlightPeerClose(Fn&& issue_requests) {
      libsync::Completion server_blocked;
      libsync::Completion unblock_server;
      libsync::Completion reset_called;
      libsync::Completion* server_blocked_ptr = &server_blocked;
      libsync::Completion* unblock_server_ptr = &unblock_server;
      libsync::Completion* reset_called_ptr = &reset_called;
      auto unblock_guard = fit::defer([&] {
        unblock_server.Signal();
        reset_called.Wait();
      });
      async::PostTask(
          fixture.display_coordinator_loop_.dispatcher(),
          [mock_ptr = mock.get(), server_blocked_ptr, unblock_server_ptr, reset_called_ptr] {
            server_blocked_ptr->Signal();
            unblock_server_ptr->Wait();
            mock_ptr->ResetCoordinatorBinding();
            reset_called_ptr->Signal();
          });
      server_blocked.Wait();

      std::forward<Fn>(issue_requests)();

      unblock_guard.call();

      libsync::Completion after_teardown;
      async::PostTask(fixture.display_coordinator_loop_.dispatcher(),
                      [after_teardown_ptr = &after_teardown] { after_teardown_ptr->Signal(); });
      after_teardown.Wait();

      reply_loop.RunUntilIdle();
    }

    AsyncTestHarness(const AsyncTestHarness&) = delete;
    AsyncTestHarness& operator=(const AsyncTestHarness&) = delete;

    CoordinatorProxyTest& fixture;
    async::Loop reply_loop;
    std::unique_ptr<MockDisplayCoordinator> mock;
    std::unique_ptr<CoordinatorProxy> proxy;
  };

  // Blocks `server_dispatcher` on a posted task until `Unblock()` (or destruction) and waits for
  // the server thread to finish reading `unblock_` before returning, preventing
  // `stack-use-after-scope` on `unblock_` when the caller's scope exits.
  struct ScopedServerBlock {
    explicit ScopedServerBlock(async_dispatcher_t* server_dispatcher) {
      libsync::Completion* blocked = &blocked_;
      libsync::Completion* unblock = &unblock_;
      libsync::Completion* unblocked = &unblocked_;
      async::PostTask(server_dispatcher, [blocked, unblock, unblocked] {
        blocked->Signal();
        unblock->Wait();
        unblocked->Signal();
      });
      blocked_.Wait();
    }
    ~ScopedServerBlock() { Unblock(); }

    void Unblock() {
      if (!unblocked_called_) {
        unblocked_called_ = true;
        unblock_.Signal();
        unblocked_.Wait();
      }
    }

    ScopedServerBlock(const ScopedServerBlock&) = delete;
    ScopedServerBlock& operator=(const ScopedServerBlock&) = delete;

   private:
    libsync::Completion blocked_;
    libsync::Completion unblock_;
    libsync::Completion unblocked_;
    bool unblocked_called_ = false;
  };

  void SetUp() override {
    gtest::RealLoopFixture::SetUp();
    sysmem_allocator_ = utils::CreateSysmemAllocatorClient(dispatcher(), "CoordinatorProxyTest");

    display_coordinator_loop_.StartThread("display-coordinator-loop");

    auto mock_and_proxy = MakeMockDisplayCoordinatorAndCoordinatorProxy();
    mock_coordinator_ = std::move(mock_and_proxy.first);
    coordinator_proxy_ = std::move(mock_and_proxy.second);
  }

  void TearDown() override {
    coordinator_proxy_.reset();

    // This is to make sure that the display coordinator loop has finished
    // handling all its pending tasks.
    RunLoopUntilIdle();
    libsync::Completion completion;
    async::PostTask(display_coordinator_loop_.dispatcher(), [this, &completion]() mutable {
      display_coordinator_loop_.RunUntilIdle();
      mock_coordinator_.reset();
      completion.Signal();
    });
    completion.Wait();
    display_coordinator_loop_.Quit();
    display_coordinator_loop_.JoinThreads();

    gtest::RealLoopFixture::TearDown();
  }

  // Synchronously calls `GetLatestCommittedConfigStamp()` on the display coordinator.  When we
  // receive the response from this, we know that the coordinator has also received/processed any
  // previously-sent messages.
  void PingMockDisplayCoordinator(CoordinatorProxy* proxy = nullptr) {
    auto _ =
        (proxy ? proxy : coordinator_proxy_.get())->raw().sync()->GetLatestCommittedConfigStamp();
  }

  void ConfigureDisplayedPrimaryLayer(const LayerId& layer_id) {
    const DisplayId kDisplayId(1);
    const Extent2 kExtent({.width = 100, .height = 100});
    const Rectangle kRect({.x = 0, .y = 0, .width = 100, .height = 100});

    coordinator_proxy_->SetLayerPrimaryConfig(layer_id, kExtent, /*image_tiling_type=*/0);
    coordinator_proxy_->SetLayerPrimaryPosition(layer_id, RotateFlip::kIdentity(), kRect, kRect);
    coordinator_proxy_->SetLayerPrimaryAlpha(layer_id, BlendMode::kReplace(), 1.0f);
    coordinator_proxy_->SetDisplayLayers(kDisplayId, std::span<const LayerId>(&layer_id, 1));
  }

 protected:
  static constexpr fuchsia_images2::PixelFormat kPixelFormat =
      fuchsia_images2::PixelFormat::kB8G8R8A8;

  async::Loop display_coordinator_loop_{&kAsyncLoopConfigNeverAttachToThread};

  std::unique_ptr<MockDisplayCoordinator> mock_coordinator_;
  std::unique_ptr<CoordinatorProxy> coordinator_proxy_;

  // Only for use on the main thread. Establish a new connection when on the MockDisplayCoordinator
  // thread.
  fidl::WireClient<fuchsia_sysmem2::Allocator> sysmem_allocator_;
};

// Test that CreateLayer() and DestroyLayer() immediately send the corresponding FIDL calls.
TEST_F(CoordinatorProxyTest, CreateAndDestroyLayer) {
  bool create_layer_called = false;
  mock_coordinator_->set_create_layer_fn([&](WireLayerId layer_id) { create_layer_called = true; });

  std::optional<LayerId> destroyed_layer_id;
  mock_coordinator_->set_destroy_layer_fn(
      [&](fuchsia_hardware_display::wire::CoordinatorDestroyLayerRequest* request) {
        destroyed_layer_id = LayerId(request->layer_id);
      });

  // CreateLayer should be called on the mock.
  const LayerId layer_id = coordinator_proxy_->CreateLayer();
  EXPECT_TRUE(create_layer_called);
  EXPECT_NE(layer_id, kInvalidLayerId);

  // DestroyLayer should be called on the mock with the correct ID.
  coordinator_proxy_->DestroyLayer(layer_id);
  PingMockDisplayCoordinator();
  EXPECT_TRUE(destroyed_layer_id.has_value());

  EXPECT_EQ(destroyed_layer_id.value(), layer_id);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Test that SetDisplayMode() does not immediately send a FIDL call, but that the call is sent
// when ApplyConfig() is called.
TEST_F(CoordinatorProxyTest, SetAndApplyDisplayMode) {
  bool set_display_mode_called = false;
  WireDisplayId captured_display_id = {};
  WireDisplayMode captured_mode = {};

  mock_coordinator_->set_display_mode_fn([&](WireDisplayId display_id, WireDisplayMode mode) {
    set_display_mode_called = true;
    captured_display_id = display_id;
    captured_mode = mode;
  });

  // SetDisplayMode should not call the mock.
  const DisplayId display_id(1);
  const DisplayMode display_mode({.active_area = types::Extent2({.width = 1920, .height = 1080}),
                                  .refresh_rate_millihertz = 60000,
                                  .mode_flags = 0});
  coordinator_proxy_->SetDisplayMode(display_id, display_mode);
  PingMockDisplayCoordinator();
  EXPECT_FALSE(set_display_mode_called);

  // ApplyConfig should call the mock.
  const WireConfigStamp config_stamp{.value = 1};
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(config_stamp).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_TRUE(set_display_mode_called);
  EXPECT_EQ(captured_display_id.value, display_id.value());
  EXPECT_EQ(captured_mode.active_area.width, 1920u);
  EXPECT_EQ(captured_mode.active_area.height, 1080u);
  EXPECT_EQ(captured_mode.refresh_rate_millihertz, 60000u);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Test that SetDisplayColorConversion() does not immediately send a FIDL call, but that the call
// is sent when ApplyConfig() is called.
TEST_F(CoordinatorProxyTest, SetAndApplyDisplayColorConversion) {
  bool set_display_color_conversion_called = false;
  WireDisplayId captured_display_id = {};
  std::array<float, 3> captured_preblend;
  std::array<float, 9> captured_coefficients;
  std::array<float, 3> captured_postblend;

  mock_coordinator_->set_display_color_conversion_fn(
      [&](WireDisplayId display_id, fidl::Array<float, 3> preblend,
          fidl::Array<float, 9> coefficients, fidl::Array<float, 3> postblend) {
        set_display_color_conversion_called = true;
        captured_display_id = display_id;

        memcpy(captured_preblend.data(), preblend.data(), sizeof(captured_preblend));
        memcpy(captured_coefficients.data(), coefficients.data(), sizeof(captured_coefficients));
        memcpy(captured_postblend.data(), postblend.data(), sizeof(captured_postblend));
      });

  // SetDisplayColorConversion should not call the mock.
  const DisplayId display_id(1);
  const std::array<float, 3> kExpectedPreblend = {0.1f, 0.2f, 0.3f};
  const std::array<float, 9> kExpectedCoefficients = {1.f, 0.f, 0.f, 0.f, 1.f, 0.f, 0.f, 0.f, 1.f};
  const std::array<float, 3> kExpectedPostblend = {0.4f, 0.5f, 0.6f};
  coordinator_proxy_->SetDisplayColorConversion(display_id, kExpectedPreblend,
                                                kExpectedCoefficients, kExpectedPostblend);
  PingMockDisplayCoordinator();
  EXPECT_FALSE(set_display_color_conversion_called);

  // ApplyConfig should call the mock.
  const WireConfigStamp config_stamp{.value = 1};
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(config_stamp).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_TRUE(set_display_color_conversion_called);
  EXPECT_EQ(captured_display_id.value, display_id.value());
  EXPECT_EQ(captured_preblend, kExpectedPreblend);
  EXPECT_EQ(captured_coefficients, kExpectedCoefficients);
  EXPECT_EQ(captured_postblend, kExpectedPostblend);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Test that SetDisplayLayers() does not immediately send a FIDL call, but that the call is sent
// when ApplyConfig() is called.
TEST_F(CoordinatorProxyTest, SetAndApplyDisplayLayers) {
  bool set_display_layers_called = false;
  WireDisplayId captured_display_id = {};
  std::vector<LayerId> captured_layers;

  mock_coordinator_->set_set_display_layers_fn(
      [&](WireDisplayId display_id, fidl::VectorView<WireLayerId> layers) {
        set_display_layers_called = true;
        captured_display_id = display_id;
        captured_layers.clear();
        captured_layers.reserve(layers.size());
        for (const auto& wire_layer : layers) {
          captured_layers.push_back(LayerId(wire_layer));
        }
      });

  // Create a couple of layers to use.
  const LayerId layer_id1 = coordinator_proxy_->CreateLayer();
  const LayerId layer_id2 = coordinator_proxy_->CreateLayer();

  // SetDisplayLayers should not call the mock.
  const DisplayId display_id(1);
  const std::vector<LayerId> kExpectedLayers = {layer_id1, layer_id2};
  coordinator_proxy_->SetDisplayLayers(display_id, std::span<const LayerId>(kExpectedLayers));
  PingMockDisplayCoordinator();
  EXPECT_FALSE(set_display_layers_called);

  // ApplyConfig should call the mock.
  const WireConfigStamp config_stamp{.value = 1};
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(config_stamp).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_TRUE(set_display_layers_called);
  EXPECT_EQ(captured_display_id.value, display_id.value());
  EXPECT_EQ(captured_layers, kExpectedLayers);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Test that SetLayerPrimaryConfig(), SetLayerPrimaryPosition(), SetLayerPrimaryAlpha(), and
// SetLayerImage() do not immediately send FIDL calls, but that the calls are sent when
// ApplyConfig() is called.
TEST_F(CoordinatorProxyTest, SetAndApplyLayerImage) {
  bool set_layer_primary_config_called = false;
  WireLayerId captured_config_layer_id = {};
  WireImageMetadata captured_image_metadata = {};
  mock_coordinator_->set_set_layer_primary_config_fn(
      [&](WireLayerId layer_id, WireImageMetadata image_metadata) {
        set_layer_primary_config_called = true;
        captured_config_layer_id = layer_id;
        captured_image_metadata = image_metadata;
      });

  bool set_layer_primary_position_called = false;
  WireLayerId captured_position_layer_id = {};
  WireCoordinateTransformation captured_transform = {};
  WireRectU captured_src_frame = {};
  WireRectU captured_dest_frame = {};
  mock_coordinator_->set_layer_primary_position_fn([&](WireLayerId layer_id,
                                                       WireCoordinateTransformation transform,
                                                       WireRectU src_frame, WireRectU dest_frame) {
    set_layer_primary_position_called = true;
    captured_position_layer_id = layer_id;
    captured_transform = transform;
    captured_src_frame = src_frame;
    captured_dest_frame = dest_frame;
  });

  bool set_layer_primary_alpha_called = false;
  WireLayerId captured_alpha_layer_id = {};
  WireAlphaMode captured_alpha_mode = {};
  float captured_alpha_val = 0.f;
  mock_coordinator_->set_set_layer_primary_alpha_fn(
      [&](WireLayerId layer_id, WireAlphaMode mode, float val) {
        set_layer_primary_alpha_called = true;
        captured_alpha_layer_id = layer_id;
        captured_alpha_mode = mode;
        captured_alpha_val = val;
      });

  bool set_layer_image_called = false;
  WireLayerId captured_image_layer_id = {};
  WireImageId captured_image_id = {};
  WireEventId captured_wait_event_id = {};
  mock_coordinator_->set_set_layer_image_fn(
      [&](WireLayerId layer_id, WireImageId image_id, WireEventId wait_event_id) {
        set_layer_image_called = true;
        captured_image_layer_id = layer_id;
        captured_image_id = image_id;
        captured_wait_event_id = wait_event_id;
      });

  // Create a layer to use.
  const LayerId layer_id = coordinator_proxy_->CreateLayer();

  // Call the SetLayer* methods. These should not call the mock yet.
  const ImageId kImageId(1);
  const types::Extent2 kImageExtent({.width = 100, .height = 100});
  const uint32_t kImageTilingType = 0;
  coordinator_proxy_->SetLayerPrimaryConfig(layer_id, kImageExtent, kImageTilingType);

  const RotateFlip kTransform = RotateFlip::kReflectY();
  const Rectangle kSrcFrame({.x = 0, .y = 0, .width = 100, .height = 100});
  const Rectangle kDestFrame({.x = 0, .y = 0, .width = 200, .height = 200});
  coordinator_proxy_->SetLayerPrimaryPosition(layer_id, kTransform, kSrcFrame, kDestFrame);

  const BlendMode kBlendMode = BlendMode::kStraightAlpha();
  const float kAlpha = 0.5f;
  coordinator_proxy_->SetLayerPrimaryAlpha(layer_id, kBlendMode, kAlpha);

  const EventId kWaitEventId(1);
  mock_coordinator_->MarkImageImported(kImageId);
  coordinator_proxy_->SetLayerImage(layer_id, kImageId, kWaitEventId);

  // The proxy sends layer diffs only for displayed layers.
  const DisplayId kDisplayId(1);
  coordinator_proxy_->SetDisplayLayers(kDisplayId, std::span<const LayerId>(&layer_id, 1));

  PingMockDisplayCoordinator();
  EXPECT_FALSE(set_layer_primary_config_called);
  EXPECT_FALSE(set_layer_primary_position_called);
  EXPECT_FALSE(set_layer_primary_alpha_called);
  EXPECT_FALSE(set_layer_image_called);

  // ApplyConfig should call the mock functions.
  const WireConfigStamp config_stamp{.value = 1};
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(config_stamp).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->set_display_layers_count(), 1u);

  EXPECT_TRUE(set_layer_primary_config_called);
  EXPECT_EQ(captured_config_layer_id.value, layer_id.value());
  EXPECT_EQ(Extent2::From(captured_image_metadata.dimensions), kImageExtent);
  EXPECT_EQ(captured_image_metadata.tiling_type, kImageTilingType);

  EXPECT_TRUE(set_layer_primary_position_called);
  EXPECT_EQ(captured_position_layer_id.value, layer_id.value());
  EXPECT_EQ(RotateFlip::From(captured_transform), kTransform);
  EXPECT_EQ(Rectangle::From(captured_src_frame), kSrcFrame);
  EXPECT_EQ(Rectangle::From(captured_dest_frame), kDestFrame);

  EXPECT_TRUE(set_layer_primary_alpha_called);
  EXPECT_EQ(captured_alpha_layer_id.value, layer_id.value());
  EXPECT_EQ(BlendMode::From(captured_alpha_mode), kBlendMode);
  EXPECT_EQ(captured_alpha_val, kAlpha);

  EXPECT_TRUE(set_layer_image_called);
  EXPECT_EQ(captured_image_layer_id.value, layer_id.value());
  EXPECT_EQ(captured_image_id.value, kImageId.value());
  EXPECT_EQ(captured_wait_event_id.value, kWaitEventId.value());

  // Updating alpha within (0.0, 1.0) should send `SetLayerPrimaryAlpha()` on the next
  // `ApplyConfig()`. The alpha stays within the same `AlphaRange`, so the config hits
  // `check_config_cache_` and `CheckConfig()` is skipped even though a layout diff was sent.
  const uint64_t initial_check_config_calls_skipped =
      coordinator_proxy_->check_config_calls_skipped();
  set_layer_primary_alpha_called = false;
  const float kUpdatedAlpha = 0.25f;
  coordinator_proxy_->SetLayerPrimaryAlpha(layer_id, kBlendMode, kUpdatedAlpha);
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 2}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_TRUE(set_layer_primary_alpha_called);
  EXPECT_EQ(captured_alpha_val, kUpdatedAlpha);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(),
            initial_check_config_calls_skipped + 1);

  // Calling `ApplyConfig()` again without changing alpha skips the alpha update and skips
  // `CheckConfig()`.
  set_layer_primary_alpha_called = false;
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 3}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_FALSE(set_layer_primary_alpha_called);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(),
            initial_check_config_calls_skipped + 2);

  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// The Coordinator resets a layer's position and alpha whenever `SetLayerPrimaryConfig()` is called.
// Test that the proxy re-sends them when it sends `SetLayerPrimaryConfig()`, even if they are
// unchanged, and that it mirrors the reset when the client doesn't re-set them.
TEST_F(CoordinatorProxyTest, SetLayerPrimaryConfigResetsPositionAndAlpha) {
  // The re-sent position and alpha only take effect if they arrive after the config, which
  // resets them, so the test records the order of the calls as well as their counts.
  std::vector<std::string> calls;

  int set_layer_primary_config_count = 0;
  mock_coordinator_->set_set_layer_primary_config_fn(
      [&](WireLayerId layer_id, WireImageMetadata image_metadata) {
        ++set_layer_primary_config_count;
        calls.push_back("config");
      });

  int set_layer_primary_position_count = 0;
  WireCoordinateTransformation captured_transform = {};
  WireRectU captured_src_frame = {};
  WireRectU captured_dest_frame = {};
  mock_coordinator_->set_layer_primary_position_fn([&](WireLayerId layer_id,
                                                       WireCoordinateTransformation transform,
                                                       WireRectU src_frame, WireRectU dest_frame) {
    ++set_layer_primary_position_count;
    calls.push_back("position");
    captured_transform = transform;
    captured_src_frame = src_frame;
    captured_dest_frame = dest_frame;
  });

  int set_layer_primary_alpha_count = 0;
  WireAlphaMode captured_alpha_mode = {};
  float captured_alpha_val = 0.f;
  mock_coordinator_->set_set_layer_primary_alpha_fn(
      [&](WireLayerId layer_id, WireAlphaMode mode, float val) {
        ++set_layer_primary_alpha_count;
        calls.push_back("alpha");
        captured_alpha_mode = mode;
        captured_alpha_val = val;
      });

  mock_coordinator_->set_set_layer_image_fn(
      [&](WireLayerId layer_id, WireImageId image_id, WireEventId wait_event_id) {
        calls.push_back("image");
      });

  const LayerId layer_id = coordinator_proxy_->CreateLayer();
  const ImageId kImageId(1);
  const uint32_t kImageTilingType = 0;
  const RotateFlip kTransform = RotateFlip::kReflectY();
  const Rectangle kSrcFrame({.x = 0, .y = 0, .width = 100, .height = 100});
  const Rectangle kDestFrame({.x = 0, .y = 0, .width = 200, .height = 200});
  const BlendMode kBlendMode = BlendMode::kStraightAlpha();
  const float kAlpha = 0.5f;

  auto set_image_layer = [&](const types::Extent2& image_extent) {
    coordinator_proxy_->SetLayerPrimaryConfig(layer_id, image_extent, kImageTilingType);
    coordinator_proxy_->SetLayerPrimaryPosition(layer_id, kTransform, kSrcFrame, kDestFrame);
    coordinator_proxy_->SetLayerPrimaryAlpha(layer_id, kBlendMode, kAlpha);
    mock_coordinator_->MarkImageImported(kImageId);
    coordinator_proxy_->SetLayerImage(layer_id, kImageId, kInvalidEventId);
  };

  set_image_layer(types::Extent2({.width = 100, .height = 100}));
  // The proxy sends layer diffs only for displayed layers.
  coordinator_proxy_->SetDisplayLayers(DisplayId(1), std::span<const LayerId>(&layer_id, 1));
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 1}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->set_display_layers_count(), 1u);
  EXPECT_EQ(set_layer_primary_config_count, 1);
  EXPECT_EQ(set_layer_primary_position_count, 1);
  EXPECT_EQ(set_layer_primary_alpha_count, 1);

  // Same config, position, and alpha: nothing but the image is re-sent.
  set_image_layer(types::Extent2({.width = 100, .height = 100}));
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 2}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(set_layer_primary_config_count, 1);
  EXPECT_EQ(set_layer_primary_position_count, 1);
  EXPECT_EQ(set_layer_primary_alpha_count, 1);
  EXPECT_EQ(calls, (std::vector<std::string>{"config", "position", "alpha", "image", "image"}));
  calls.clear();

  // Changing only the image dimensions re-sends the config, and therefore also the (unchanged)
  // position and alpha, which the coordinator resets.
  set_image_layer(types::Extent2({.width = 120, .height = 120}));
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 3}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(calls, (std::vector<std::string>{"config", "position", "alpha", "image"}));
  EXPECT_EQ(set_layer_primary_config_count, 2);
  EXPECT_EQ(set_layer_primary_position_count, 2);
  EXPECT_EQ(RotateFlip::From(captured_transform), kTransform);
  EXPECT_EQ(Rectangle::From(captured_src_frame), kSrcFrame);
  EXPECT_EQ(Rectangle::From(captured_dest_frame), kDestFrame);
  EXPECT_EQ(set_layer_primary_alpha_count, 2);
  EXPECT_EQ(BlendMode::From(captured_alpha_mode), kBlendMode);
  EXPECT_EQ(captured_alpha_val, kAlpha);

  // Calling `SetLayerPrimaryConfig()` without re-setting position and alpha resets them to the
  // defaults, just like the coordinator does.
  const types::Extent2 kImageExtent({.width = 120, .height = 120});
  coordinator_proxy_->SetLayerPrimaryConfig(layer_id, kImageExtent, kImageTilingType);
  mock_coordinator_->MarkImageImported(kImageId);
  coordinator_proxy_->SetLayerImage(layer_id, kImageId, kInvalidEventId);
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 4}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  const Rectangle kImageArea({.x = 0, .y = 0, .width = 120, .height = 120});
  EXPECT_EQ(set_layer_primary_position_count, 3);
  EXPECT_EQ(RotateFlip::From(captured_transform), RotateFlip::kIdentity());
  EXPECT_EQ(Rectangle::From(captured_src_frame), kImageArea);
  EXPECT_EQ(Rectangle::From(captured_dest_frame), kImageArea);
  EXPECT_EQ(set_layer_primary_alpha_count, 3);
  EXPECT_EQ(BlendMode::From(captured_alpha_mode), BlendMode::kReplace());
  EXPECT_EQ(captured_alpha_val, 1.f);

  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Test that an ApplyConfig() failure followed by applying a different property successfully does
// not send the abandoned draft alpha.
TEST_F(CoordinatorProxyTest, ResetDraftStateDiscardsAbandonedAlphaOnApplyConfigFailure) {
  bool set_layer_primary_alpha_called = false;
  float captured_alpha_val = 0.f;
  mock_coordinator_->set_set_layer_primary_alpha_fn(
      [&](WireLayerId layer_id, WireAlphaMode alpha_mode, float alpha_val) {
        set_layer_primary_alpha_called = true;
        captured_alpha_val = alpha_val;
      });

  bool set_layer_primary_position_called = false;
  mock_coordinator_->set_layer_primary_position_fn(
      [&](WireLayerId layer_id, WireCoordinateTransformation transform, WireRectU src_frame,
          WireRectU dest_frame) { set_layer_primary_position_called = true; });

  const LayerId layer_id = coordinator_proxy_->CreateLayer();
  const types::Extent2 kImageExtent({.width = 100, .height = 100});
  const uint32_t kImageTilingType = 0;
  coordinator_proxy_->SetLayerPrimaryConfig(layer_id, kImageExtent, kImageTilingType);

  const RotateFlip kTransform = RotateFlip::kReflectY();
  const Rectangle kSrcFrame({.x = 0, .y = 0, .width = 100, .height = 100});
  const Rectangle kDestFrame({.x = 0, .y = 0, .width = 200, .height = 200});
  coordinator_proxy_->SetLayerPrimaryPosition(layer_id, kTransform, kSrcFrame, kDestFrame);

  const BlendMode kBlendMode = BlendMode::kStraightAlpha();
  const float kInitialAlpha = 0.5f;
  coordinator_proxy_->SetLayerPrimaryAlpha(layer_id, kBlendMode, kInitialAlpha);

  const ImageId kImageId(1);
  const EventId kWaitEventId(1);
  mock_coordinator_->MarkImageImported(kImageId);
  coordinator_proxy_->SetLayerImage(layer_id, kImageId, kWaitEventId);

  const DisplayId kDisplayId(1);
  coordinator_proxy_->SetDisplayLayers(kDisplayId, std::span<const LayerId>(&layer_id, 1));

  // Successfully apply the initial config.
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 1}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_TRUE(set_layer_primary_alpha_called);
  EXPECT_EQ(captured_alpha_val, kInitialAlpha);

  // Update alpha to a new value and modify position so the config is not cached, then make
  // CheckConfig() fail.
  mock_coordinator_->set_check_config_fn(
      [](WireConfigResult* out) { *out = WireConfigResult::kInvalidConfig; });
  const float kAbandonedAlpha = 0.8f;
  coordinator_proxy_->SetLayerPrimaryAlpha(layer_id, kBlendMode, kAbandonedAlpha);
  const Rectangle kFailingDestFrame({.x = 0, .y = 0, .width = 500, .height = 500});
  coordinator_proxy_->SetLayerPrimaryPosition(layer_id, kTransform, kSrcFrame, kFailingDestFrame);
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 2}).status_value(),
            ZX_ERR_BAD_STATE);
  PingMockDisplayCoordinator();

  // Allow CheckConfig() to succeed again.
  mock_coordinator_->set_check_config_fn(
      [](WireConfigResult* out) { *out = WireConfigResult::kOk; });

  // Apply a different property (destination frame) without changing alpha.
  // The proxy should have reset the draft alpha back to kInitialAlpha, so no alpha update
  // should be sent.
  set_layer_primary_alpha_called = false;
  set_layer_primary_position_called = false;
  const Rectangle kNewDestFrame({.x = 0, .y = 0, .width = 300, .height = 300});
  coordinator_proxy_->SetLayerPrimaryPosition(layer_id, kTransform, kSrcFrame, kNewDestFrame);
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 3}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();

  EXPECT_TRUE(set_layer_primary_position_called);
  EXPECT_FALSE(set_layer_primary_alpha_called);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Test that SetLayerColorConfig() does not immediately send a FIDL call, but that the call is
// sent when ApplyConfig() is called.  Also test transitioning between image and color layers.
TEST_F(CoordinatorProxyTest, SetAndApplyLayerColor) {
  bool set_layer_color_config_called = false;
  WireLayerId captured_layer_id = {};
  WireColor captured_color = {};
  WireRectU captured_dest_frame = {};

  mock_coordinator_->set_set_layer_color_config_fn(
      [&](WireLayerId layer_id, WireColor color, WireRectU dest_frame) {
        set_layer_color_config_called = true;
        captured_layer_id = layer_id;
        captured_color = color;
        captured_dest_frame = dest_frame;
      });

  // Create a layer to use.
  const LayerId layer_id = coordinator_proxy_->CreateLayer();

  // Call SetLayerColorConfig. This should not call the mock yet.
  const std::array<float, 4> kColorBgra = {0.1f, 0.2f, 0.3f, 0.4f};
  const WireColor kWireColor = {
      .format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8,
      .bytes =
          {
              static_cast<uint8_t>(kColorBgra[0] * 255.f),  // Blue
              static_cast<uint8_t>(kColorBgra[1] * 255.f),  // Green
              static_cast<uint8_t>(kColorBgra[2] * 255.f),  // Red
              static_cast<uint8_t>(kColorBgra[3] * 255.f),  // Alpha
          },
  };

  const Rectangle kDestFrame({.x = 10, .y = 20, .width = 100, .height = 200});
  coordinator_proxy_->SetLayerColorConfig(layer_id, kWireColor, kDestFrame);

  // The proxy sends layer diffs only for displayed layers.
  coordinator_proxy_->SetDisplayLayers(DisplayId(1), std::span<const LayerId>(&layer_id, 1));

  PingMockDisplayCoordinator();
  EXPECT_FALSE(set_layer_color_config_called);

  // ApplyConfig should call the mock function.
  const WireConfigStamp config_stamp1{.value = 1};
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(config_stamp1).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->set_display_layers_count(), 1u);

  EXPECT_TRUE(set_layer_color_config_called);
  EXPECT_EQ(captured_layer_id.value, layer_id.value());
  EXPECT_EQ(captured_color.format, fuchsia_images2::wire::PixelFormat::kB8G8R8A8);
  EXPECT_EQ(captured_color.bytes, kWireColor.bytes);
  EXPECT_EQ(Rectangle::From(captured_dest_frame), kDestFrame);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);

  // Transition to an image layer.
  WireLayerId captured_primary_config_layer_id = {};
  mock_coordinator_->set_set_layer_primary_config_fn(
      [&](WireLayerId id, WireImageMetadata) { captured_primary_config_layer_id = id; });
  WireLayerId captured_image_layer_id = {};
  mock_coordinator_->set_set_layer_image_fn(
      [&](WireLayerId id, WireImageId, WireEventId) { captured_image_layer_id = id; });

  set_layer_color_config_called = false;
  const ImageId kImageId(1);
  const EventId kWaitEventId(1);
  coordinator_proxy_->SetLayerPrimaryConfig(layer_id, Extent2({.width = 100, .height = 100}),
                                            /*image_tiling_type=*/0);
  mock_coordinator_->MarkImageImported(kImageId);
  coordinator_proxy_->SetLayerImage(layer_id, kImageId, kWaitEventId);

  const WireConfigStamp config_stamp2{.value = 2};
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(config_stamp2).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_FALSE(set_layer_color_config_called);  // Should not be called
  EXPECT_EQ(mock_coordinator_->set_layer_primary_config_count(), 1u);
  EXPECT_EQ(mock_coordinator_->set_layer_image_count(), 1u);
  EXPECT_EQ(captured_primary_config_layer_id.value, layer_id.value());
  EXPECT_EQ(captured_image_layer_id.value, layer_id.value());
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);

  // Transition back to a color layer (RGBA this time, not BGRA).
  const std::array<float, 4> kColorRgba = {0.5f, 0.6f, 0.7f, 0.8f};
  const WireColor kWireColor2 = {
      .format = fuchsia_images2::wire::PixelFormat::kR8G8B8A8,
      .bytes =
          {
              static_cast<uint8_t>(kColorRgba[0] * 255.f),  // Red
              static_cast<uint8_t>(kColorRgba[1] * 255.f),  // Green
              static_cast<uint8_t>(kColorRgba[2] * 255.f),  // Blue
              static_cast<uint8_t>(kColorRgba[3] * 255.f),  // Alpha
          },
  };

  const Rectangle kDestFrame2({.x = 30, .y = 40, .width = 150, .height = 250});
  coordinator_proxy_->SetLayerColorConfig(layer_id, kWireColor2, kDestFrame2);

  const WireConfigStamp config_stamp3{.value = 3};
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(config_stamp3).status_value(), ZX_OK);
  PingMockDisplayCoordinator();

  EXPECT_TRUE(set_layer_color_config_called);
  EXPECT_EQ(captured_layer_id.value, layer_id.value());
  EXPECT_EQ(captured_color.format, fuchsia_images2::wire::PixelFormat::kR8G8B8A8);
  EXPECT_EQ(captured_color.bytes, kWireColor2.bytes);
  EXPECT_EQ(Rectangle::From(captured_dest_frame), kDestFrame2);
  EXPECT_EQ(mock_coordinator_->set_layer_image_count(), 1u);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Records the layer and config calls that a mock coordinator receives, in order, as
// "<method>:<layer id>" (plus "check" and "commit"), along with the last values sent.
struct LayerCallRecorder {
  static std::string Call(std::string_view method, const LayerId& layer_id) {
    return std::string(method) + ":" + std::to_string(layer_id.value());
  }

  void Attach(MockDisplayCoordinator& mock) {
    mock.set_set_layer_primary_config_fn([this](WireLayerId layer_id, WireImageMetadata metadata) {
      calls.push_back(Call("config", LayerId(layer_id)));
      image_metadata = metadata;
    });
    mock.set_layer_primary_position_fn([this](WireLayerId layer_id,
                                              WireCoordinateTransformation transform,
                                              WireRectU src_frame, WireRectU dest_frame) {
      calls.push_back(Call("position", LayerId(layer_id)));
      position_dest_frame = dest_frame;
    });
    mock.set_set_layer_primary_alpha_fn(
        [this](WireLayerId layer_id, WireAlphaMode mode, float value) {
          calls.push_back(Call("alpha", LayerId(layer_id)));
          alpha_value = value;
        });
    mock.set_set_layer_image_fn(
        [this](WireLayerId layer_id, WireImageId wire_image_id, WireEventId wire_wait_event_id) {
          calls.push_back(Call("image", LayerId(layer_id)));
          image_id = wire_image_id;
          wait_event_id = wire_wait_event_id;
        });
    mock.set_set_layer_color_config_fn(
        [this](WireLayerId layer_id, WireColor wire_color, WireRectU dest_frame) {
          calls.push_back(Call("color", LayerId(layer_id)));
          color = wire_color;
        });
    mock.set_check_config_fn([this](WireConfigResult* out) {
      calls.push_back("check");
      *out = check_config_result;
    });
    mock.set_commit_config_fn([this](WireConfigStamp stamp) { calls.push_back("commit"); });
  }

  std::vector<std::string> calls;
  WireImageMetadata image_metadata = {};
  WireRectU position_dest_frame = {};
  float alpha_value = 0.f;
  WireImageId image_id = {};
  WireEventId wait_event_id = {};
  WireColor color = {};
  WireConfigResult check_config_result = WireConfigResult::kOk;
};

// The Coordinator's `CommitConfig()` applies draft changes only to layers on a display, and
// `DiscardConfig()` drops the rest.  Test that the proxy sends no diffs for layers that are not in
// the display list, and sends all of them once the layers are displayed, because none were
// accepted while the layers were not displayed.
TEST_F(CoordinatorProxyTest, UndisplayedLayerDiffsAreHeldUntilDisplayed) {
  LayerCallRecorder recorder;
  recorder.Attach(*mock_coordinator_);

  const LayerId image_layer_id = coordinator_proxy_->CreateLayer();
  const LayerId color_layer_id = coordinator_proxy_->CreateLayer();

  const types::Extent2 kImageExtent({.width = 100, .height = 100});
  const Rectangle kDestFrame({.x = 0, .y = 0, .width = 200, .height = 200});
  const float kAlpha = 0.5f;
  const ImageId kImageId(1);
  const EventId kWaitEventId(2);
  coordinator_proxy_->SetLayerPrimaryConfig(image_layer_id, kImageExtent, /*image_tiling_type=*/0);
  coordinator_proxy_->SetLayerPrimaryPosition(
      image_layer_id, RotateFlip::kIdentity(),
      Rectangle({.x = 0, .y = 0, .width = 100, .height = 100}), kDestFrame);
  coordinator_proxy_->SetLayerPrimaryAlpha(image_layer_id, BlendMode::kStraightAlpha(), kAlpha);
  mock_coordinator_->MarkImageImported(kImageId);
  coordinator_proxy_->SetLayerImage(image_layer_id, kImageId, kWaitEventId);

  const WireColor kWireColor = {
      .format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8,
      .bytes = {10, 20, 30, 40},
  };
  coordinator_proxy_->SetLayerColorConfig(color_layer_id, kWireColor,
                                          Rectangle({.x = 0, .y = 0, .width = 50, .height = 50}));

  // Neither layer is displayed, so none of their diffs are sent.
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 1}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(recorder.calls, (std::vector<std::string>{"check", "commit"}));
  EXPECT_EQ(mock_coordinator_->set_display_layers_count(), 0u);
  recorder.calls.clear();

  // Once displayed, all of each layer's diffs are sent, without any further `Set*()` calls.
  // Layout diffs for all displayed layers precede `CheckConfig()`, and image diffs follow it
  // before `CommitConfig()`.
  const std::vector<LayerId> kBothLayers = {image_layer_id, color_layer_id};
  coordinator_proxy_->SetDisplayLayers(DisplayId(1), kBothLayers);
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 2}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(recorder.calls, (std::vector<std::string>{
                                LayerCallRecorder::Call("config", image_layer_id),
                                LayerCallRecorder::Call("position", image_layer_id),
                                LayerCallRecorder::Call("alpha", image_layer_id),
                                LayerCallRecorder::Call("color", color_layer_id),
                                "check",
                                LayerCallRecorder::Call("image", image_layer_id),
                                "commit",
                            }));
  EXPECT_EQ(Extent2::From(recorder.image_metadata.dimensions), kImageExtent);
  EXPECT_EQ(Rectangle::From(recorder.position_dest_frame), kDestFrame);
  EXPECT_EQ(recorder.alpha_value, kAlpha);
  EXPECT_EQ(recorder.image_id.value, kImageId.value());
  EXPECT_EQ(recorder.wait_event_id.value, kWaitEventId.ToFidl().value);
  EXPECT_EQ(recorder.color.bytes, kWireColor.bytes);
  EXPECT_EQ(mock_coordinator_->set_display_layers_count(), 1u);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Test that a layer removed from the display list in the same frame that its properties change
// sends no diffs, and that when it is displayed again, its diffs are computed against the state
// that was last applied while it was displayed.  If the proxy accepted the undisplayed layer's
// draft, it would disagree with the Coordinator, which drops such drafts on `DiscardConfig()`.
TEST_F(CoordinatorProxyTest, RemovedLayerDiffsAreHeldUntilDisplayedAgain) {
  LayerCallRecorder recorder;
  recorder.Attach(*mock_coordinator_);

  const LayerId image_layer_id = coordinator_proxy_->CreateLayer();
  const LayerId color_layer_id = coordinator_proxy_->CreateLayer();
  const DisplayId kDisplayId(1);
  const std::vector<LayerId> kBothLayers = {image_layer_id, color_layer_id};

  const RotateFlip kTransform = RotateFlip::kIdentity();
  const Rectangle kSrcFrame({.x = 0, .y = 0, .width = 100, .height = 100});
  const Rectangle kDestFrame({.x = 0, .y = 0, .width = 200, .height = 200});
  const BlendMode kBlendMode = BlendMode::kStraightAlpha();
  const float kAlpha = 0.5f;
  const ImageId kImageId(1);
  coordinator_proxy_->SetLayerPrimaryConfig(image_layer_id,
                                            types::Extent2({.width = 100, .height = 100}),
                                            /*image_tiling_type=*/0);
  coordinator_proxy_->SetLayerPrimaryPosition(image_layer_id, kTransform, kSrcFrame, kDestFrame);
  coordinator_proxy_->SetLayerPrimaryAlpha(image_layer_id, kBlendMode, kAlpha);
  mock_coordinator_->MarkImageImported(kImageId);
  coordinator_proxy_->SetLayerImage(image_layer_id, kImageId, kInvalidEventId);

  const Rectangle kColorDestFrame({.x = 0, .y = 0, .width = 50, .height = 50});
  const WireColor kWireColor = {
      .format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8,
      .bytes = {10, 20, 30, 40},
  };
  coordinator_proxy_->SetLayerColorConfig(color_layer_id, kWireColor, kColorDestFrame);

  coordinator_proxy_->SetDisplayLayers(kDisplayId, kBothLayers);
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 1}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  recorder.calls.clear();

  // Change both layers, and remove the image layer from the display list.  Only the color layer's
  // diff is sent.
  const Rectangle kNewDestFrame({.x = 0, .y = 0, .width = 300, .height = 300});
  const float kNewAlpha = 0.25f;
  coordinator_proxy_->SetLayerPrimaryPosition(image_layer_id, kTransform, kSrcFrame, kNewDestFrame);
  coordinator_proxy_->SetLayerPrimaryAlpha(image_layer_id, kBlendMode, kNewAlpha);

  const WireColor kNewWireColor = {
      .format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8,
      .bytes = {50, 60, 70, 80},
  };
  coordinator_proxy_->SetLayerColorConfig(color_layer_id, kNewWireColor, kColorDestFrame);

  coordinator_proxy_->SetDisplayLayers(kDisplayId, std::span<const LayerId>(&color_layer_id, 1));
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 2}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(recorder.calls, (std::vector<std::string>{
                                LayerCallRecorder::Call("color", color_layer_id),
                                "check",
                                "commit",
                            }));
  EXPECT_EQ(recorder.color.bytes, kNewWireColor.bytes);
  recorder.calls.clear();

  // Display the image layer again, with its position set back to the applied value but its alpha
  // left at the value set while it was not displayed.  The diff is against the state applied in
  // the first frame: the alpha is sent, the position and config are not.
  coordinator_proxy_->SetLayerPrimaryPosition(image_layer_id, kTransform, kSrcFrame, kDestFrame);
  coordinator_proxy_->SetDisplayLayers(kDisplayId, kBothLayers);
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 3}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(recorder.calls,
            (std::vector<std::string>{
                LayerCallRecorder::Call("alpha", image_layer_id),
                "check",
                // TODO(https://fxbug.dev/449807074): the unchanged image is always re-sent.
                LayerCallRecorder::Call("image", image_layer_id),
                "commit",
            }));
  EXPECT_EQ(recorder.alpha_value, kNewAlpha);
  EXPECT_EQ(mock_coordinator_->set_display_layers_count(), 3u);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Test that a failed `ApplyConfig()` resets only the displayed layers' drafts.  An undisplayed
// layer keeps a draft that was set once, as `DisplayCompositor` does for its empty-scene layer, and
// sends it when the layer is later displayed.
TEST_F(CoordinatorProxyTest, FailedApplyConfigKeepsUndisplayedLayerDraft) {
  LayerCallRecorder recorder;
  recorder.Attach(*mock_coordinator_);

  const LayerId displayed_layer_id = coordinator_proxy_->CreateLayer();
  const LayerId undisplayed_layer_id = coordinator_proxy_->CreateLayer();
  const DisplayId kDisplayId(1);

  const WireColor kUndisplayedColor = {
      .format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8,
      .bytes = {0, 0, 0, 255},
  };
  coordinator_proxy_->SetLayerColorConfig(undisplayed_layer_id, kUndisplayedColor,
                                          Rectangle({.x = 0, .y = 0, .width = 50, .height = 50}));

  // A frame that displays only the other layer fails `CheckConfig()`.
  recorder.check_config_result = WireConfigResult::kInvalidConfig;
  coordinator_proxy_->SetLayerColorConfig(
      displayed_layer_id,
      {.format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8, .bytes = {10, 20, 30, 40}},
      Rectangle({.x = 0, .y = 0, .width = 100, .height = 100}));
  coordinator_proxy_->SetDisplayLayers(kDisplayId,
                                       std::span<const LayerId>(&displayed_layer_id, 1));
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 1}).status_value(),
            ZX_ERR_BAD_STATE);
  PingMockDisplayCoordinator();
  EXPECT_EQ(recorder.calls, (std::vector<std::string>{
                                LayerCallRecorder::Call("color", displayed_layer_id),
                                "check",
                            }));
  recorder.calls.clear();

  // Displaying the undisplayed layer, with no further `Set*()` call for it, sends its draft.
  recorder.check_config_result = WireConfigResult::kOk;
  coordinator_proxy_->SetDisplayLayers(kDisplayId,
                                       std::span<const LayerId>(&undisplayed_layer_id, 1));
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 2}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(recorder.calls, (std::vector<std::string>{
                                LayerCallRecorder::Call("color", undisplayed_layer_id),
                                "check",
                                "commit",
                            }));
  EXPECT_EQ(recorder.color.bytes, kUndisplayedColor.bytes);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Test the states of the `CheckConfig()` cache:
// 1. Not cached, success: `CheckConfig()` is called, returns success, and `ApplyConfig()` is
//    called.
// 2. Cached, success: `CheckConfig()` is skipped, even when layout diffs are sent, and
//    `ApplyConfig()` is called.
// 3. Not cached, failure: `CheckConfig()` is called, returns failure, `ApplyConfig()` is not
//    called, and `DiscardConfig()` is.
// 4. Cached, failure: No FIDL calls are made, and an error is returned.
TEST_F(CoordinatorProxyTest, ApplyConfig_CheckConfigCache) {
  // Create a couple of layers to use.
  const std::array<LayerId, 2> layer_ids = {coordinator_proxy_->CreateLayer(),
                                            coordinator_proxy_->CreateLayer()};
  const LayerId& color_layer_id = layer_ids[0];
  const LayerId& image_layer_id = layer_ids[1];

  const DisplayId kDisplayID(1);

  // Set up color layer.
  {
    const std::array<float, 4> kColorBgra = {0.1f, 0.2f, 0.3f, 0.4f};
    const WireColor kWireColor = {
        .format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8,
        .bytes =
            {
                static_cast<uint8_t>(kColorBgra[0] * 255.f),  // Blue
                static_cast<uint8_t>(kColorBgra[1] * 255.f),  // Green
                static_cast<uint8_t>(kColorBgra[2] * 255.f),  // Red
                static_cast<uint8_t>(kColorBgra[3] * 255.f),  // Alpha
            },
    };

    const Rectangle kDestFrame({.x = 10, .y = 20, .width = 100, .height = 200});
    coordinator_proxy_->SetLayerColorConfig(color_layer_id, kWireColor, kDestFrame);
  }

  // Set up image layer.
  {
    const types::Extent2 kImageExtent({.width = 100, .height = 100});
    const uint32_t kImageTilingType = 0;
    coordinator_proxy_->SetLayerPrimaryConfig(image_layer_id, kImageExtent, kImageTilingType);

    const RotateFlip kTransform = RotateFlip::kReflectY();
    const Rectangle kSrcFrame({.x = 0, .y = 0, .width = 100, .height = 100});
    const Rectangle kDestFrame({.x = 0, .y = 0, .width = 200, .height = 200});
    coordinator_proxy_->SetLayerPrimaryPosition(image_layer_id, kTransform, kSrcFrame, kDestFrame);

    const BlendMode kBlendMode = BlendMode::kStraightAlpha();
    const float kAlpha = 0.5f;
    coordinator_proxy_->SetLayerPrimaryAlpha(image_layer_id, kBlendMode, kAlpha);

    const ImageId kImageId(1);
    const EventId kWaitEventId(1);
    mock_coordinator_->MarkImageImported(kImageId);
    coordinator_proxy_->SetLayerImage(image_layer_id, kImageId, kWaitEventId);
  }

  // Verify count of API calls made to the proxy.  None have been over FIDL yet.
  EXPECT_EQ(coordinator_proxy_->api_calls_received(),
            // 2 CreateLayer +
            // 1 SetLayerColorConfig +
            // 3 SetLayerPrimaryConfig/Position/Alpha +
            // 1 SetLayerImage
            7U);
  EXPECT_EQ(coordinator_proxy_->api_calls_sent(),
            // only layer creation occurs immediately
            2U);

  // The mock coordinator doesn't care whether the stamp is strictly increasing.
  const WireConfigStamp kConfigStamp{.value = 1};

  // We'll pretend that only 1 layer is allowed, so either the color layer or image layer alone is
  // OK, but both together means that `CheckConfig()` fails.

  // The first two single layer configs will succeed.
  mock_coordinator_->set_check_config_fn(
      [](WireConfigResult* out) { *out = WireConfigResult::kOk; });

  coordinator_proxy_->SetDisplayLayers(kDisplayID, {&color_layer_id, 1});
  auto result = coordinator_proxy_->ApplyConfig(kConfigStamp);
  EXPECT_TRUE(result.is_ok());
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_received(), 1U);
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_sent(), 1U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 0U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), 1U);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->check_config_count(), 1U);
  EXPECT_EQ(mock_coordinator_->discard_config_count(), 0U);

  coordinator_proxy_->SetDisplayLayers(kDisplayID, {&image_layer_id, 1});
  result = coordinator_proxy_->ApplyConfig(kConfigStamp);
  EXPECT_TRUE(result.is_ok());
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_received(), 2U);
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_sent(), 2U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 0U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), 2U);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->check_config_count(), 2U);
  EXPECT_EQ(mock_coordinator_->discard_config_count(), 0U);

  // Verify count of API calls made to the proxy.  All have been sent over FIDL.
  EXPECT_EQ(coordinator_proxy_->api_calls_received(),
            // 7 (previous value) +
            // 2 SetDisplayLayers
            9U);
  EXPECT_EQ(coordinator_proxy_->api_calls_sent(),
            // The image layer's diffs were held until it was displayed, so they are sent with the
            // second config, and there is no extra `SetLayerImage` yet.
            9U);

  // The two-layer config will fail.
  mock_coordinator_->set_check_config_fn(
      [](WireConfigResult* out) { *out = WireConfigResult::kInvalidConfig; });

  coordinator_proxy_->SetDisplayLayers(kDisplayID, layer_ids);
  result = coordinator_proxy_->ApplyConfig(kConfigStamp);
  EXPECT_TRUE(result.is_error());
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_received(), 3U);
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_sent(), 2U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 0U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), 3U);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->check_config_count(), 3U);
  EXPECT_EQ(mock_coordinator_->discard_config_count(), 1U);

  // Verify count of API calls made to the proxy.  All have been sent over FIDL.
  EXPECT_EQ(coordinator_proxy_->api_calls_received(),
            // 9 (previous value) +
            // 1 SetDisplayLayers
            10U);
  EXPECT_EQ(coordinator_proxy_->api_calls_sent(),
            // `ValidateDraftLayout()` rejects the config before `SetLayerImage2()` is sent.
            10U);

  // Pass 2: The single-layer configs hit the cache as success, so `CheckConfig()` is skipped even
  // though their display layers diffs are sent to the coordinator.
  mock_coordinator_->set_check_config_fn(
      [](WireConfigResult* out) { *out = WireConfigResult::kOk; });

  coordinator_proxy_->SetDisplayLayers(kDisplayID, {&color_layer_id, 1});
  result = coordinator_proxy_->ApplyConfig(kConfigStamp);
  EXPECT_TRUE(result.is_ok());
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_received(), 4U);
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_sent(), 3U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 1U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), 3U);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->check_config_count(), 3U);
  EXPECT_EQ(mock_coordinator_->discard_config_count(), 1U);

  coordinator_proxy_->SetDisplayLayers(kDisplayID, {&image_layer_id, 1});
  result = coordinator_proxy_->ApplyConfig(kConfigStamp);
  EXPECT_TRUE(result.is_ok());
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_received(), 5U);
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_sent(), 4U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 2U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), 3U);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->check_config_count(), 3U);
  EXPECT_EQ(mock_coordinator_->discard_config_count(), 1U);

  // The two-layer config is a cached failure: it skips `CheckConfig()` without sending diffs.
  coordinator_proxy_->SetDisplayLayers(kDisplayID, layer_ids);
  result = coordinator_proxy_->ApplyConfig(kConfigStamp);
  EXPECT_TRUE(result.is_error());
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_received(), 6U);
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_sent(), 4U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 3U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), 3U);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->check_config_count(), 3U);
  EXPECT_EQ(mock_coordinator_->discard_config_count(), 1U);

  // Verify count of API calls made to the proxy.  All have been sent over FIDL.
  EXPECT_EQ(coordinator_proxy_->api_calls_received(),
            // 10 (previous value) +
            // 3 SetDisplayLayers
            13U);
  EXPECT_EQ(coordinator_proxy_->api_calls_sent(),
            // 10 (previous value) +
            // 2 SetDisplayLayers
            // NOTE: the third SetDisplayLayers is not sent because the proxy was able to determine
            //       that CheckConfig would fail without calling SetDisplayLayers, and then needing
            //       to subsequently call DiscardConfig to clean up afterward.
            // TODO(https://fxbug.dev/449807074): +1 extra call to `SetLayerImage2()` for the
            // cached image-only config; rejected and color-only configs send none.
            12U + 1U);
}

// Helper for CheckConfigHeuristics test.  Sets up the display, and a simple config with one
// non-fullscreen layer, which triggers the "only fullscreen layers allowed" heuristic.
static void CheckConfigHeuristicsTestSetup(CoordinatorProxy& proxy,
                                           MockDisplayCoordinator* mock = nullptr) {
  const DisplayId display_id(1);
  const DisplayMode display_mode({.active_area = types::Extent2({.width = 1920, .height = 1080}),
                                  .refresh_rate_millihertz = 60000,
                                  .mode_flags = 0});
  proxy.SetDisplayMode(display_id, display_mode);

  const LayerId layer_id = proxy.CreateLayer();
  const ImageId kImageId(1);
  const types::Extent2 kImageExtent({.width = 100, .height = 100});
  const uint32_t kImageTilingType = 0;
  proxy.SetLayerPrimaryConfig(layer_id, kImageExtent, kImageTilingType);

  const RotateFlip kTransform = RotateFlip::kReflectY();
  const Rectangle kSrcFrame({.x = 0, .y = 0, .width = 100, .height = 100});
  const Rectangle kDestFrame({.x = 0, .y = 0, .width = 200, .height = 200});
  proxy.SetLayerPrimaryPosition(layer_id, kTransform, kSrcFrame, kDestFrame);

  const BlendMode kBlendMode = BlendMode::kStraightAlpha();
  const float kAlpha = 0.5f;
  proxy.SetLayerPrimaryAlpha(layer_id, kBlendMode, kAlpha);

  const EventId kWaitEventId(1);
  if (mock) {
    mock->MarkImageImported(kImageId);
  }
  proxy.SetLayerImage(layer_id, kImageId, kWaitEventId);

  proxy.SetDisplayLayers(display_id, std::vector{layer_id});
}

// Test that enabling "check config heuristics" gives the expected results.  In particular, they
// should cause `ApplyConfig()` to fail without checking the `check_config_cache_`.
TEST_F(CoordinatorProxyTest, CheckConfigHeuristics) {
  const WireConfigStamp config_stamp{.value = 1};

  // Heuristics disabled, so `ApplyConfig()` succeeds.
  {
    auto [mock, proxy] =
        MakeMockDisplayCoordinatorAndCoordinatorProxy({.enable_heuristics = false});
    CheckConfigHeuristicsTestSetup(*proxy, mock.get());
    EXPECT_EQ(proxy->ApplyConfig(config_stamp).status_value(), ZX_OK);

    // Drain all calls before destroying mock/proxy.
    PingMockDisplayCoordinator(proxy.get());
  }

  // Heuristics enabled, so `ApplyConfig()` fails.
  {
    auto [mock, proxy] = MakeMockDisplayCoordinatorAndCoordinatorProxy({.enable_heuristics = true});
    CheckConfigHeuristicsTestSetup(*proxy, mock.get());
    EXPECT_EQ(proxy->ApplyConfig(config_stamp).status_value(), ZX_ERR_BAD_STATE);

    // Drain all calls before destroying mock/proxy.
    PingMockDisplayCoordinator(proxy.get());
  }
}

// Guard against sending `SetLayerImage2(kInvalidImageId)` when `ValidateDraftLayout()` runs on a
// newly-configured image layer before its image has been imported and bound via `SetLayerImage()`.
TEST_F(CoordinatorProxyTest, ValidateDraftLayoutBeforeSetLayerImageCommitsCleanly) {
  LayerCallRecorder recorder;
  recorder.Attach(*mock_coordinator_);

  const LayerId layer_id = coordinator_proxy_->CreateLayer();
  const DisplayId kDisplayId(1);
  const Extent2 kImageExtent({.width = 100, .height = 100});
  const Rectangle kSrcFrame({.x = 0, .y = 0, .width = 100, .height = 100});
  const Rectangle kDestFrame({.x = 0, .y = 0, .width = 200, .height = 200});

  coordinator_proxy_->SetLayerPrimaryConfig(layer_id, kImageExtent, /*image_tiling_type=*/0);
  coordinator_proxy_->SetLayerPrimaryPosition(layer_id, RotateFlip::kIdentity(), kSrcFrame,
                                              kDestFrame);
  coordinator_proxy_->SetLayerPrimaryAlpha(layer_id, BlendMode::kStraightAlpha(), 0.5f);
  coordinator_proxy_->SetDisplayLayers(kDisplayId, std::span<const LayerId>(&layer_id, 1));

  // Layout validation sends only the layout commands and `CheckConfig()`, with no image bound yet.
  ASSERT_EQ(coordinator_proxy_->ValidateDraftLayout().status_value(), ZX_OK);
  EXPECT_TRUE(coordinator_proxy_->has_validated_layout());
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_received(), 1u);
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_sent(), 0u);
  PingMockDisplayCoordinator();
  EXPECT_EQ(recorder.calls, (std::vector<std::string>{
                                LayerCallRecorder::Call("config", layer_id),
                                LayerCallRecorder::Call("position", layer_id),
                                LayerCallRecorder::Call("alpha", layer_id),
                                "check",
                            }));
  EXPECT_EQ(mock_coordinator_->set_layer_image_count(), 0u);
  EXPECT_EQ(mock_coordinator_->check_config_count(), 1u);
  EXPECT_EQ(mock_coordinator_->commit_config_count(), 0u);

  // Binding the image afterward and committing sends only `SetLayerImage2()` and `CommitConfig()`,
  // without re-running `CheckConfig()`.
  const ImageId kImageId(7);
  const EventId kWaitEventId(3);
  const WireConfigStamp kStamp{.value = 42};
  mock_coordinator_->MarkImageImported(kImageId);
  coordinator_proxy_->SetLayerImage(layer_id, kImageId, kWaitEventId);
  coordinator_proxy_->CommitValidatedConfig(kStamp);
  EXPECT_FALSE(coordinator_proxy_->has_validated_layout());
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_sent(), 1u);

  PingMockDisplayCoordinator();
  EXPECT_EQ(recorder.calls, (std::vector<std::string>{
                                LayerCallRecorder::Call("config", layer_id),
                                LayerCallRecorder::Call("position", layer_id),
                                LayerCallRecorder::Call("alpha", layer_id),
                                "check",
                                LayerCallRecorder::Call("image", layer_id),
                                "commit",
                            }));
  EXPECT_EQ(recorder.image_id.value, kImageId.value());
  EXPECT_EQ(recorder.wait_event_id.value, kWaitEventId.ToFidl().value);
  EXPECT_EQ(mock_coordinator_->check_config_count(), 1u);
  EXPECT_EQ(mock_coordinator_->commit_config_count(), 1u);
  EXPECT_EQ(mock_coordinator_->last_committed_config_stamp().value, kStamp.value);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

TEST_F(CoordinatorProxyTest, RawCommitConfigWithoutImageIsIllegal) {
  const LayerId layer_id = coordinator_proxy_->CreateLayer();
  ConfigureDisplayedPrimaryLayer(layer_id);
  ASSERT_EQ(coordinator_proxy_->ValidateDraftLayout().status_value(), ZX_OK);

  // 1. Directly bypass `CoordinatorProxy`'s `FX_CHECK` by issuing `CommitConfig()` directly on the
  // raw client. The mock coordinator flags an illegal action for committing a primary layer without
  // an image.
  fidl::Arena arena;
  auto res = coordinator_proxy_->raw()->CommitConfig(
      fuchsia_hardware_display::wire::CoordinatorCommitConfigRequest::Builder(arena)
          .stamp(WireConfigStamp{.value = 1})
          .Build());
  EXPECT_TRUE(res.ok());
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 1u);

  // 2. `SetLayerImage2()` with invalid image ID 0 (`kInvalidDispId`) is illegal.
  auto set_img_res = coordinator_proxy_->raw()->SetLayerImage2(
      layer_id.ToFidl(), WireImageId{.value = fuchsia_hardware_display_types::wire::kInvalidDispId},
      kInvalidEventId.ToFidl());
  EXPECT_TRUE(set_img_res.ok());
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 2u);

  // 3. Commit a valid displayed primary layer with an imported image, then release that image and
  // call `CommitConfig()` without rebinding: `ReleaseImage()` clears the layer's image on the mock,
  // so the commit is illegal.
  const ImageId kImageId(1);
  ASSERT_TRUE(coordinator_proxy_
                  ->ImportImage(Extent2({.width = 100, .height = 100}), 0,
                                WireBufferCollectionId{.value = 1}, 0, kImageId)
                  .is_ok());
  coordinator_proxy_->SetLayerImage(layer_id, kImageId, kInvalidEventId);
  coordinator_proxy_->CommitValidatedConfig(WireConfigStamp{.value = 2});
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 2u);

  coordinator_proxy_->ReleaseImage(kImageId);
  auto post_release_commit = coordinator_proxy_->raw()->CommitConfig(
      fuchsia_hardware_display::wire::CoordinatorCommitConfigRequest::Builder(arena)
          .stamp(WireConfigStamp{.value = 3})
          .Build());
  EXPECT_TRUE(post_release_commit.ok());
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 3u);

  // 4. `DestroyLayer()` on a layer that is in the display layer list is illegal.
  auto destroy_res = coordinator_proxy_->raw()->DestroyLayer(layer_id.ToFidl());
  EXPECT_TRUE(destroy_res.ok());
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 4u);
}

TEST_F(CoordinatorProxyTest, ValidateDraftLayoutTwiceIsIdempotent) {
  const ImageId kImageId(1);
  const WireBufferCollectionId kCollectionId{.value = 1};
  const Extent2 kExtent({.width = 100, .height = 100});
  ASSERT_TRUE(coordinator_proxy_->ImportImage(kExtent, 0, kCollectionId, 0, kImageId).is_ok());

  const LayerId layer_id = coordinator_proxy_->CreateLayer();
  ConfigureDisplayedPrimaryLayer(layer_id);
  coordinator_proxy_->SetLayerImage(layer_id, kImageId, kInvalidEventId);

  ASSERT_EQ(coordinator_proxy_->ValidateDraftLayout().status_value(), ZX_OK);
  EXPECT_TRUE(coordinator_proxy_->has_validated_layout());
  const uint64_t check_calls_sent = coordinator_proxy_->check_config_calls_sent();
  const uint64_t check_calls_skipped = coordinator_proxy_->check_config_calls_skipped();

  // Calling `ValidateDraftLayout()` a second time while already validated is an idempotent no-op.
  ASSERT_EQ(coordinator_proxy_->ValidateDraftLayout().status_value(), ZX_OK);
  EXPECT_TRUE(coordinator_proxy_->has_validated_layout());
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), check_calls_sent);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), check_calls_skipped);

  coordinator_proxy_->CommitValidatedConfig(WireConfigStamp{.value = 1});
  EXPECT_FALSE(coordinator_proxy_->has_validated_layout());

  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->check_config_count(), 1u);
  EXPECT_EQ(mock_coordinator_->commit_config_count(), 1u);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

TEST_F(CoordinatorProxyTest, DiscardValidatedLayoutWhenNotValidatedIsNoop) {
  EXPECT_FALSE(coordinator_proxy_->has_validated_layout());
  // Calling `DiscardValidatedLayout()` when no layout has been validated is a safe no-op.
  coordinator_proxy_->DiscardValidatedLayout();
  EXPECT_FALSE(coordinator_proxy_->has_validated_layout());

  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->discard_config_count(), 0u);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

TEST_F(CoordinatorProxyTest, DestroyUndisplayedLayerWhileValidatedCommitsCleanly) {
  const ImageId kImageId(1);
  const WireBufferCollectionId kCollectionId{.value = 1};
  const Extent2 kExtent({.width = 100, .height = 100});
  ASSERT_TRUE(coordinator_proxy_->ImportImage(kExtent, 0, kCollectionId, 0, kImageId).is_ok());

  const LayerId displayed_layer = coordinator_proxy_->CreateLayer();
  const LayerId undisplayed_layer = coordinator_proxy_->CreateLayer();
  ConfigureDisplayedPrimaryLayer(displayed_layer);
  coordinator_proxy_->SetLayerImage(displayed_layer, kImageId, kInvalidEventId);

  ASSERT_EQ(coordinator_proxy_->ValidateDraftLayout().status_value(), ZX_OK);
  EXPECT_TRUE(coordinator_proxy_->has_validated_layout());

  // Destroying an undisplayed layer while validated is permitted.
  coordinator_proxy_->DestroyLayer(undisplayed_layer);

  coordinator_proxy_->CommitValidatedConfig(WireConfigStamp{.value = 1});
  EXPECT_FALSE(coordinator_proxy_->has_validated_layout());

  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->destroy_layer_count(), 1u);
  EXPECT_EQ(mock_coordinator_->commit_config_count(), 1u);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

TEST_F(CoordinatorProxyTest, CheckConfigCacheHitWithLayoutDiffSkipsCheckConfig) {
  const ImageId kImage1(1);
  const ImageId kImage2(2);
  const WireBufferCollectionId kCollectionId{.value = 1};
  const Extent2 kExtent({.width = 100, .height = 100});
  ASSERT_TRUE(coordinator_proxy_->ImportImage(kExtent, 0, kCollectionId, 0, kImage1).is_ok());
  ASSERT_TRUE(coordinator_proxy_->ImportImage(kExtent, 0, kCollectionId, 1, kImage2).is_ok());

  const LayerId layer_id = coordinator_proxy_->CreateLayer();
  const Rectangle kSrc({.x = 0, .y = 0, .width = 100, .height = 100});
  const Rectangle kDest1({.x = 0, .y = 0, .width = 100, .height = 100});
  const Rectangle kDest2({.x = 10, .y = 10, .width = 100, .height = 100});

  // Frame 1: populate cache for layout 1.
  ConfigureDisplayedPrimaryLayer(layer_id);
  coordinator_proxy_->SetLayerImage(layer_id, kImage1, kInvalidEventId);
  ASSERT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 1}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->check_config_count(), 1u);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), 1u);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 0u);

  // Frame 2: populate cache for layout 2.
  coordinator_proxy_->SetLayerPrimaryPosition(layer_id, RotateFlip::kIdentity(), kSrc, kDest2);
  coordinator_proxy_->SetLayerImage(layer_id, kImage2, kInvalidEventId);
  ASSERT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 2}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->check_config_count(), 2u);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), 2u);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 0u);

  // Frame 3: switch back to layout 1 (position `kDest1`). This hits `check_config_cache_`, so
  // `CheckConfig()` is skipped even though the position diff is sent.
  coordinator_proxy_->SetLayerPrimaryPosition(layer_id, RotateFlip::kIdentity(), kSrc, kDest1);
  coordinator_proxy_->SetLayerImage(layer_id, kImage1, kInvalidEventId);
  ASSERT_EQ(coordinator_proxy_->ValidateDraftLayout().status_value(), ZX_OK);
  EXPECT_TRUE(coordinator_proxy_->has_validated_layout());
  coordinator_proxy_->CommitValidatedConfig(WireConfigStamp{.value = 3});
  EXPECT_FALSE(coordinator_proxy_->has_validated_layout());

  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->check_config_count(), 2u);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), 2u);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 1u);
  EXPECT_EQ(mock_coordinator_->commit_config_count(), 3u);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);

  // Frame 4: no layout change (same position `kDest1`, same layers), so `CheckConfig()` is skipped
  // again.
  coordinator_proxy_->SetLayerImage(layer_id, kImage1, kInvalidEventId);
  ASSERT_EQ(coordinator_proxy_->ValidateDraftLayout().status_value(), ZX_OK);
  EXPECT_TRUE(coordinator_proxy_->has_validated_layout());
  coordinator_proxy_->CommitValidatedConfig(WireConfigStamp{.value = 4});
  EXPECT_FALSE(coordinator_proxy_->has_validated_layout());

  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->check_config_count(), 2u);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), 2u);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 2u);
  EXPECT_EQ(mock_coordinator_->commit_config_count(), 4u);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

TEST_F(CoordinatorProxyTest, ReleaseImageAfterValidateThenRebindCommitsReplacement) {
  LayerCallRecorder recorder;
  recorder.Attach(*mock_coordinator_);

  const ImageId kImageA(1);
  const ImageId kImageB(2);
  const WireBufferCollectionId kCollectionId{.value = 1};
  const Extent2 kExtent({.width = 100, .height = 100});
  ASSERT_TRUE(coordinator_proxy_->ImportImage(kExtent, 0, kCollectionId, 0, kImageA).is_ok());
  ASSERT_TRUE(coordinator_proxy_->ImportImage(kExtent, 0, kCollectionId, 1, kImageB).is_ok());

  const LayerId layer_id = coordinator_proxy_->CreateLayer();
  const Rectangle kSrc({.x = 0, .y = 0, .width = 100, .height = 100});
  const Rectangle kDest2({.x = 10, .y = 10, .width = 100, .height = 100});

  ConfigureDisplayedPrimaryLayer(layer_id);
  coordinator_proxy_->SetLayerImage(layer_id, kImageA, kInvalidEventId);

  ASSERT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 1}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  recorder.calls.clear();

  // Next frame: change layer's position and validate the layout.
  coordinator_proxy_->SetLayerPrimaryPosition(layer_id, RotateFlip::kIdentity(), kSrc, kDest2);
  ASSERT_EQ(coordinator_proxy_->ValidateDraftLayout().status_value(), ZX_OK);
  EXPECT_TRUE(coordinator_proxy_->has_validated_layout());

  // In the validated window: release Image A, bind Image B, and commit.
  coordinator_proxy_->ReleaseImage(kImageA);
  coordinator_proxy_->SetLayerImage(layer_id, kImageB, kInvalidEventId);
  coordinator_proxy_->CommitValidatedConfig(WireConfigStamp{.value = 2});
  EXPECT_FALSE(coordinator_proxy_->has_validated_layout());

  PingMockDisplayCoordinator();
  EXPECT_EQ(recorder.calls, (std::vector<std::string>{
                                LayerCallRecorder::Call("position", layer_id),
                                "check",
                                LayerCallRecorder::Call("image", layer_id),
                                "commit",
                            }));
  EXPECT_EQ(recorder.image_id.value, kImageB.value());
  EXPECT_EQ(mock_coordinator_->check_config_count(), 2u);
  EXPECT_EQ(mock_coordinator_->commit_config_count(), 2u);
  EXPECT_EQ(mock_coordinator_->release_image_count(), 1u);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// When a frame aborts after `ValidateDraftLayout()` succeeded (for example, because a display image
// import failed), `DiscardValidatedLayout()` sends `DiscardConfig()` on both the uncached and
// cached attempts and reverts draft state back to the last committed state.
TEST_F(CoordinatorProxyTest, DiscardValidatedLayoutRevertsPrimaryConfigAndPreservesAppliedState) {
  LayerCallRecorder recorder;
  recorder.Attach(*mock_coordinator_);

  const LayerId layer_id = coordinator_proxy_->CreateLayer();
  const DisplayId kDisplayId(1);
  const Extent2 kInitialExtent({.width = 100, .height = 100});
  const Rectangle kSrcFrame({.x = 0, .y = 0, .width = 100, .height = 100});
  const Rectangle kInitialDest({.x = 0, .y = 0, .width = 200, .height = 200});
  const float kInitialAlpha = 0.5f;
  const ImageId kInitialImageId(1);

  coordinator_proxy_->SetLayerPrimaryConfig(layer_id, kInitialExtent, /*image_tiling_type=*/0);
  coordinator_proxy_->SetLayerPrimaryPosition(layer_id, RotateFlip::kIdentity(), kSrcFrame,
                                              kInitialDest);
  coordinator_proxy_->SetLayerPrimaryAlpha(layer_id, BlendMode::kStraightAlpha(), kInitialAlpha);
  mock_coordinator_->MarkImageImported(kInitialImageId);
  coordinator_proxy_->SetLayerImage(layer_id, kInitialImageId, kInvalidEventId);
  coordinator_proxy_->SetDisplayLayers(kDisplayId, std::span<const LayerId>(&layer_id, 1));
  ASSERT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 1}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  recorder.calls.clear();

  // Validate a new layout with different image dimensions (`SetLayerPrimaryConfig()` sent) and a
  // different alpha, then abort it: once on a cache miss, which sends `CheckConfig()`, and once on
  // a cache hit, which skips it. Layout diffs are sent on both attempts, so both discard.
  const Extent2 kAbortedExtent({.width = 120, .height = 120});
  const Rectangle kAbortedSrc({.x = 0, .y = 0, .width = 120, .height = 120});
  const float kAbortedAlpha = 0.8f;
  for (uint32_t expected_discards : {1u, 2u}) {
    SCOPED_TRACE(expected_discards);
    coordinator_proxy_->SetLayerPrimaryConfig(layer_id, kAbortedExtent, /*image_tiling_type=*/0);
    coordinator_proxy_->SetLayerPrimaryPosition(layer_id, RotateFlip::kIdentity(), kAbortedSrc,
                                                kInitialDest);
    coordinator_proxy_->SetLayerPrimaryAlpha(layer_id, BlendMode::kStraightAlpha(), kAbortedAlpha);

    ASSERT_EQ(coordinator_proxy_->ValidateDraftLayout().status_value(), ZX_OK);
    EXPECT_TRUE(coordinator_proxy_->has_validated_layout());
    coordinator_proxy_->DiscardValidatedLayout();
    EXPECT_FALSE(coordinator_proxy_->has_validated_layout());

    PingMockDisplayCoordinator();
    EXPECT_EQ(mock_coordinator_->discard_config_count(), expected_discards);
    EXPECT_EQ(mock_coordinator_->commit_config_count(), 1u);
    EXPECT_EQ(mock_coordinator_->check_config_count(), 2u);
  }
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 1u);
  recorder.calls.clear();

  // Subsequent frame updates only position: neither the aborted `120x120` config nor `0.8` alpha
  // is sent, and `kInitialImageId` remains the layer's image.
  const Rectangle kNextDest({.x = 0, .y = 0, .width = 300, .height = 300});
  coordinator_proxy_->SetLayerPrimaryPosition(layer_id, RotateFlip::kIdentity(), kSrcFrame,
                                              kNextDest);
  ASSERT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 2}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(recorder.calls, (std::vector<std::string>{
                                LayerCallRecorder::Call("position", layer_id),
                                "check",
                                LayerCallRecorder::Call("image", layer_id),
                                "commit",
                            }));
  EXPECT_EQ(recorder.image_id.value, kInitialImageId.value());
  EXPECT_EQ(mock_coordinator_->check_config_count(), 3u);
  EXPECT_EQ(mock_coordinator_->commit_config_count(), 2u);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// `ValidateDraftLayout()` must not send `SetLayerImage2()` when `CheckConfig()` rejects the layout,
// even if `SetLayerImage()` was already called on the draft.
TEST_F(CoordinatorProxyTest, ValidateDraftLayoutFailureSendsNoSetLayerImage2) {
  mock_coordinator_->set_check_config_fn(
      [](WireConfigResult* out) { *out = WireConfigResult::kInvalidConfig; });

  const LayerId layer_id = coordinator_proxy_->CreateLayer();
  coordinator_proxy_->SetLayerPrimaryConfig(layer_id, Extent2({.width = 100, .height = 100}),
                                            /*image_tiling_type=*/0);
  const ImageId kImageId(1);
  mock_coordinator_->MarkImageImported(kImageId);
  coordinator_proxy_->SetLayerImage(layer_id, kImageId, kInvalidEventId);
  coordinator_proxy_->SetDisplayLayers(DisplayId(1), std::span<const LayerId>(&layer_id, 1));

  EXPECT_EQ(coordinator_proxy_->ValidateDraftLayout().status_value(), ZX_ERR_BAD_STATE);
  EXPECT_FALSE(coordinator_proxy_->has_validated_layout());
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->set_layer_primary_config_count(), 1u);
  EXPECT_EQ(mock_coordinator_->set_layer_image_count(), 0u);
  EXPECT_EQ(mock_coordinator_->discard_config_count(), 1u);
  EXPECT_EQ(mock_coordinator_->commit_config_count(), 0u);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

TEST_F(CoordinatorProxyTest, ImportAndReleaseEventUpdatesStateAndInspect) {
  inspect::Inspector inspector;
  AsyncTestHarness harness(*this, inspector.GetRoot().CreateChild("coordinator_proxy"));
  auto& mock = harness.mock;
  auto& proxy = harness.proxy;

  auto read_inspect_uint = [&](const char* prop_name) -> std::optional<uint64_t> {
    auto result = inspect::ReadFromVmo(inspector.DuplicateVmo());
    if (result.is_error()) {
      ADD_FAILURE() << "Failed to read Inspect VMO";
      return std::nullopt;
    }
    auto hierarchy = result.take_value();
    const auto* child = hierarchy.GetByPath({"coordinator_proxy"});
    if (child == nullptr) {
      ADD_FAILURE() << "Missing coordinator_proxy Inspect child";
      return std::nullopt;
    }
    const auto* prop = child->node().get_property<inspect::UintPropertyValue>(prop_name);
    if (prop == nullptr) {
      ADD_FAILURE() << "Missing Inspect property: " << prop_name;
      return std::nullopt;
    }
    return prop->value();
  };

  zx::event explicit_event;
  zx::event generated_event;
  ASSERT_EQ(zx::event::create(0, &explicit_event), ZX_OK);
  ASSERT_EQ(zx::event::create(0, &generated_event), ZX_OK);

  const EventId kExplicitId(100);
  proxy->ImportEvent(std::move(explicit_event), kExplicitId);
  EXPECT_EQ(read_inspect_uint("Imported Events (total)"), 1u);
  EXPECT_EQ(read_inspect_uint("Imported Events (current)"), 1u);

  const EventId generated_id = proxy->ImportEvent(generated_event);
  EXPECT_NE(generated_id, kInvalidEventId);
  EXPECT_NE(generated_id, kExplicitId);
  EXPECT_EQ(read_inspect_uint("Imported Events (total)"), 2u);
  EXPECT_EQ(read_inspect_uint("Imported Events (current)"), 2u);

  PingMockDisplayCoordinator(proxy.get());
  EXPECT_EQ(mock->import_event_count(), 2u);
  EXPECT_EQ(mock->release_event_count(), 0u);
  EXPECT_EQ(mock->illegal_action_count(), 0u);

  proxy->ReleaseEvent(kExplicitId);
  EXPECT_EQ(read_inspect_uint("Imported Events (total)"), 2u);
  EXPECT_EQ(read_inspect_uint("Imported Events (current)"), 1u);

  proxy->ReleaseEvent(generated_id);
  EXPECT_EQ(read_inspect_uint("Imported Events (total)"), 2u);
  EXPECT_EQ(read_inspect_uint("Imported Events (current)"), 0u);

  PingMockDisplayCoordinator(proxy.get());
  EXPECT_EQ(mock->release_event_count(), 2u);
  EXPECT_EQ(mock->illegal_action_count(), 0u);

  // Re-importing the released explicit `EventId` succeeds because `ReleaseEvent()` erased it.
  zx::event reimported_event;
  ASSERT_EQ(zx::event::create(0, &reimported_event), ZX_OK);
  proxy->ImportEvent(std::move(reimported_event), kExplicitId);
  EXPECT_EQ(read_inspect_uint("Imported Events (total)"), 3u);
  EXPECT_EQ(read_inspect_uint("Imported Events (current)"), 1u);
  PingMockDisplayCoordinator(proxy.get());
  EXPECT_EQ(mock->import_event_count(), 3u);
  EXPECT_EQ(mock->illegal_action_count(), 0u);
}

// Exercises asynchronous `ImportImage()` with `CoordinatorProxy`'s client bound to a dedicated
// reply loop (matching Scenic's off-main reply loop): success with `ReleaseImage()` after `ZX_OK`,
// automatic unregistration on coordinator domain error and deterministic epitaph peer-closed
// transport error, local proxy teardown while a request is in flight, and `reply_loop.Shutdown()`.
TEST_F(CoordinatorProxyTest, AsyncImportImageSuccessFailurePeerCloseAndTeardown) {
  const Extent2 kExtent({.width = 64, .height = 48});
  const uint32_t kTilingType = fuchsia_hardware_display_types::kImageTilingTypeCapture;
  const WireBufferCollectionId kCollectionId{.value = 10};
  const uint32_t kBufferIndex = 4;

  inspect::Inspector inspector;
  auto read_inspect_image_count = [&](const char* child_name =
                                          "coordinator_proxy") -> std::optional<uint64_t> {
    auto result = inspect::ReadFromVmo(inspector.DuplicateVmo());
    if (result.is_error()) {
      ADD_FAILURE() << "Failed to read Inspect VMO";
      return std::nullopt;
    }
    auto hierarchy = result.take_value();
    const auto* child = hierarchy.GetByPath({child_name});
    if (child == nullptr) {
      ADD_FAILURE() << "Missing Inspect child: " << child_name;
      return std::nullopt;
    }
    const auto* prop =
        child->node().get_property<inspect::UintPropertyValue>("Imported Images (current)");
    if (prop == nullptr) {
      ADD_FAILURE() << "Missing 'Imported Images (current)' property on " << child_name;
      return std::nullopt;
    }
    return prop->value();
  };

  {
    AsyncTestHarness harness(*this, inspector.GetRoot().CreateChild("coordinator_proxy"));
    auto& mock = harness.mock;
    auto& proxy = harness.proxy;
    auto& reply_loop = harness.reply_loop;

    // Verify all non-zero request fields received by the coordinator, and release only after
    // `on_complete` reports `ZX_OK`.
    std::optional<WireImageMetadata> captured_metadata;
    std::optional<WireBufferCollectionId> captured_collection_id;
    std::optional<uint32_t> captured_buffer_index;
    std::optional<WireImageId> captured_image_id;
    mock->set_import_image_fn(
        [&](fuchsia_hardware_display::wire::CoordinatorImportImageRequest* request) {
          captured_metadata = request->image_metadata;
          captured_collection_id = request->buffer_collection_id;
          captured_buffer_index = request->buffer_index;
          captured_image_id = request->image_id;
        });

    std::optional<zx::result<>> ok_result;
    proxy->ImportImage(kExtent, kTilingType, kCollectionId, kBufferIndex, ImageId(1),
                       [&](zx::result<> res) { ok_result = res; });
    EXPECT_EQ(proxy->import_image_calls_sent(), 1u);
    EXPECT_EQ(proxy->current_image_count(), 1u);
    EXPECT_EQ(read_inspect_image_count(), 1u);
    EXPECT_FALSE(ok_result.has_value());

    PingMockDisplayCoordinator(proxy.get());
    reply_loop.RunUntilIdle();
    ASSERT_TRUE(ok_result.has_value());
    EXPECT_EQ(ok_result->status_value(), ZX_OK);
    ASSERT_TRUE(captured_metadata.has_value());
    EXPECT_EQ(Extent2::From(captured_metadata->dimensions), kExtent);
    EXPECT_EQ(captured_metadata->tiling_type, kTilingType);
    ASSERT_TRUE(captured_collection_id.has_value());
    EXPECT_EQ(captured_collection_id->value, kCollectionId.value);
    EXPECT_EQ(captured_buffer_index, kBufferIndex);
    ASSERT_TRUE(captured_image_id.has_value());
    EXPECT_EQ(captured_image_id->value, 1u);
    EXPECT_EQ(proxy->current_image_count(), 1u);
    EXPECT_EQ(read_inspect_image_count(), 1u);

    proxy->ReleaseImage(ImageId(1));
    EXPECT_EQ(proxy->current_image_count(), 0u);
    EXPECT_EQ(read_inspect_image_count(), 0u);
    PingMockDisplayCoordinator(proxy.get());
    EXPECT_EQ(mock->illegal_action_count(), 0u);

    mock->set_import_image_fn(nullptr);

    // Domain error from coordinator: `CoordinatorProxy` unregisters `ImageId(2)` before invoking
    // `on_complete`, and the caller never calls `ReleaseImage()`, so no illegal release is sent.
    mock->set_import_image_result(ZX_ERR_NOT_SUPPORTED);
    std::optional<zx::result<>> fail_result;
    std::optional<uint64_t> count_in_fail_callback;
    std::optional<uint64_t> inspect_in_fail_callback;
    proxy->ImportImage(kExtent, kTilingType, kCollectionId, /*buffer_index=*/2, ImageId(2),
                       [&](zx::result<> res) {
                         fail_result = res;
                         count_in_fail_callback = proxy->current_image_count();
                         inspect_in_fail_callback = read_inspect_image_count();
                       });
    EXPECT_EQ(proxy->current_image_count(), 1u);
    EXPECT_EQ(read_inspect_image_count(), 1u);

    PingMockDisplayCoordinator(proxy.get());
    reply_loop.RunUntilIdle();
    ASSERT_TRUE(fail_result.has_value());
    EXPECT_EQ(fail_result->status_value(), ZX_ERR_NOT_SUPPORTED);
    EXPECT_EQ(count_in_fail_callback, 0u);
    EXPECT_EQ(inspect_in_fail_callback, 0u);
    EXPECT_EQ(proxy->current_image_count(), 0u);
    EXPECT_EQ(read_inspect_image_count(), 0u);
    PingMockDisplayCoordinator(proxy.get());
    EXPECT_EQ(mock->illegal_action_count(), 0u);
    mock->set_import_image_result(ZX_OK);

    // Deterministic epitaph peer-close using `RunWithInFlightPeerClose()`.
    std::optional<zx::result<>> peer_closed_result;
    std::optional<uint64_t> count_in_peer_closed_callback;
    std::optional<uint64_t> inspect_in_peer_closed_callback;

    harness.RunWithInFlightPeerClose([&] {
      proxy->ImportImage(kExtent, kTilingType, kCollectionId, /*buffer_index=*/3, ImageId(3),
                         [&](zx::result<> res) {
                           peer_closed_result = res;
                           count_in_peer_closed_callback = proxy->current_image_count();
                           inspect_in_peer_closed_callback = read_inspect_image_count();
                         });
      EXPECT_EQ(proxy->current_image_count(), 1u);
      EXPECT_EQ(read_inspect_image_count(), 1u);
    });

    ASSERT_TRUE(peer_closed_result.has_value());
    EXPECT_EQ(peer_closed_result->status_value(), ZX_ERR_PEER_CLOSED);
    EXPECT_EQ(count_in_peer_closed_callback, 0u);
    EXPECT_EQ(inspect_in_peer_closed_callback, 0u);
    EXPECT_EQ(proxy->current_image_count(), 0u);
    EXPECT_EQ(read_inspect_image_count(), 0u);
  }

  // Local teardown while an `ImportImage()` request is in flight: resetting `proxy` delivers
  // `ZX_ERR_CANCELED` without crashing.
  {
    AsyncTestHarness harness(*this, inspector.GetRoot().CreateChild("coordinator_proxy_teardown"));
    ScopedServerBlock server_block(display_coordinator_loop_.dispatcher());

    std::optional<zx::result<>> canceled_result;
    harness.proxy->ImportImage(kExtent, kTilingType, kCollectionId, kBufferIndex, ImageId(4),
                               [&](zx::result<> res) { canceled_result = res; });
    EXPECT_EQ(read_inspect_image_count("coordinator_proxy_teardown"), 1u);

    harness.proxy.reset();
    harness.reply_loop.RunUntilIdle();
    ASSERT_TRUE(canceled_result.has_value());
    EXPECT_EQ(canceled_result->status_value(), ZX_ERR_CANCELED);

    server_block.Unblock();
  }

  // `reply_loop.Shutdown()` while an `ImportImage()` request is in flight cancels the callback
  // with `ZX_ERR_CANCELED` and cleans up bookkeeping without logging an ERROR.
  {
    AsyncTestHarness harness(*this,
                             inspector.GetRoot().CreateChild("coordinator_proxy_loop_shutdown"));
    ScopedServerBlock server_block(display_coordinator_loop_.dispatcher());

    std::optional<zx::result<>> canceled_result;
    harness.proxy->ImportImage(kExtent, kTilingType, kCollectionId, kBufferIndex, ImageId(5),
                               [&](zx::result<> res) { canceled_result = res; });
    EXPECT_EQ(harness.proxy->current_image_count(), 1u);
    EXPECT_EQ(read_inspect_image_count("coordinator_proxy_loop_shutdown"), 1u);

    harness.reply_loop.Shutdown();
    server_block.Unblock();

    ASSERT_TRUE(canceled_result.has_value());
    EXPECT_EQ(canceled_result->status_value(), ZX_ERR_CANCELED);
    EXPECT_EQ(harness.proxy->current_image_count(), 0u);
    EXPECT_EQ(read_inspect_image_count("coordinator_proxy_loop_shutdown"), 0u);
  }

  // Calling `ImportImage()` after `reply_loop.Shutdown()` runs `on_complete` synchronously on the
  // calling thread with `ZX_ERR_CANCELED` before `ImportImage()` returns (testing re-entrant
  // locking of `SharedState::mutex` without deadlocking or logging an ERROR).
  {
    AsyncTestHarness harness(*this,
                             inspector.GetRoot().CreateChild("coordinator_proxy_after_shutdown"));
    harness.reply_loop.Shutdown();

    std::optional<zx::result<>> sync_canceled_result;
    harness.proxy->ImportImage(kExtent, kTilingType, kCollectionId, kBufferIndex, ImageId(6),
                               [&](zx::result<> res) { sync_canceled_result = res; });
    ASSERT_TRUE(sync_canceled_result.has_value());
    EXPECT_EQ(sync_canceled_result->status_value(), ZX_ERR_CANCELED);
    EXPECT_EQ(harness.proxy->current_image_count(), 0u);
    EXPECT_EQ(read_inspect_image_count("coordinator_proxy_after_shutdown"), 0u);
  }
}

TEST_F(CoordinatorProxyTest, AsyncBufferCollectionMethods) {
  AsyncTestHarness harness(*this);
  auto& mock = harness.mock;
  auto& proxy = harness.proxy;
  auto& reply_loop = harness.reply_loop;

  const WireBufferCollectionId kCollectionId{.value = 55};
  auto [token_client, token_server] =
      fidl::Endpoints<fuchsia_sysmem2::BufferCollectionToken>::Create();

  std::optional<WireBufferCollectionId> captured_import_id;
  bool captured_token_valid = false;
  mock->set_import_buffer_collection_fn(
      [&](WireBufferCollectionId id,
          fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token) {
        captured_import_id = id;
        captured_token_valid = token.is_valid();
      });

  std::optional<zx::result<>> import_result;
  proxy->ImportBufferCollection(kCollectionId, std::move(token_client),
                                [&](zx::result<> res) { import_result = res; });
  EXPECT_FALSE(import_result.has_value());

  PingMockDisplayCoordinator(proxy.get());
  reply_loop.RunUntilIdle();
  ASSERT_TRUE(import_result.has_value());
  EXPECT_EQ(import_result->status_value(), ZX_OK);
  EXPECT_EQ(mock->import_buffer_collection_count(), 1u);
  ASSERT_TRUE(captured_import_id.has_value());
  EXPECT_EQ(captured_import_id->value, kCollectionId.value);
  EXPECT_TRUE(captured_token_valid);

  std::optional<WireBufferCollectionId> captured_constraints_id;
  std::optional<uint32_t> captured_tiling_type;
  mock->set_set_buffer_collection_constraints_fn(
      [&](WireBufferCollectionId id, WireImageBufferUsage usage) {
        captured_constraints_id = id;
        captured_tiling_type = usage.tiling_type;
      });

  std::optional<zx::result<>> constraints_result;
  proxy->SetBufferCollectionConstraints(
      kCollectionId,
      WireImageBufferUsage{.tiling_type = fuchsia_hardware_display_types::kImageTilingTypeCapture},
      [&](zx::result<> res) { constraints_result = res; });
  PingMockDisplayCoordinator(proxy.get());
  reply_loop.RunUntilIdle();
  ASSERT_TRUE(constraints_result.has_value());
  EXPECT_EQ(constraints_result->status_value(), ZX_OK);
  EXPECT_EQ(mock->set_buffer_collection_constraints_count(), 1u);
  ASSERT_TRUE(captured_constraints_id.has_value());
  EXPECT_EQ(captured_constraints_id->value, kCollectionId.value);
  EXPECT_EQ(captured_tiling_type, fuchsia_hardware_display_types::kImageTilingTypeCapture);

  // Domain error propagation on `ImportBufferCollection()`.
  mock->set_import_buffer_collection_result(ZX_ERR_NO_MEMORY);
  auto [failing_token_client, failing_token_server] =
      fidl::Endpoints<fuchsia_sysmem2::BufferCollectionToken>::Create();
  std::optional<zx::result<>> failing_import_result;
  proxy->ImportBufferCollection(WireBufferCollectionId{.value = 56},
                                std::move(failing_token_client),
                                [&](zx::result<> res) { failing_import_result = res; });
  PingMockDisplayCoordinator(proxy.get());
  reply_loop.RunUntilIdle();
  ASSERT_TRUE(failing_import_result.has_value());
  EXPECT_EQ(failing_import_result->status_value(), ZX_ERR_NO_MEMORY);
  mock->set_import_buffer_collection_result(ZX_OK);

  // Domain error propagation on `SetBufferCollectionConstraints()`.
  mock->set_set_buffer_collection_constraints_result(ZX_ERR_NOT_SUPPORTED);
  std::optional<zx::result<>> failing_constraints_result;
  proxy->SetBufferCollectionConstraints(
      kCollectionId,
      WireImageBufferUsage{.tiling_type = fuchsia_hardware_display_types::kImageTilingTypeCapture},
      [&](zx::result<> res) { failing_constraints_result = res; });
  PingMockDisplayCoordinator(proxy.get());
  reply_loop.RunUntilIdle();
  ASSERT_TRUE(failing_constraints_result.has_value());
  EXPECT_EQ(failing_constraints_result->status_value(), ZX_ERR_NOT_SUPPORTED);
  mock->set_set_buffer_collection_constraints_result(ZX_OK);

  std::optional<WireBufferCollectionId> captured_release_id;
  mock->set_release_buffer_collection_fn(
      [&](WireBufferCollectionId id) { captured_release_id = id; });
  proxy->ReleaseBufferCollection(kCollectionId);
  PingMockDisplayCoordinator(proxy.get());
  EXPECT_EQ(mock->release_buffer_collection_count(), 1u);
  ASSERT_TRUE(captured_release_id.has_value());
  EXPECT_EQ(captured_release_id->value, kCollectionId.value);
  EXPECT_EQ(mock->illegal_action_count(), 0u);

  // Deterministic epitaph peer-close while requests from both methods are in flight.
  auto [peer_close_token_client, peer_close_token_server] =
      fidl::Endpoints<fuchsia_sysmem2::BufferCollectionToken>::Create();
  const WireBufferCollectionId kPeerCloseCollectionId{.value = 57};
  std::optional<zx::result<>> peer_closed_import_result;
  std::optional<zx::result<>> peer_closed_constraints_result;

  harness.RunWithInFlightPeerClose([&] {
    proxy->ImportBufferCollection(kPeerCloseCollectionId, std::move(peer_close_token_client),
                                  [&](zx::result<> res) { peer_closed_import_result = res; });
    proxy->SetBufferCollectionConstraints(
        kPeerCloseCollectionId,
        WireImageBufferUsage{.tiling_type =
                                 fuchsia_hardware_display_types::kImageTilingTypeCapture},
        [&](zx::result<> res) { peer_closed_constraints_result = res; });
  });

  ASSERT_TRUE(peer_closed_import_result.has_value());
  EXPECT_EQ(peer_closed_import_result->status_value(), ZX_ERR_PEER_CLOSED);
  ASSERT_TRUE(peer_closed_constraints_result.has_value());
  EXPECT_EQ(peer_closed_constraints_result->status_value(), ZX_ERR_PEER_CLOSED);

  // Local teardown while `ImportBufferCollection()` and `SetBufferCollectionConstraints()` are in
  // flight: resetting `proxy` delivers `ZX_ERR_CANCELED` to both callbacks.
  {
    AsyncTestHarness teardown_harness(*this);
    ScopedServerBlock server_block(display_coordinator_loop_.dispatcher());

    auto [teardown_token_client, teardown_token_server] =
        fidl::Endpoints<fuchsia_sysmem2::BufferCollectionToken>::Create();
    const WireBufferCollectionId kTeardownCollectionId{.value = 58};
    std::optional<zx::result<>> canceled_import_result;
    std::optional<zx::result<>> canceled_constraints_result;
    teardown_harness.proxy->ImportBufferCollection(
        kTeardownCollectionId, std::move(teardown_token_client),
        [&](zx::result<> res) { canceled_import_result = res; });
    teardown_harness.proxy->SetBufferCollectionConstraints(
        kTeardownCollectionId,
        WireImageBufferUsage{.tiling_type =
                                 fuchsia_hardware_display_types::kImageTilingTypeCapture},
        [&](zx::result<> res) { canceled_constraints_result = res; });

    teardown_harness.proxy.reset();
    teardown_harness.reply_loop.RunUntilIdle();
    ASSERT_TRUE(canceled_import_result.has_value());
    EXPECT_EQ(canceled_import_result->status_value(), ZX_ERR_CANCELED);
    ASSERT_TRUE(canceled_constraints_result.has_value());
    EXPECT_EQ(canceled_constraints_result->status_value(), ZX_ERR_CANCELED);

    server_block.Unblock();
  }

  // `reply_loop.Shutdown()` while `ImportBufferCollection()` and
  // `SetBufferCollectionConstraints()` are in flight delivers `ZX_ERR_CANCELED` without logging an
  // ERROR.
  {
    AsyncTestHarness shutdown_harness(*this);
    ScopedServerBlock server_block(display_coordinator_loop_.dispatcher());

    auto [shutdown_token_client, shutdown_token_server] =
        fidl::Endpoints<fuchsia_sysmem2::BufferCollectionToken>::Create();
    const WireBufferCollectionId kShutdownCollectionId{.value = 59};
    std::optional<zx::result<>> canceled_import_result;
    std::optional<zx::result<>> canceled_constraints_result;
    shutdown_harness.proxy->ImportBufferCollection(
        kShutdownCollectionId, std::move(shutdown_token_client),
        [&](zx::result<> res) { canceled_import_result = res; });
    shutdown_harness.proxy->SetBufferCollectionConstraints(
        kShutdownCollectionId,
        WireImageBufferUsage{.tiling_type =
                                 fuchsia_hardware_display_types::kImageTilingTypeCapture},
        [&](zx::result<> res) { canceled_constraints_result = res; });

    shutdown_harness.reply_loop.Shutdown();
    server_block.Unblock();

    ASSERT_TRUE(canceled_import_result.has_value());
    EXPECT_EQ(canceled_import_result->status_value(), ZX_ERR_CANCELED);
    ASSERT_TRUE(canceled_constraints_result.has_value());
    EXPECT_EQ(canceled_constraints_result->status_value(), ZX_ERR_CANCELED);
  }

  // Calling `ImportBufferCollection()` and `SetBufferCollectionConstraints()` after
  // `reply_loop.Shutdown()` runs `on_complete` synchronously on the calling thread with
  // `ZX_ERR_CANCELED` before each call returns.
  {
    AsyncTestHarness after_shutdown_harness(*this);
    after_shutdown_harness.reply_loop.Shutdown();

    auto [after_token_client, after_token_server] =
        fidl::Endpoints<fuchsia_sysmem2::BufferCollectionToken>::Create();
    const WireBufferCollectionId kAfterShutdownCollectionId{.value = 60};
    std::optional<zx::result<>> sync_canceled_import;
    after_shutdown_harness.proxy->ImportBufferCollection(
        kAfterShutdownCollectionId, std::move(after_token_client),
        [&](zx::result<> res) { sync_canceled_import = res; });
    ASSERT_TRUE(sync_canceled_import.has_value());
    EXPECT_EQ(sync_canceled_import->status_value(), ZX_ERR_CANCELED);

    std::optional<zx::result<>> sync_canceled_constraints;
    after_shutdown_harness.proxy->SetBufferCollectionConstraints(
        kAfterShutdownCollectionId,
        WireImageBufferUsage{.tiling_type =
                                 fuchsia_hardware_display_types::kImageTilingTypeCapture},
        [&](zx::result<> res) { sync_canceled_constraints = res; });
    ASSERT_TRUE(sync_canceled_constraints.has_value());
    EXPECT_EQ(sync_canceled_constraints->status_value(), ZX_ERR_CANCELED);
  }
}

TEST_F(CoordinatorProxyTest, ChannelHandleAndOnFidlErrorCallback) {
  // 1. Peer close invokes the registered callback with `is_peer_closed()`, clears
  // `channel_handle()`, and runs any later callback synchronously.
  {
    AsyncTestHarness harness(*this);
    const zx_handle_t ch = harness.proxy->channel_handle();
    EXPECT_NE(ch, ZX_HANDLE_INVALID);

    std::optional<fidl::UnbindInfo> observed_error;
    harness.proxy->SetOnFidlErrorCallback([&](fidl::UnbindInfo info) { observed_error = info; });

    libsync::Completion closed;
    async::PostTask(display_coordinator_loop_.dispatcher(), [&] {
      harness.mock->ResetCoordinatorBinding();
      closed.Signal();
    });
    closed.Wait();

    // `fidl::ServerBindingRef::Close()` posts server teardown asynchronously onto
    // `display_coordinator_loop_`; wait until the peer channel handle closes before
    // pumping `reply_loop`.
    zx_signals_t observed_signals = 0;
    ASSERT_EQ(zx_object_wait_one(ch, ZX_CHANNEL_PEER_CLOSED, zx::deadline_after(zx::sec(5)).get(),
                                 &observed_signals),
              ZX_OK);

    harness.reply_loop.RunUntilIdle();
    ASSERT_TRUE(observed_error.has_value());
    EXPECT_TRUE(observed_error->is_peer_closed());
    EXPECT_EQ(harness.proxy->channel_handle(), ZX_HANDLE_INVALID);

    // Registering after unbind invokes the callback immediately with the latched `UnbindInfo`.
    std::optional<fidl::UnbindInfo> immediate_info;
    harness.proxy->SetOnFidlErrorCallback([&](fidl::UnbindInfo info) { immediate_info = info; });
    ASSERT_TRUE(immediate_info.has_value());
    EXPECT_TRUE(immediate_info->is_peer_closed());
  }

  // 2. Clearing the callback with `SetOnFidlErrorCallback(nullptr)` before peer close suppresses
  // invocation, and re-registering a replacement callback after clearing invokes the replacement.
  {
    AsyncTestHarness harness(*this);
    const zx_handle_t ch = harness.proxy->channel_handle();
    bool cleared_called = false;
    harness.proxy->SetOnFidlErrorCallback([&](fidl::UnbindInfo) { cleared_called = true; });
    harness.proxy->SetOnFidlErrorCallback(nullptr);

    libsync::Completion closed;
    async::PostTask(display_coordinator_loop_.dispatcher(), [&] {
      harness.mock->ResetCoordinatorBinding();
      closed.Signal();
    });
    closed.Wait();

    zx_signals_t observed_signals = 0;
    ASSERT_EQ(zx_object_wait_one(ch, ZX_CHANNEL_PEER_CLOSED, zx::deadline_after(zx::sec(5)).get(),
                                 &observed_signals),
              ZX_OK);
    harness.reply_loop.RunUntilIdle();
    EXPECT_FALSE(cleared_called);
  }
  {
    AsyncTestHarness harness(*this);
    const zx_handle_t ch = harness.proxy->channel_handle();
    bool first_called = false;
    bool second_called = false;
    harness.proxy->SetOnFidlErrorCallback([&](fidl::UnbindInfo) { first_called = true; });
    harness.proxy->SetOnFidlErrorCallback(nullptr);
    harness.proxy->SetOnFidlErrorCallback([&](fidl::UnbindInfo) { second_called = true; });

    libsync::Completion closed;
    async::PostTask(display_coordinator_loop_.dispatcher(), [&] {
      harness.mock->ResetCoordinatorBinding();
      closed.Signal();
    });
    closed.Wait();

    zx_signals_t observed_signals = 0;
    ASSERT_EQ(zx_object_wait_one(ch, ZX_CHANNEL_PEER_CLOSED, zx::deadline_after(zx::sec(5)).get(),
                                 &observed_signals),
              ZX_OK);
    harness.reply_loop.RunUntilIdle();
    EXPECT_FALSE(first_called);
    EXPECT_TRUE(second_called);
  }

  // 3. Destroying `CoordinatorProxy` clears any registered callback before peer close.
  {
    AsyncTestHarness harness(*this);
    bool callback_called = false;
    harness.proxy->SetOnFidlErrorCallback([&](fidl::UnbindInfo) { callback_called = true; });
    auto keep_alive = harness.proxy->raw().Clone();
    harness.proxy.reset();

    libsync::Completion closed;
    async::PostTask(display_coordinator_loop_.dispatcher(), [&] {
      harness.mock->ResetCoordinatorBinding();
      closed.Signal();
    });
    closed.Wait();

    libsync::Completion drained;
    async::PostTask(display_coordinator_loop_.dispatcher(), [&] { drained.Signal(); });
    drained.Wait();
    harness.reply_loop.RunUntilIdle();
    EXPECT_FALSE(callback_called);
  }

  // 4. A proxy constructed from a pre-bound `WireSharedClient` reports `ZX_HANDLE_INVALID`, and a
  // callback registered on it never runs on peer close.
  {
    auto [client_end, server_end] =
        fidl::Endpoints<fuchsia_hardware_display::Coordinator>::Create();
    async::Loop prebound_loop(&kAsyncLoopConfigNeverAttachToThread);
    fidl::WireSharedClient<fuchsia_hardware_display::Coordinator> shared_client(
        std::move(client_end), prebound_loop.dispatcher());
    CoordinatorProxy prebound_proxy(std::move(shared_client));
    EXPECT_EQ(prebound_proxy.channel_handle(), ZX_HANDLE_INVALID);

    bool prebound_callback_called = false;
    prebound_proxy.SetOnFidlErrorCallback(
        [&](fidl::UnbindInfo) { prebound_callback_called = true; });
    server_end.reset();
    prebound_loop.RunUntilIdle();
    EXPECT_FALSE(prebound_callback_called);
  }

  // 5. `raw().AsyncTeardown()` does not invoke the registered callback and clears
  // `channel_handle()` once teardown completes.
  {
    AsyncTestHarness harness(*this);
    EXPECT_NE(harness.proxy->channel_handle(), ZX_HANDLE_INVALID);

    bool callback_called = false;
    harness.proxy->SetOnFidlErrorCallback([&](fidl::UnbindInfo) { callback_called = true; });
    harness.proxy->raw().AsyncTeardown();
    harness.reply_loop.RunUntilIdle();
    EXPECT_FALSE(callback_called);
    EXPECT_EQ(harness.proxy->channel_handle(), ZX_HANDLE_INVALID);
  }
}

TEST_F(CoordinatorProxyTest, NotifyExpectedPresentForwardsImmediatelyAndPreservesCheckConfigCache) {
  std::optional<WireDisplayId> captured_display_id;
  std::optional<zx::time_monotonic> captured_present_time;
  std::optional<zx::duration> captured_frame_interval;
  mock_coordinator_->set_notify_expected_present_fn([&](WireDisplayId display_id,
                                                        zx::time_monotonic expected_present_time,
                                                        zx::duration frame_interval) {
    captured_display_id = display_id;
    captured_present_time = expected_present_time;
    captured_frame_interval = frame_interval;
  });

  const DisplayId kDisplayId(1);
  const LayerId layer_id = coordinator_proxy_->CreateLayer();
  coordinator_proxy_->SetLayerColorConfig(
      layer_id,
      WireColor{.format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8, .bytes = {1, 2, 3, 4}},
      Rectangle({.x = 0, .y = 0, .width = 100, .height = 100}));
  coordinator_proxy_->SetDisplayLayers(kDisplayId, std::span<const LayerId>(&layer_id, 1));
  ASSERT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 1}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), 1u);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 0u);

  coordinator_proxy_->NotifyExpectedPresent(kDisplayId, zx::time_monotonic(500'000'000),
                                            zx::nsec(8'333'333));
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->notify_expected_present_count(), 1u);
  ASSERT_TRUE(captured_display_id.has_value());
  EXPECT_EQ(captured_display_id->value, kDisplayId.value());
  EXPECT_EQ(captured_present_time, zx::time_monotonic(500'000'000));
  EXPECT_EQ(captured_frame_interval, zx::nsec(8'333'333));

  // Re-applying the same configuration must still hit `check_config_cache_`.
  ASSERT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 2}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), 1u);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 1u);
}

}  // namespace display::test
