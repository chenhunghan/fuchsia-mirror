// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <memory>
#include <optional>
#include <string>
#include <vector>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/ui/scenic/lib/display/fidl_id_types.h"
#include "src/ui/scenic/lib/flatland/engine/engine.h"
#include "src/ui/scenic/lib/flatland/global_resolved_layers.h"
#include "src/ui/scenic/lib/flatland/uber_struct.h"
#include "src/ui/scenic/lib/types/blend_mode.h"
#include "src/ui/scenic/lib/types/rectangle.h"
#include "src/ui/scenic/lib/types/rectangle_f.h"
#include "src/ui/scenic/lib/types/rotate_flip.h"

#include <glm/gtc/constants.hpp>
#include <glm/gtx/matrix_transform_2d.hpp>

namespace flatland::test {
namespace {

using types::BlendMode;
using types::Rectangle;
using types::RectangleF;
using types::RotateFlip;

constexpr TransformHandle::InstanceId kLinkSystemId = 999;
constexpr uint64_t kLinkTopologyGeneration = 1;

class SceneStateTest : public ::testing::Test {
 protected:
  static UberStructSnapshot MakeBaseSnapshot() {
    const TransformHandle kRoot(1, 0);
    const TransformHandle kChild(1, 1);
    const LayerHandle kLayer1(1, 1);
    const LayerHandle kLayer2(1, 2);

    auto uber = std::make_shared<UberStruct>();
    uber->local_topology = {{kRoot, 1}, {kChild, 0}};
    uber->local_matrices[kChild] = glm::translate(glm::mat3(1.f), {100.f, 200.f}) *
                                   glm::rotate(glm::mat3(1.f), -glm::half_pi<float>());
    uber->local_clip_regions[kRoot] = Rectangle({.x = 0, .y = 0, .width = 1000, .height = 1000});
    uber->local_opacity_values[kChild] = 0.8f;

    uber->layer_stacks[kChild] = {kLayer1, kLayer2};
    uber->layers[kLayer1] = UberStructLayer{
        .content =
            UberStructLayer::ImageModeProperties{
                .sample_rect = RectangleF({.x = 0.f, .y = 0.f, .width = 100.f, .height = 200.f}),
                .transform = RotateFlip::kRotateCcw90(),
                .image_id = display::ImageId(10),
            },
        .common =
            {
                .display_rect = Rectangle({.x = 10, .y = 20, .width = 50, .height = 80}),
                .opacity = 0.5f,
                .blend_mode = BlendMode::kPremultipliedAlpha(),
            },
    };
    uber->layers[kLayer2] = UberStructLayer{
        .content =
            UberStructLayer::SolidColorModeProperties{
                .color = {1.f, 0.f, 0.f, 1.f},
            },
        .common =
            {
                .display_rect = Rectangle({.x = 0, .y = 0, .width = 30, .height = 40}),
                .opacity = 1.0f,
                .blend_mode = BlendMode::kPremultipliedAlpha(),
            },
    };

    UberStructSnapshot snapshot;
    snapshot.map[1] = std::move(uber);
    return snapshot;
  }

