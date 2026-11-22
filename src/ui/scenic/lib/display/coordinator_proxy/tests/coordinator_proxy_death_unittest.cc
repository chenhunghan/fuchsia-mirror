// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// clang-format off
#include "src/ui/scenic/lib/display/coordinator_proxy/coordinator_proxy.h"
// clang-format on

#include <lib/async-loop/cpp/loop.h>
#include <lib/async/cpp/task.h>
#include <lib/sync/cpp/completion.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/event.h>

#include <array>
#include <memory>
#include <span>

#include <gtest/gtest.h>

#include "src/lib/testing/loop_fixture/real_loop_fixture.h"
#include "src/ui/scenic/lib/display/tests/mock_display_coordinator.h"

namespace display::test {

class CoordinatorProxyDeathTest : public gtest::RealLoopFixture {
 public:
  static constexpr uint32_t kMaxDisplayLayersCount = 2;

  static constexpr std::array<WireDisplayMode, 1> kSupportedDisplayModes = {
      fuchsia_hardware_display_types::wire::Mode{.active_area = {.width = 1024, .height = 768},
                                                 .refresh_rate_millihertz = 60000,
                                                 .flags = {}}};

  void SetUp() override {
    gtest::RealLoopFixture::SetUp();
    display_coordinator_loop_.StartThread("display-coordinator-loop");

    zx::result endpoints_result = fidl::CreateEndpoints<fuchsia_hardware_display::Coordinator>();
    FX_CHECK(endpoints_result.is_ok());
    auto [coordinator_client, coordinator_server] = std::move(endpoints_result).value();

    fuchsia_hardware_display::wire::Info display_info = {
        .modes = fidl::VectorView<fuchsia_hardware_display_types::wire::Mode>::FromExternal(
            const_cast<WireDisplayMode*>(kSupportedDisplayModes.data()),
            kSupportedDisplayModes.size()),
        .max_layer_count = kMaxDisplayLayersCount,
    };

    mock_coordinator_ = std::make_unique<MockDisplayCoordinator>(display_info);
    libsync::Completion completion;
    async::PostTask(
        display_coordinator_loop_.dispatcher(),
        [this, &completion, coordinator_server = std::move(coordinator_server)]() mutable {
          mock_coordinator_->Bind(std::move(coordinator_server), {},
                                  display_coordinator_loop_.dispatcher());
          completion.Signal();
        });
    completion.Wait();

    coordinator_proxy_ =
        std::make_unique<CoordinatorProxy>(std::move(coordinator_client), dispatcher());
  }