  static std::shared_ptr<UberStruct> CloneEngineInputs(const UberStruct& src) {
    auto copy = std::make_shared<UberStruct>();
    copy->flatland_version = src.flatland_version;
    copy->local_topology = src.local_topology;
    copy->local_matrices = src.local_matrices;
    copy->local_clip_regions = src.local_clip_regions;
    copy->local_opacity_values = src.local_opacity_values;
    copy->layer_stacks = src.layer_stacks;
    copy->layers = src.layers;
    return copy;
  }
};

// When `needs_full_rebuild` is false on a layer-only update:
// - `rebuild_count` is unchanged;
// - cached transform-stage vectors (`topology_vector`, `global_matrices`, `clip_regions`,
//   `opacities`, `resolved_layer_stacks`) retain their values and buffer allocations untouched;
// - `scene_state.snapshot` is refreshed so `ComputeGlobalResolvedLayers` produces output
//   field-identical to a full rebuild.
TEST_F(SceneStateTest, LayerOnlyFrameReusesTransformState) {
  const TransformHandle kRoot(1, 0);
  const LayerHandle kLayer1(1, 1);
  const GlobalTopologyData::LinkTopologyMap kEmptyLinks;

  Engine::SceneState cached_state;
  const UberStructSnapshot initial_snapshot = MakeBaseSnapshot();
  Engine::PrepareSceneState(cached_state, initial_snapshot, kEmptyLinks, kLinkTopologyGeneration,
                            kLinkSystemId, kRoot,
                            /*needs_full_rebuild=*/true);
  EXPECT_EQ(cached_state.rebuild_count, 1u);
  ASSERT_EQ(cached_state.resolved_layer_stacks.size(), 1u);

  const auto* initial_topology_ptr = cached_state.topology_data.topology_vector.data();
  const auto* initial_matrices_ptr = cached_state.global_matrices.data();
  const auto* initial_clips_ptr = cached_state.clip_regions.data();
  const auto* initial_opacities_ptr = cached_state.opacities.data();
  const auto* initial_stacks_ptr = cached_state.resolved_layer_stacks.data();

  // Mutate only layer properties in a new snapshot while keeping all transform inputs identical.
  auto updated_uber = CloneEngineInputs(*initial_snapshot.map.at(1));
  updated_uber->layers[kLayer1] = UberStructLayer{
      .content =
          UberStructLayer::ImageModeProperties{
              .sample_rect = RectangleF({.x = 5.f, .y = 10.f, .width = 60.f, .height = 120.f}),
              .transform = RotateFlip::kReflectY(),
              .image_id = display::ImageId(77),
          },
      .common =
          {
              .display_rect = Rectangle({.x = 25, .y = 35, .width = 70, .height = 90}),
              .opacity = 0.75f,
              .blend_mode = BlendMode::kPremultipliedAlpha(),
          },
  };
  UberStructSnapshot layer_only_snapshot;
  layer_only_snapshot.map[1] = updated_uber;

  Engine::PrepareSceneState(cached_state, layer_only_snapshot, kEmptyLinks,
                            kLinkTopologyGeneration + 1, kLinkSystemId, kRoot,
                            /*needs_full_rebuild=*/false);

  // `rebuild_count` and buffer addresses are unchanged.
  EXPECT_EQ(cached_state.rebuild_count, 1u);
  EXPECT_EQ(cached_state.topology_data.topology_vector.data(), initial_topology_ptr);
  EXPECT_EQ(cached_state.global_matrices.data(), initial_matrices_ptr);
  EXPECT_EQ(cached_state.clip_regions.data(), initial_clips_ptr);
  EXPECT_EQ(cached_state.opacities.data(), initial_opacities_ptr);
  EXPECT_EQ(cached_state.resolved_layer_stacks.data(), initial_stacks_ptr);
  EXPECT_EQ(cached_state.snapshot.map.at(1).get(), updated_uber.get());
  // The reuse arm stores the generation passed in.
  EXPECT_EQ(cached_state.link_topology_generation, kLinkTopologyGeneration + 1);

  // Compare layer-stage output against a reference full rebuild.
  Engine::SceneState rebuilt_state;
  Engine::PrepareSceneState(rebuilt_state, layer_only_snapshot, kEmptyLinks,
                            kLinkTopologyGeneration, kLinkSystemId, kRoot,
                            /*needs_full_rebuild=*/true);

  const auto reused_layers =
      ComputeGlobalResolvedLayers(cached_state.resolved_layer_stacks, cached_state.snapshot.map);
  const auto rebuilt_layers =
      ComputeGlobalResolvedLayers(rebuilt_state.resolved_layer_stacks, rebuilt_state.snapshot.map);
  ASSERT_EQ(reused_layers.size(), 2u);
  EXPECT_EQ(reused_layers, rebuilt_layers);
}

// When `needs_full_rebuild` is true:
// - `rebuild_count` advances;
// - transform-stage state (`topology_data`, `global_matrices`, `resolved_layer_stacks`, etc.) is
//   rebuilt in place.
TEST_F(SceneStateTest, TransformFrameRebuildsTransformState) {
  const TransformHandle kRoot(1, 0);
  const TransformHandle kChild(1, 1);
  const GlobalTopologyData::LinkTopologyMap kEmptyLinks;

  Engine::SceneState scene_state;
  const UberStructSnapshot initial_snapshot = MakeBaseSnapshot();
  Engine::PrepareSceneState(scene_state, initial_snapshot, kEmptyLinks, kLinkTopologyGeneration,
                            kLinkSystemId, kRoot,
                            /*needs_full_rebuild=*/true);
  EXPECT_EQ(scene_state.rebuild_count, 1u);

  // Mutate child transform matrix and opacity and rebuild with `needs_full_rebuild = true`.
  auto updated_uber = CloneEngineInputs(*initial_snapshot.map.at(1));
  updated_uber->local_matrices[kChild] = glm::translate(glm::mat3(1.f), {300.f, 400.f});
  updated_uber->local_opacity_values[kChild] = 0.25f;

  UberStructSnapshot updated_snapshot;
  updated_snapshot.map[1] = updated_uber;

  Engine::PrepareSceneState(scene_state, updated_snapshot, kEmptyLinks, kLinkTopologyGeneration + 1,
                            kLinkSystemId, kRoot,
                            /*needs_full_rebuild=*/true);
  EXPECT_EQ(scene_state.rebuild_count, 2u);
  // The rebuild arm stores the generation passed in.
  EXPECT_EQ(scene_state.link_topology_generation, kLinkTopologyGeneration + 1);
  ASSERT_EQ(scene_state.resolved_layer_stacks.size(), 1u);
  EXPECT_EQ(scene_state.resolved_layer_stacks[0].node_rotation, RotateFlip::kIdentity());
  EXPECT_FLOAT_EQ(scene_state.resolved_layer_stacks[0].opacity, 0.25f);
  EXPECT_EQ(scene_state.resolved_layer_stacks[0].global_matrix,
            glm::translate(glm::mat3(1.f), {300.f, 400.f}));
}

// `FindStaleSceneStateInput()` finds nothing when only layer data changed, and names each
// transform-stage input when that input alone changed.  This runs in every build type; debug
// builds also DCHECK on the function in `PrepareSceneState()`.
TEST_F(SceneStateTest, FindStaleSceneStateInputNamesEachInput) {
  const TransformHandle kRoot(1, 0);
  const TransformHandle kChild(1, 1);
  const TransformHandle kExtra(1, 2);
  const LayerHandle kLayer1(1, 1);
  const GlobalTopologyData::LinkTopologyMap kEmptyLinks;

  Engine::SceneState scene_state;
  const UberStructSnapshot initial_snapshot = MakeBaseSnapshot();
  const UberStruct& initial_uber = *initial_snapshot.map.at(1);

  // A cleared `SceneState` reports "scene state is cleared".
  {
    const std::optional<std::string> stale =
        Engine::FindStaleSceneStateInput(scene_state, initial_snapshot, kEmptyLinks, kRoot);
    ASSERT_TRUE(stale.has_value());
    EXPECT_THAT(*stale, ::testing::HasSubstr("scene state is cleared"));
  }

  // If the previous rebuild produced an empty `topology_vector` (because the root session had not
  // yet published an `UberStruct`), switching `root_transform` to a session already present in
  // `snapshot.map` reports "root transform changed", while keeping an unpublished root does not.
  {
    const TransformHandle kUnpublishedRoot(99, 0);
    Engine::SceneState empty_topology_state;
    Engine::PrepareSceneState(empty_topology_state, initial_snapshot, kEmptyLinks,
                              kLinkTopologyGeneration, kLinkSystemId, kUnpublishedRoot,
                              /*needs_full_rebuild=*/true);
    ASSERT_TRUE(empty_topology_state.topology_data.topology_vector.empty());
    EXPECT_EQ(Engine::FindStaleSceneStateInput(empty_topology_state, initial_snapshot, kEmptyLinks,
                                               kUnpublishedRoot),
              std::nullopt);
    const std::optional<std::string> stale = Engine::FindStaleSceneStateInput(
        empty_topology_state, initial_snapshot, kEmptyLinks, kRoot);
    ASSERT_TRUE(stale.has_value());
    EXPECT_THAT(*stale, ::testing::HasSubstr("root transform changed"));
  }

  Engine::PrepareSceneState(scene_state, initial_snapshot, kEmptyLinks, kLinkTopologyGeneration,
                            kLinkSystemId, kRoot,
                            /*needs_full_rebuild=*/true);

  // Returns a snapshot holding only `uber`, as session `id`.
  const auto snapshot_with = [](std::shared_ptr<UberStruct> uber, scheduling::SessionId id = 1) {
    UberStructSnapshot snapshot;
    snapshot.map[id] = std::move(uber);
    return snapshot;
  };

  // Nothing is stale when the snapshot is unchanged, or when only a layer changed.
  EXPECT_EQ(Engine::FindStaleSceneStateInput(scene_state, initial_snapshot, kEmptyLinks, kRoot),
            std::nullopt);
  {
    auto layer_only = CloneEngineInputs(initial_uber);
    layer_only->layers[kLayer1].common.opacity = 0.25f;
    EXPECT_EQ(Engine::FindStaleSceneStateInput(scene_state, snapshot_with(layer_only), kEmptyLinks,
                                               kRoot),
              std::nullopt);
  }

  struct Case {
    const char* name;
    UberStructSnapshot snapshot;
    GlobalTopologyData::LinkTopologyMap links;
    TransformHandle root;
    const char* expected;
  };
  std::vector<Case> cases;
  {
    GlobalTopologyData::LinkTopologyMap links;
    links[TransformHandle(kLinkSystemId, 1)] = TransformHandle(2, 0);
    cases.push_back({
        .name = "links",
        .snapshot = initial_snapshot,
        .links = links,
        .root = kRoot,
        .expected = "links map changed",
    });
  }
  cases.push_back({
      .name = "root",
      .snapshot = initial_snapshot,
      .links = kEmptyLinks,
      .root = kChild,
      .expected = "root transform changed",
  });
  {
    UberStructSnapshot snapshot = initial_snapshot;
    snapshot.map[2] = CloneEngineInputs(initial_uber);
    cases.push_back({
        .name = "session count",
        .snapshot = snapshot,
        .links = kEmptyLinks,
        .root = kRoot,
        .expected = "session count changed",
    });
  }
  cases.push_back({
      .name = "new session",
      .snapshot = snapshot_with(CloneEngineInputs(initial_uber), /*id=*/2),
      .links = kEmptyLinks,
      .root = kRoot,
      .expected = "new session 2 appeared",
  });
  {
    auto uber = CloneEngineInputs(initial_uber);
    uber->local_topology = {{kRoot, 2}, {kChild, 0}, {kExtra, 0}};
    cases.push_back({
        .name = "local_topology",
        .snapshot = snapshot_with(uber),
        .links = kEmptyLinks,
        .root = kRoot,
        .expected = "local_topology changed",
    });
  }
  {
    auto uber = CloneEngineInputs(initial_uber);
    uber->local_matrices[kChild] = glm::translate(glm::mat3(1.f), {999.f, 999.f});
    cases.push_back({
        .name = "local_matrices",
        .snapshot = snapshot_with(uber),
        .links = kEmptyLinks,
        .root = kRoot,
        .expected = "local_matrices changed",
    });
  }
  {
    auto uber = CloneEngineInputs(initial_uber);
    uber->local_matrices[kRoot] = glm::mat3(1.f);
    cases.push_back({
        .name = "local_matrices insert",
        .snapshot = snapshot_with(uber),
        .links = kEmptyLinks,
        .root = kRoot,
        .expected = "local_matrices changed",
    });
  }
  {
    auto uber = CloneEngineInputs(initial_uber);
    uber->local_matrices.erase(kChild);
    cases.push_back({
        .name = "local_matrices erase",
        .snapshot = snapshot_with(uber),
        .links = kEmptyLinks,
        .root = kRoot,
        .expected = "local_matrices changed",
    });
  }
  {
    auto uber = CloneEngineInputs(initial_uber);
    uber->local_clip_regions[kRoot] = Rectangle({.x = 0, .y = 0, .width = 50, .height = 50});
    cases.push_back({
        .name = "local_clip_regions",
        .snapshot = snapshot_with(uber),
        .links = kEmptyLinks,
        .root = kRoot,
        .expected = "local_clip_regions changed",
    });
  }
  {
    auto uber = CloneEngineInputs(initial_uber);
    uber->local_clip_regions[kChild] = Rectangle({.x = 0, .y = 0, .width = 50, .height = 50});
    cases.push_back({
        .name = "local_clip_regions insert",
        .snapshot = snapshot_with(uber),
        .links = kEmptyLinks,
        .root = kRoot,
        .expected = "local_clip_regions changed",
    });
  }
  {
    auto uber = CloneEngineInputs(initial_uber);
    uber->local_clip_regions.erase(kRoot);
    cases.push_back({
        .name = "local_clip_regions erase",
        .snapshot = snapshot_with(uber),
        .links = kEmptyLinks,
        .root = kRoot,
        .expected = "local_clip_regions changed",
    });
  }
  {
    auto uber = CloneEngineInputs(initial_uber);
    uber->local_opacity_values[kChild] = 0.1f;
    cases.push_back({
        .name = "local_opacity_values",
        .snapshot = snapshot_with(uber),
        .links = kEmptyLinks,
        .root = kRoot,
        .expected = "local_opacity_values changed",
    });
  }
  {
    auto uber = CloneEngineInputs(initial_uber);
    uber->local_opacity_values[kRoot] = 0.5f;
    cases.push_back({
        .name = "local_opacity_values insert",
        .snapshot = snapshot_with(uber),
        .links = kEmptyLinks,
        .root = kRoot,
        .expected = "local_opacity_values changed",
    });
  }
  {
    auto uber = CloneEngineInputs(initial_uber);
    uber->local_opacity_values.erase(kChild);
    cases.push_back({
        .name = "local_opacity_values erase",
        .snapshot = snapshot_with(uber),
        .links = kEmptyLinks,
        .root = kRoot,
        .expected = "local_opacity_values changed",
    });
  }
  {
    // `kRoot` starts hosting a stack; no per-session transform field changes.
    auto uber = CloneEngineInputs(initial_uber);
    uber->layer_stacks[kRoot] = {kLayer1};
    cases.push_back({
        .name = "stack-hosting set",
        .snapshot = snapshot_with(uber),
        .links = kEmptyLinks,
        .root = kRoot,
        .expected = "set of stack-hosting transforms changed",
    });
  }

  for (const auto& test_case : cases) {
    SCOPED_TRACE(test_case.name);
    const std::optional<std::string> stale = Engine::FindStaleSceneStateInput(
        scene_state, test_case.snapshot, test_case.links, test_case.root);
    ASSERT_TRUE(stale.has_value());
    EXPECT_THAT(*stale, ::testing::HasSubstr(test_case.expected));
  }
}

// `cleared` is set on construction, reset by a rebuild, and set again by `Clear()`.
TEST_F(SceneStateTest, ClearedFollowsConstructionRebuildAndClear) {
  const TransformHandle kRoot(1, 0);
  const GlobalTopologyData::LinkTopologyMap kEmptyLinks;

  Engine::SceneState scene_state;
  EXPECT_TRUE(scene_state.cleared);

  Engine::PrepareSceneState(scene_state, MakeBaseSnapshot(), kEmptyLinks, kLinkTopologyGeneration,
                            kLinkSystemId, kRoot,
                            /*needs_full_rebuild=*/true);
  EXPECT_FALSE(scene_state.cleared);

  scene_state.Clear();
  EXPECT_TRUE(scene_state.cleared);
  EXPECT_EQ(scene_state.link_topology_generation, 0u);
}

// `links == std::nullopt` keeps the cached links, whether the state is reused or rebuilt.
TEST_F(SceneStateTest, NulloptLinksKeepCachedLinks) {
  const TransformHandle kRoot(1, 0);
  GlobalTopologyData::LinkTopologyMap links;
  // A link whose handle is not in the topology, so it does not change what is built.
  links[TransformHandle(kLinkSystemId, 1)] = TransformHandle(2, 0);

  Engine::SceneState scene_state;
  Engine::PrepareSceneState(scene_state, MakeBaseSnapshot(), links, kLinkTopologyGeneration,
                            kLinkSystemId, kRoot, /*needs_full_rebuild=*/true);
  ASSERT_EQ(scene_state.links, links);

  Engine::PrepareSceneState(scene_state, MakeBaseSnapshot(), std::nullopt, kLinkTopologyGeneration,
                            kLinkSystemId, kRoot, /*needs_full_rebuild=*/false);
  EXPECT_EQ(scene_state.rebuild_count, 1u);
  EXPECT_EQ(scene_state.links, links);

  Engine::PrepareSceneState(scene_state, MakeBaseSnapshot(), std::nullopt, kLinkTopologyGeneration,
                            kLinkSystemId, kRoot, /*needs_full_rebuild=*/true);
  EXPECT_EQ(scene_state.rebuild_count, 2u);
  EXPECT_EQ(scene_state.links, links);
}

// A cleared `SceneState` holds no links, so preparing it with `links == std::nullopt` fails an
// `FX_CHECK` in every build type.
TEST_F(SceneStateTest, ClearedStateWithoutLinksDies) {
  const TransformHandle kRoot(1, 0);

  Engine::SceneState scene_state;
  EXPECT_DEATH(Engine::PrepareSceneState(scene_state, MakeBaseSnapshot(), std::nullopt,
                                         kLinkTopologyGeneration, kLinkSystemId, kRoot,
                                         /*needs_full_rebuild=*/true),
               "cleared scene state holds no links");
}

// Reusing a `SceneState` that describes no frame fails an `FX_CHECK`, so this runs in every build
// type.
TEST_F(SceneStateTest, ReuseOfClearedStateDies) {
  const TransformHandle kRoot(1, 0);
  const GlobalTopologyData::LinkTopologyMap kEmptyLinks;

  Engine::SceneState scene_state;
  EXPECT_DEATH(Engine::PrepareSceneState(scene_state, MakeBaseSnapshot(), kEmptyLinks,
                                         kLinkTopologyGeneration, kLinkSystemId, kRoot,
                                         /*needs_full_rebuild=*/false),
               "scene state is cleared");
}

// Entries of `local_matrices`, `local_clip_regions`, and `local_opacity_values` for a transform
// absent from `local_topology` are not transform inputs: removing or adding them with
// `needs_full_rebuild = false` passes `FindStaleSceneStateInput()` and keeps the cached state.
TEST_F(SceneStateTest, UnreachableEntriesAreNotTransformInputs) {
  const TransformHandle kRoot(1, 0);
  const TransformHandle kUnreachable(1, 5);
  const GlobalTopologyData::LinkTopologyMap kEmptyLinks;

  const UberStructSnapshot base_snapshot = MakeBaseSnapshot();
  auto uber_with_unreachable = CloneEngineInputs(*base_snapshot.map.at(1));
  uber_with_unreachable->local_matrices[kUnreachable] = glm::translate(glm::mat3(1.f), {7.f, 8.f});
  uber_with_unreachable->local_clip_regions[kUnreachable] =
      Rectangle({.x = 0, .y = 0, .width = 5, .height = 5});
  uber_with_unreachable->local_opacity_values[kUnreachable] = 0.3f;
  UberStructSnapshot snapshot_with_unreachable;
  snapshot_with_unreachable.map[1] = uber_with_unreachable;

  Engine::SceneState scene_state;
  Engine::PrepareSceneState(scene_state, snapshot_with_unreachable, kEmptyLinks,
                            kLinkTopologyGeneration, kLinkSystemId, kRoot,
                            /*needs_full_rebuild=*/true);
  ASSERT_EQ(scene_state.rebuild_count, 1u);
  const auto expected_layers =
      ComputeGlobalResolvedLayers(scene_state.resolved_layer_stacks, scene_state.snapshot.map);
  ASSERT_EQ(expected_layers.size(), 2u);

  // 1. The entries of the unreachable transform are removed.
  EXPECT_EQ(Engine::FindStaleSceneStateInput(scene_state, base_snapshot, kEmptyLinks, kRoot),
            std::nullopt);
  Engine::PrepareSceneState(scene_state, base_snapshot, kEmptyLinks, kLinkTopologyGeneration,
                            kLinkSystemId, kRoot,
                            /*needs_full_rebuild=*/false);
  EXPECT_EQ(scene_state.rebuild_count, 1u);
  EXPECT_EQ(
      ComputeGlobalResolvedLayers(scene_state.resolved_layer_stacks, scene_state.snapshot.map),
      expected_layers);

  // 2. The entries of the unreachable transform are added.
  EXPECT_EQ(
      Engine::FindStaleSceneStateInput(scene_state, snapshot_with_unreachable, kEmptyLinks, kRoot),
      std::nullopt);
  Engine::PrepareSceneState(scene_state, snapshot_with_unreachable, kEmptyLinks,
                            kLinkTopologyGeneration, kLinkSystemId, kRoot,
                            /*needs_full_rebuild=*/false);
  EXPECT_EQ(scene_state.rebuild_count, 1u);
  EXPECT_EQ(
      ComputeGlobalResolvedLayers(scene_state.resolved_layer_stacks, scene_state.snapshot.map),
      expected_layers);
}

// Mutating the layer contents of an existing stack-hosting transform is a layer-only change, not a
// transform input: `FindStaleSceneStateInput()` returns std::nullopt.
TEST_F(SceneStateTest, StackLayerContentsAreNotTransformInputs) {
  const TransformHandle kRoot(1, 0);
  const TransformHandle kChild(1, 1);
  const LayerHandle kLayer2(1, 2);
  const GlobalTopologyData::LinkTopologyMap kEmptyLinks;

  Engine::SceneState scene_state;
  const UberStructSnapshot initial_snapshot = MakeBaseSnapshot();
  Engine::PrepareSceneState(scene_state, initial_snapshot, kEmptyLinks, kLinkTopologyGeneration,
                            kLinkSystemId, kRoot,
                            /*needs_full_rebuild=*/true);

  auto updated_uber = CloneEngineInputs(*initial_snapshot.map.at(1));
  updated_uber->layer_stacks[kChild] = {kLayer2};

  UberStructSnapshot updated_snapshot;
  updated_snapshot.map[1] = std::move(updated_uber);

  EXPECT_EQ(Engine::FindStaleSceneStateInput(scene_state, updated_snapshot, kEmptyLinks, kRoot),
            std::nullopt);
}

}  // namespace
}  // namespace flatland::test