  void TearDown() override {
    coordinator_proxy_.reset();
    libsync::Completion completion;
    async::PostTask(display_coordinator_loop_.dispatcher(), [this, &completion]() {
      mock_coordinator_.reset();
      completion.Signal();
    });
    completion.Wait();
    display_coordinator_loop_.Quit();
    display_coordinator_loop_.JoinThreads();
    gtest::RealLoopFixture::TearDown();
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
  async::Loop display_coordinator_loop_{&kAsyncLoopConfigNeverAttachToThread};
  std::unique_ptr<MockDisplayCoordinator> mock_coordinator_;
  std::unique_ptr<CoordinatorProxy> coordinator_proxy_;
};

TEST_F(CoordinatorProxyDeathTest, CommitValidatedConfigWithoutValidateAborts) {
  ASSERT_DEATH(coordinator_proxy_->CommitValidatedConfig(WireConfigStamp{.value = 1}),
               "has_validated_layout_");
}

TEST_F(CoordinatorProxyDeathTest, CommitValidatedConfigWithoutImageAborts) {
  const LayerId layer_id = coordinator_proxy_->CreateLayer();
  const Extent2 kExtent({.width = 100, .height = 100});

  ConfigureDisplayedPrimaryLayer(layer_id);
  ASSERT_EQ(coordinator_proxy_->ValidateDraftLayout().status_value(), ZX_OK);

  ASSERT_DEATH(coordinator_proxy_->CommitValidatedConfig(WireConfigStamp{.value = 1}),
               "draft_image_ != kInvalidImageId");

  // Import `kImageA` and commit an initial frame with `kImageA` bound on `layer_id`.
  // `DiscardValidatedLayout()` resets the draft to the (empty) applied state, so the layer config
  // and display layer list must be set again before committing.
  coordinator_proxy_->DiscardValidatedLayout();
  ConfigureDisplayedPrimaryLayer(layer_id);
  const ImageId kImageA(1);
  const WireBufferCollectionId kCollectionId{.value = 1};
  ASSERT_TRUE(coordinator_proxy_->ImportImage(kExtent, 0, kCollectionId, 0, kImageA).is_ok());
  coordinator_proxy_->SetLayerImage(layer_id, kImageA, kInvalidEventId);
  ASSERT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 1}).status_value(), ZX_OK);

  // 1. `SetLayerPrimaryConfig()` with new dimensions clears `draft_image_`; committing without
  // rebinding an image aborts.
  const Extent2 kNewExtent({.width = 120, .height = 120});
  const Rectangle kNewRect({.x = 0, .y = 0, .width = 120, .height = 120});
  coordinator_proxy_->SetLayerPrimaryConfig(layer_id, kNewExtent, /*image_tiling_type=*/0);
  coordinator_proxy_->SetLayerPrimaryPosition(layer_id, RotateFlip::kIdentity(), kNewRect,
                                              kNewRect);
  ASSERT_EQ(coordinator_proxy_->ValidateDraftLayout().status_value(), ZX_OK);
  ASSERT_DEATH(coordinator_proxy_->CommitValidatedConfig(WireConfigStamp{.value = 2}),
               "draft_image_ != kInvalidImageId");
  coordinator_proxy_->DiscardValidatedLayout();

  // 2. `ReleaseImage(kImageA)` while validated clears `draft_image_`; committing without
  // rebinding an image aborts.
  ASSERT_DEATH(
      {
        ASSERT_EQ(coordinator_proxy_->ValidateDraftLayout().status_value(), ZX_OK);
        coordinator_proxy_->ReleaseImage(kImageA);
        coordinator_proxy_->CommitValidatedConfig(WireConfigStamp{.value = 3});
      },
      "draft_image_ != kInvalidImageId");

  // 3. `ReleaseImage(kImageA)` also clears `applied_image_`, so `DiscardValidatedLayout()` copies
  // `kInvalidImageId` back to `draft_image_`; re-validating and committing without rebinding an
  // image aborts.
  ASSERT_EQ(coordinator_proxy_->ValidateDraftLayout().status_value(), ZX_OK);
  coordinator_proxy_->ReleaseImage(kImageA);
  coordinator_proxy_->DiscardValidatedLayout();
  ASSERT_EQ(coordinator_proxy_->ValidateDraftLayout().status_value(), ZX_OK);
  ASSERT_DEATH(coordinator_proxy_->CommitValidatedConfig(WireConfigStamp{.value = 4}),
               "draft_image_ != kInvalidImageId");
}

TEST_F(CoordinatorProxyDeathTest, LayoutSetterWhileValidatedAborts) {
  const LayerId layer_id = coordinator_proxy_->CreateLayer();
  const DisplayId kDisplayId(1);
  const Extent2 kExtent({.width = 100, .height = 100});
  const Rectangle kRect({.x = 0, .y = 0, .width = 100, .height = 100});
  const DisplayMode kMode({.active_area = Extent2({.width = 1024, .height = 768}),
                           .refresh_rate_millihertz = 60000,
                           .mode_flags = 0});
  const std::array<float, 3> kZeroOffsets = {0.f, 0.f, 0.f};
  const std::array<float, 9> kIdentityCoeffs = {1.f, 0.f, 0.f, 0.f, 1.f, 0.f, 0.f, 0.f, 1.f};
  const WireColor kColor = {
      .format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8,
      .bytes = {0, 0, 0, 255},
  };

  ConfigureDisplayedPrimaryLayer(layer_id);
  ASSERT_EQ(coordinator_proxy_->ValidateDraftLayout().status_value(), ZX_OK);
  EXPECT_TRUE(coordinator_proxy_->has_validated_layout());

  EXPECT_DEATH(coordinator_proxy_->SetDisplayMode(kDisplayId, kMode), "!has_validated_layout_");
  EXPECT_DEATH(coordinator_proxy_->SetDisplayColorConversion(kDisplayId, kZeroOffsets,
                                                             kIdentityCoeffs, kZeroOffsets),
               "!has_validated_layout_");
  EXPECT_DEATH(
      coordinator_proxy_->SetDisplayLayers(kDisplayId, std::span<const LayerId>(&layer_id, 1)),
      "!has_validated_layout_");
  EXPECT_DEATH(
      coordinator_proxy_->SetLayerPrimaryConfig(layer_id, kExtent, /*image_tiling_type=*/0),
      "!has_validated_layout_");
  EXPECT_DEATH(
      coordinator_proxy_->SetLayerPrimaryPosition(layer_id, RotateFlip::kIdentity(), kRect, kRect),
      "!has_validated_layout_");
  EXPECT_DEATH(coordinator_proxy_->SetLayerPrimaryAlpha(layer_id, BlendMode::kReplace(), 1.0f),
               "!has_validated_layout_");
  EXPECT_DEATH(coordinator_proxy_->SetLayerColorConfig(layer_id, kColor, kRect),
               "!has_validated_layout_");
}

TEST_F(CoordinatorProxyDeathTest, DestroyDisplayedOrUnknownLayerAborts) {
  const DisplayId kDisplayId(1);
  const Extent2 kExtent({.width = 100, .height = 100});

  // (a) Unknown layer ID never created by `CreateLayer()`.
  EXPECT_DEATH(coordinator_proxy_->DestroyLayer(LayerId(999)), "layers_\\.contains\\(layer_id\\)");

  // (b) Layer present only in the draft display list (not yet committed).
  const LayerId draft_layer = coordinator_proxy_->CreateLayer();
  coordinator_proxy_->SetDisplayLayers(kDisplayId, std::span<const LayerId>(&draft_layer, 1));
  EXPECT_DEATH(coordinator_proxy_->DestroyLayer(draft_layer), "draft_display_layers_");

  // Clear `draft_layer` from the draft list so we can set up case (c) cleanly.
  coordinator_proxy_->SetDisplayLayers(kDisplayId, {});

  // (c) Layer present in the applied display list, after being removed from the draft list without
  // committing yet (isolates the `applied_display_layers_` check).
  const LayerId applied_layer = coordinator_proxy_->CreateLayer();
  ConfigureDisplayedPrimaryLayer(applied_layer);
  const ImageId kImageId(1);
  ASSERT_TRUE(
      coordinator_proxy_->ImportImage(kExtent, 0, WireBufferCollectionId{.value = 1}, 0, kImageId)
          .is_ok());
  coordinator_proxy_->SetLayerImage(applied_layer, kImageId, kInvalidEventId);
  ASSERT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 1}).status_value(), ZX_OK);

  coordinator_proxy_->SetDisplayLayers(kDisplayId, {});
  EXPECT_DEATH(coordinator_proxy_->DestroyLayer(applied_layer), "applied_display_layers_");
}

TEST_F(CoordinatorProxyDeathTest, SetLayerImageWithInvalidImageIdAborts) {
  const LayerId layer_id = coordinator_proxy_->CreateLayer();
  ASSERT_DEATH(coordinator_proxy_->SetLayerImage(layer_id, kInvalidImageId, kInvalidEventId),
               "image_id != kInvalidImageId");
}

TEST_F(CoordinatorProxyDeathTest, ReleaseImageWhileAsyncImportPendingAborts) {
  libsync::Completion configured;
  async::PostTask(display_coordinator_loop_.dispatcher(), [this, &configured] {
    mock_coordinator_->set_import_image_result(ZX_ERR_NOT_SUPPORTED);
    configured.Signal();
  });
  configured.Wait();

  const ImageId kImageId(1);
  coordinator_proxy_->ImportImage(Extent2({.width = 100, .height = 100}), /*image_tiling_type=*/0,
                                  WireBufferCollectionId{.value = 1}, /*buffer_index=*/0, kImageId,
                                  [](zx::result<>) {});

  // Wait until the server thread has processed `ImportImage()` and queued the error reply on the
  // channel, before pumping the client loop inside the forked death-test child.
  auto _ = coordinator_proxy_->raw().sync()->GetLatestCommittedConfigStamp();

  ASSERT_DEATH(
      {
        coordinator_proxy_->ReleaseImage(kImageId);
        RunLoopUntilIdle();
      },
      "ReleaseImage\\(\\) was called while the import was pending");

  // Drain the pending error reply cleanly in the parent process before teardown.
  RunLoopUntilIdle();
}

TEST_F(CoordinatorProxyDeathTest, DuplicateImportEventAndInvalidReleaseEventAbort) {
  zx::event event1;
  zx::event event2;
  ASSERT_EQ(zx::event::create(0, &event1), ZX_OK);
  ASSERT_EQ(zx::event::create(0, &event2), ZX_OK);

  const EventId kEventId(10);
  coordinator_proxy_->ImportEvent(std::move(event1), kEventId);

  ASSERT_DEATH(coordinator_proxy_->ImportEvent(std::move(event2), kEventId),
               "!events_\\.contains\\(event_id\\)");
  ASSERT_DEATH(coordinator_proxy_->ReleaseEvent(EventId(999)), "Expected to find event_id=");

  coordinator_proxy_->ReleaseEvent(kEventId);
  ASSERT_DEATH(coordinator_proxy_->ReleaseEvent(kEventId), "Expected to find event_id=");
}

// TODO(https://fxbug.dev/580156981): Remove this test together with the synchronous `ImportImage()`
// overload.
TEST_F(CoordinatorProxyDeathTest, SyncImportImageErrorsDoNotRecordImage) {
  const Extent2 kExtent({.width = 100, .height = 100});
  const WireBufferCollectionId kCollectionId{.value = 1};

  libsync::Completion set_error;
  async::PostTask(display_coordinator_loop_.dispatcher(), [this, &set_error] {
    mock_coordinator_->set_import_image_result(ZX_ERR_NOT_SUPPORTED);
    set_error.Signal();
  });
  set_error.Wait();

  const zx::result<> domain_result =
      coordinator_proxy_->ImportImage(kExtent, /*image_tiling_type=*/0, kCollectionId,
                                      /*buffer_index=*/0, ImageId(1));
  EXPECT_EQ(domain_result.status_value(), ZX_ERR_NOT_SUPPORTED);
  EXPECT_EQ(coordinator_proxy_->current_image_count(), 0u);

  libsync::Completion closed;
  async::PostTask(display_coordinator_loop_.dispatcher(), [this, &closed] {
    mock_coordinator_->set_import_image_result(ZX_OK);
    mock_coordinator_->ResetCoordinatorBinding();
    closed.Signal();
  });
  closed.Wait();

  const zx::result<> peer_closed_result =
      coordinator_proxy_->ImportImage(kExtent, /*image_tiling_type=*/0, kCollectionId,
                                      /*buffer_index=*/1, ImageId(2));
  EXPECT_TRUE(peer_closed_result.is_error());
  EXPECT_EQ(coordinator_proxy_->current_image_count(), 0u);
}

TEST_F(CoordinatorProxyDeathTest, OverwritePendingOnFidlErrorCallbackAborts) {
  coordinator_proxy_->SetOnFidlErrorCallback([](fidl::UnbindInfo) {});
  EXPECT_DEATH(coordinator_proxy_->SetOnFidlErrorCallback([](fidl::UnbindInfo) {}),
               "cannot overwrite an existing callback without clearing first");
  coordinator_proxy_->SetOnFidlErrorCallback(nullptr);
}

}  // namespace display::test
