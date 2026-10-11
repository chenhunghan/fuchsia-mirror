// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/syslog/cpp/macros.h>

#include <memory>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/ui/scenic/lib/flatland/flatland_types.h"
#include "src/ui/scenic/lib/flatland/global_matrix_data.h"
#include "src/ui/scenic/lib/flatland/global_resolved_layers.h"
#include "src/ui/scenic/lib/flatland/global_topology_data.h"

#include <glm/glm.hpp>
#include <glm/gtc/constants.hpp>
#include <glm/gtx/matrix_transform_2d.hpp>

namespace flatland {
namespace test {

namespace {

using fuchsia_ui_composition::Orientation;

// Helper function for getting the correct rotation angle. Matrices are specified in view-space
// coordinates, in which the +y axis points downwards (not upwards). Rotations which are specified
// as counter-clockwise must actually occur in a clockwise fashion in this coordinate space (a
// vector on the +x axis rotates towards -y axis to give the appearance of a counter-clockwise
// rotation).
float GetOrientationAngleInViewSpaceCoordinates(Orientation angle) {
  switch (angle) {
    case Orientation::kCcw90Degrees:
      return -glm::half_pi<float>();
    case Orientation::kCcw180Degrees:
      return -glm::pi<float>();
    case Orientation::kCcw270Degrees:
      return -glm::three_over_two_pi<float>();
    case Orientation::kCcw0Degrees:
      return 0.f;
  }
}

}  // namespace

// The following tests ensure the transform hierarchy is properly reflected in the list of global
// rectangles.

TEST(GlobalMatrixDataTest, EmptyTopologyReturnsEmptyMatrices) {
  UberStruct::InstanceMap uber_structs;
  GlobalTopologyData::TopologyVector topology_vector;
  GlobalTopologyData::ParentIndexVector parent_indices;

  auto global_matrices = ComputeGlobalMatrices(topology_vector, parent_indices, uber_structs);
  EXPECT_TRUE(global_matrices.empty());
}

TEST(GlobalMatrixDataTest, EmptyLocalMatricesAreIdentity) {
  UberStruct::InstanceMap uber_structs;

  // Make a global topology representing the following graph:
  //
  // 1:0 - 1:1
  GlobalTopologyData::TopologyVector topology_vector = {{1, 0}, {1, 1}};
  GlobalTopologyData::ParentIndexVector parent_indices = {0, 0};

  // The UberStruct for instance ID 1 must exist, but it contains no local matrices.
  auto uber_struct = std::make_unique<UberStruct>();
  uber_structs[1] = std::move(uber_struct);

  // The root matrix is set to the identity matrix, and the second inherits that.
  std::vector<glm::mat3> expected_matrices = {
      glm::mat3(),
      glm::mat3(),
  };

  auto global_matrices = ComputeGlobalMatrices(topology_vector, parent_indices, uber_structs);
  EXPECT_THAT(global_matrices, ::testing::ElementsAreArray(expected_matrices));
}

TEST(GlobalMatrixDataTest, GlobalMatricesIncludeParentMatrix) {
  UberStruct::InstanceMap uber_structs;

  // Make a global topology representing the following graph:
  //
  //    1:0 - 1:1 - 1:2
  //       \
  //       1:3 - 1:4
  GlobalTopologyData::TopologyVector topology_vector = {{1, 0}, {1, 1}, {1, 2}, {1, 3}, {1, 4}};
  GlobalTopologyData::ParentIndexVector parent_indices = {0, 0, 1, 0, 3};

  auto uber_struct = std::make_unique<UberStruct>();

  static const glm::vec2 kTranslation = {1.f, 2.f};
  static const float kRotation = glm::half_pi<float>();
  static const glm::vec2 kScale = {3.f, 5.f};

  // All transforms will get the translation from 1:0
  uber_struct->local_matrices[{1, 0}] = glm::translate(glm::mat3(), kTranslation);

  // The 1:1 - 1:2 branch rotates, then scales.
  uber_struct->local_matrices[{1, 1}] = glm::rotate(glm::mat3(), kRotation);
  uber_struct->local_matrices[{1, 2}] = glm::scale(glm::mat3(), kScale);

  // The 1:3 - 1:4 branch scales, then rotates.
  uber_struct->local_matrices[{1, 3}] = glm::scale(glm::mat3(), kScale);
  uber_struct->local_matrices[{1, 4}] = glm::rotate(glm::mat3(), kRotation);

  uber_structs[1] = std::move(uber_struct);

  // The expected matrices apply the operations in the correct order. The translation always comes
  // first, followed by the operations of the children.
  std::vector<glm::mat3> expected_matrices = {
      glm::translate(glm::mat3(), kTranslation),
      glm::rotate(glm::translate(glm::mat3(), kTranslation), kRotation),
      glm::scale(glm::rotate(glm::translate(glm::mat3(), kTranslation), kRotation), kScale),
      glm::scale(glm::translate(glm::mat3(), kTranslation), kScale),
      glm::rotate(glm::scale(glm::translate(glm::mat3(), kTranslation), kScale), kRotation),
  };

  auto global_matrices = ComputeGlobalMatrices(topology_vector, parent_indices, uber_structs);
  EXPECT_THAT(global_matrices, ::testing::ElementsAreArray(expected_matrices));
}

TEST(GlobalMatrixDataTest, GlobalMatricesMultipleUberStructs) {
  UberStruct::InstanceMap uber_structs;

  // Make a global topology representing the following graph:
  //
  // 1:0 - 2:0
  //     \
  //       1:1
  GlobalTopologyData::TopologyVector topology_vector = {{1, 0}, {2, 0}, {1, 1}};
  GlobalTopologyData::ParentIndexVector parent_indices = {0, 0, 0};

  auto uber_struct1 = std::make_unique<UberStruct>();
  auto uber_struct2 = std::make_unique<UberStruct>();

  // Each matrix scales by a different prime number to distinguish the branches.
  uber_struct1->local_matrices[{1, 0}] = glm::scale(glm::mat3(), {2.f, 2.f});
  uber_struct1->local_matrices[{1, 1}] = glm::scale(glm::mat3(), {3.f, 3.f});

  uber_struct2->local_matrices[{2, 0}] = glm::scale(glm::mat3(), {5.f, 5.f});

  uber_structs[1] = std::move(uber_struct1);
  uber_structs[2] = std::move(uber_struct2);

  std::vector<glm::mat3> expected_matrices = {
      glm::scale(glm::mat3(), glm::vec2(2.f)),   // 1:0 = 2
      glm::scale(glm::mat3(), glm::vec2(10.f)),  // 1:0 * 2:0 = 2 * 5 = 10
      glm::scale(glm::mat3(), glm::vec2(6.f)),   // 1:0 * 1:1 = 2 * 3 = 6
  };

  auto global_matrices = ComputeGlobalMatrices(topology_vector, parent_indices, uber_structs);
  EXPECT_THAT(global_matrices, ::testing::ElementsAreArray(expected_matrices));
}

// Ensure that when a transform node has two parents, that its data is duplicated in
// the global topology vector, with the proper global data (i.e. matrices, clip regions
// and hit regions) for each entry, respecting each separate chain up the hierarchy.
// This is used for A11Y Magnification.
TEST(GlobalMatrixDataTest, MultipleParentTest) {
  // Make a global topology representing the following graph.
  // We have a diamond pattern hierarchy where transform 1:4
  // is children to both 1:1 and 1:3.
  //
  // 1:0 - 1:1
  //     \    \
  //       1:3 - 1:4
  UberStruct::InstanceMap uber_structs;
  auto uber_struct = std::make_unique<UberStruct>();

  // Set up the uber struct with the above topology. Set the doubly-parented child (1,4) up
  // with a hit region and clip region to make sure those get duplicated properly.
  constexpr TransformClipRegion kClipRegion({.x = 5, .y = 10, .width = 30, .height = 40});
  const flatland::HitRegion kHitRegion({.x = 1, .y = 2, .width = 10, .height = 20});
  const float kScale = 2.0f;

  uber_struct->local_topology = {{{1, 0}, 2}, {{1, 1}, 1}, {{1, 4}, 0}, {{1, 3}, 1}, {{1, 4}, 0}};
  uber_struct->local_matrices[{1, 3}] = glm::mat3(kScale);
  uber_struct->local_hit_regions_map[{1, 4}] = {kHitRegion};
  uber_struct->local_clip_regions[{1, 4}] = kClipRegion;
  uber_structs[1] = std::move(uber_struct);

  auto global_topology_data =
      GlobalTopologyData::ComputeGlobalTopologyData(uber_structs, {}, {}, {1, 0});
  GlobalTopologyData::TopologyVector topology_vector = global_topology_data.topology_vector;
  auto parent_indices = global_topology_data.parent_indices;

  GlobalTopologyData::TopologyVector expected_topology_vector = {
      {1, 0}, {1, 1}, {1, 4}, {1, 3}, {1, 4}};
  GlobalTopologyData::ParentIndexVector expected_parent_indices = {0, 0, 1, 0, 3};

  for (uint32_t i = 0; i < topology_vector.size(); i++) {
    EXPECT_EQ(topology_vector[i], expected_topology_vector[i]);
  }

  for (uint32_t i = 0; i < parent_indices.size(); i++) {
    EXPECT_EQ(parent_indices[i], expected_parent_indices[i]);
  }

  // Each entry for the doubly parented node should have a different global matrix.
  const auto matrix_vector = ComputeGlobalMatrices(topology_vector, parent_indices, uber_structs);
  EXPECT_EQ(matrix_vector.size(), 5U);
  EXPECT_EQ(matrix_vector[2], glm::mat3(1.0));
  EXPECT_EQ(matrix_vector[4], glm::mat3(2.0));

  // Each entry for the doubly parented node should have different clip regions.
  {
    const auto clip_vector = ComputeGlobalTransformClipRegions(topology_vector, parent_indices,
                                                               matrix_vector, uber_structs);
    EXPECT_EQ(clip_vector.size(), 5U);

    // The first clip region should match exactly the clip region above.
    EXPECT_EQ(clip_vector[2], kClipRegion);

    // The second one should be magnified by the scale factor.
    EXPECT_EQ(clip_vector[4],
              TransformClipRegion({.x = static_cast<int32_t>(kScale * kClipRegion.x()),
                                   .y = static_cast<int32_t>(kScale * kClipRegion.y()),
                                   .width = static_cast<int32_t>(kScale * kClipRegion.width()),
                                   .height = static_cast<int32_t>(kScale * kClipRegion.height())}));
  }

  // Each entry for the doubly parented node should have different hit regions.
  {
    const auto hit_map =
        ComputeGlobalHitRegions(topology_vector, parent_indices, matrix_vector, uber_structs);
    auto itr = hit_map.find({1, 4});
    EXPECT_NE(itr, hit_map.end());

    auto vec = itr->second;
    EXPECT_EQ(vec.size(), 2U);

    const auto first = vec[0];
    const auto second = vec[1];

    // The first clip region should match exactly the hit region above.
    EXPECT_EQ(first.region(), kHitRegion.region());

    // The second one should be magnified by the scale factor.
    EXPECT_EQ(second.region(), kHitRegion.region().ScaledBy(kScale));
  }
}

// The following tests test for transform clip regions

// Test that an empty uber struct returns empty clip regions.
TEST(GlobalTransformClipTest, EmptyTopologyReturnsEmptyClipRegions) {
  UberStruct::InstanceMap uber_structs;
  GlobalTopologyData::TopologyVector topology_vector;
  GlobalTopologyData::ParentIndexVector parent_indices;
  GlobalMatrixVector global_matrices;

  auto global_clip_regions = ComputeGlobalTransformClipRegions(topology_vector, parent_indices,
                                                               global_matrices, uber_structs);
  EXPECT_TRUE(global_clip_regions.empty());
}

// Check that if there are no clip regions provided, they default to
// non-clipped regions.
TEST(GlobalTransformClipTest, EmptyClipRegionsAreInvalid) {
  UberStruct::InstanceMap uber_structs;

  // Make a global topology representing the following graph:
  //
  // 1:0 - 1:1
  GlobalTopologyData::TopologyVector topology_vector = {{1, 0}, {1, 1}};
  GlobalTopologyData::ParentIndexVector parent_indices = {0, 0};
  GlobalMatrixVector global_matrices = {glm::mat3(1.0), glm::mat3(1.0)};

  // The UberStruct for instance ID 1 must exist, but it contains no local opacity values.
  auto uber_struct = std::make_unique<UberStruct>();
  uber_structs[1] = std::move(uber_struct);

  GlobalTransformClipRegionVector expected_clip_regions = {kUnclippedRegion, kUnclippedRegion};

  auto global_clip_regions = ComputeGlobalTransformClipRegions(topology_vector, parent_indices,
                                                               global_matrices, uber_structs);
  EXPECT_EQ(expected_clip_regions.size(), global_clip_regions.size());
  for (uint32_t i = 0; i < global_clip_regions.size(); i++) {
    EXPECT_EQ(expected_clip_regions[i], global_clip_regions[i]);
  }
}

// The parent and child regions do not overlap, so the child region should
// be completely empty.
TEST(GlobalTransformClipTest, NoOverlapClipRegions) {
  UberStruct::InstanceMap uber_structs;

  // Make a global topology representing the following graph:
  //
  // 1:0 - 1:1
  GlobalTopologyData::TopologyVector topology_vector = {{1, 0}, {1, 1}};
  GlobalTopologyData::ParentIndexVector parent_indices = {0, 0};
  GlobalMatrixVector global_matrices = {glm::mat3(1.0), glm::mat3(1.0)};

  auto uber_struct = std::make_unique<UberStruct>();

  // The two regions do not overlap.
  GlobalTransformClipRegionVector clip_regions = {
      TransformClipRegion({.x = 0, .y = 0, .width = 100, .height = 200}),
      TransformClipRegion({.x = 200, .y = 300, .width = 100, .height = 200})};

  uber_struct->local_clip_regions[{1, 0}] = clip_regions[0];
  uber_struct->local_clip_regions[{1, 1}] = clip_regions[1];

  uber_structs[1] = std::move(uber_struct);

  GlobalTransformClipRegionVector expected_clip_regions = {
      clip_regions[0], TransformClipRegion({.x = 0, .y = 0, .width = 0, .height = 0})};

  auto global_clip_regions = ComputeGlobalTransformClipRegions(topology_vector, parent_indices,
                                                               global_matrices, uber_structs);
  EXPECT_EQ(global_clip_regions.size(), expected_clip_regions.size());
  for (uint64_t i = 0; i < global_clip_regions.size(); i++) {
    EXPECT_EQ(global_clip_regions[i], expected_clip_regions[i]);
  }

  // Now translate the child transform, to (-200, -300). Since the clip region's region is specified
  // to be (200,300) in the local coordinate space of the child transform, its global space should
  // therefore be (0,0) and it should line up with the clip region of the parent.
  global_matrices[1] = glm::translate(glm::mat3(1.0), glm::vec2(-200, -300));
  global_clip_regions = ComputeGlobalTransformClipRegions(topology_vector, parent_indices,
                                                          global_matrices, uber_structs);

  // Both clip regions should be the same.
  expected_clip_regions[1] = clip_regions[0];
  EXPECT_EQ(global_clip_regions.size(), expected_clip_regions.size());
  for (uint64_t i = 0; i < global_clip_regions.size(); i++) {
    EXPECT_EQ(global_clip_regions[i], expected_clip_regions[i]);
  }
}

// The following tests ensure scale and rotate modify the clip region as expected.

TEST(GlobalTransformClipTest, ScaleAndRotate90DegreesTest) {
  UberStruct::InstanceMap uber_structs;

  // Make a global topology representing a single node.
  GlobalTopologyData::TopologyVector topology_vector = {{1, 0}};
  GlobalTopologyData::ParentIndexVector parent_indices = {0};

  const glm::vec2 scale(3.f, 2.f);
  glm::mat3 matrix = glm::rotate(
      glm::mat3(), GetOrientationAngleInViewSpaceCoordinates(Orientation::kCcw90Degrees));
  matrix = glm::scale(matrix, scale);
  GlobalMatrixVector global_matrices = {matrix};

  auto uber_struct = std::make_unique<UberStruct>();

  uber_struct->local_clip_regions[{1, 0}] =
      TransformClipRegion({.x = 0, .y = 0, .width = 100, .height = 50});

  uber_structs[1] = std::move(uber_struct);

  GlobalTransformClipRegionVector expected_clip_regions = {
      TransformClipRegion({.x = 0, .y = -300, .width = 100, .height = 300})};

  auto global_clip_regions = ComputeGlobalTransformClipRegions(topology_vector, parent_indices,
                                                               global_matrices, uber_structs);
  EXPECT_EQ(global_clip_regions.size(), expected_clip_regions.size());
  EXPECT_EQ(global_clip_regions[0], expected_clip_regions[0]);
}

TEST(GlobalTransformClipTest, ScaleAndRotate180DegreesTest) {
  UberStruct::InstanceMap uber_structs;

  // Make a global topology representing a single node.
  GlobalTopologyData::TopologyVector topology_vector = {{1, 0}};
  GlobalTopologyData::ParentIndexVector parent_indices = {0};

  const glm::vec2 scale(3.f, 2.f);
  glm::mat3 matrix = glm::rotate(
      glm::mat3(), GetOrientationAngleInViewSpaceCoordinates(Orientation::kCcw180Degrees));
  matrix = glm::scale(matrix, scale);
  GlobalMatrixVector global_matrices = {matrix};

  auto uber_struct = std::make_unique<UberStruct>();

  uber_struct->local_clip_regions[{1, 0}] =
      TransformClipRegion({.x = 0, .y = 0, .width = 100, .height = 50});

  uber_structs[1] = std::move(uber_struct);

  GlobalTransformClipRegionVector expected_clip_regions = {
      TransformClipRegion({.x = -300, .y = -100, .width = 300, .height = 100})};

  auto global_clip_regions = ComputeGlobalTransformClipRegions(topology_vector, parent_indices,
                                                               global_matrices, uber_structs);
  EXPECT_EQ(global_clip_regions.size(), expected_clip_regions.size());
  EXPECT_EQ(global_clip_regions[0], expected_clip_regions[0]);
}

TEST(GlobalTransformClipTest, ScaleAndRotate270DegreesTest) {
  UberStruct::InstanceMap uber_structs;

  // Make a global topology representing a single node.
  GlobalTopologyData::TopologyVector topology_vector = {{1, 0}};
  GlobalTopologyData::ParentIndexVector parent_indices = {0};

  const glm::vec2 scale(3.f, 2.f);
  glm::mat3 matrix = glm::rotate(
      glm::mat3(), GetOrientationAngleInViewSpaceCoordinates(Orientation::kCcw270Degrees));
  matrix = glm::scale(matrix, scale);
  GlobalMatrixVector global_matrices = {matrix};

  auto uber_struct = std::make_unique<UberStruct>();

  uber_struct->local_clip_regions[{1, 0}] =
      TransformClipRegion({.x = 0, .y = 0, .width = 100, .height = 50});

  uber_structs[1] = std::move(uber_struct);

  GlobalTransformClipRegionVector expected_clip_regions = {
      TransformClipRegion({.x = -100, .y = 0, .width = 100, .height = 300})};

  auto global_clip_regions = ComputeGlobalTransformClipRegions(topology_vector, parent_indices,
                                                               global_matrices, uber_structs);
  EXPECT_EQ(global_clip_regions.size(), expected_clip_regions.size());
  EXPECT_EQ(global_clip_regions[0], expected_clip_regions[0]);
}

// Test a more complicated scenario with multiple transforms, each with its own
// clip region and transform matrix set.
TEST(GlobalTransformClipTest, ComplicatedGraphClipRegions) {
  UberStruct::InstanceMap uber_structs;

  // Make a global topology representing the following graph:
  //
  // 1:0 - 1:1 - 1:2
  //     \
  //       1:3 - 1:4
  GlobalTopologyData::TopologyVector topology_vector = {{1, 0}, {1, 1}, {1, 2}, {1, 3}, {1, 4}};
  GlobalTopologyData::ParentIndexVector parent_indices = {0, 0, 1, 0, 3};
  GlobalMatrixVector global_matrices = {glm::translate(glm::mat3(1.0), glm::vec2(5, 10)),
                                        glm::translate(glm::mat3(1.0), glm::vec2(-5, -10)),
                                        glm::translate(glm::mat3(1.0), glm::vec2(20, 30)),
                                        glm::translate(glm::mat3(1.0), glm::vec2(-5, -10)),
                                        glm::translate(glm::mat3(1.0), glm::vec2(-10, -20))};

  auto uber_struct = std::make_unique<UberStruct>();

  GlobalTransformClipRegionVector clip_regions = {
      TransformClipRegion({0, 0, 100, 200}),   TransformClipRegion({-1000, -1000, 2000, 2000}),
      TransformClipRegion({0, 0, 110, 300}),   TransformClipRegion({-5, -10, 300, 400}),
      TransformClipRegion({-15, -30, 20, 30}),
  };

  uber_struct->local_clip_regions[{1, 0}] = clip_regions[0];

  uber_struct->local_clip_regions[{1, 1}] = clip_regions[1];
  uber_struct->local_clip_regions[{1, 2}] = clip_regions[2];

  uber_struct->local_clip_regions[{1, 3}] = clip_regions[3];
  uber_struct->local_clip_regions[{1, 4}] = clip_regions[4];

  uber_structs[1] = std::move(uber_struct);

  GlobalTransformClipRegionVector expected_clip_regions = {
      TransformClipRegion({.x = 5, .y = 10, .width = 100, .height = 200}),
      TransformClipRegion({.x = 5, .y = 10, .width = 100, .height = 200}),
      TransformClipRegion({.x = 20, .y = 30, .width = 85, .height = 180}),
      TransformClipRegion({.x = 5, .y = 10, .width = 100, .height = 200}),
      TransformClipRegion({.x = 0, .y = 0, .width = 0, .height = 0}),
  };

  auto global_clip_regions = ComputeGlobalTransformClipRegions(topology_vector, parent_indices,
                                                               global_matrices, uber_structs);
  EXPECT_EQ(global_clip_regions.size(), expected_clip_regions.size());
  for (uint64_t i = 0; i < global_clip_regions.size(); i++) {
    EXPECT_EQ(global_clip_regions[i], expected_clip_regions[i]);
  }
}

// We recreate several of the matrix tests above with opacity values here,
// since the logic for calculating opacities is largely the same as calculating
// matrices, where child values are the product of their local values and their
// ancestors' values.
//
// TODO(https://fxbug.dev/42153097): Since the logic between matrices and opacity is very similar,
// in the future we may want to consolidate |ComputeGlobalMatrices| and |ComputeGlobalOpacityValues|
// into a single (potentially templated) function, which would allow us to consolidate these tests
// into one. But for now, we have to keep them separate.

TEST(GlobalImageDataTest, EmptyTopologyReturnsEmptyOpacityValues) {
  UberStruct::InstanceMap uber_structs;
  GlobalTopologyData::TopologyVector topology_vector;
  GlobalTopologyData::ParentIndexVector parent_indices;

  auto global_opacity_values =
      ComputeGlobalOpacityValues(topology_vector, parent_indices, uber_structs);
  EXPECT_TRUE(global_opacity_values.empty());
}

// Check that if there are no opacity values provided, they default to 1.0 for
// parent and child.
TEST(GlobalImageDataTest, EmptyLocalOpacitiesAreOpaque) {
  UberStruct::InstanceMap uber_structs;

  // Make a global topology representing the following graph:
  //
  // 1:0 - 1:1
  GlobalTopologyData::TopologyVector topology_vector = {{1, 0}, {1, 1}};
  GlobalTopologyData::ParentIndexVector parent_indices = {0, 0};

  // The UberStruct for instance ID 1 must exist, but it contains no local opacity values.
  auto uber_struct = std::make_unique<UberStruct>();
  uber_structs[1] = std::move(uber_struct);

  // The root opacity value is set to 1.0, and the second inherits that.
  std::vector<float> expected_opacities = {
      1.f,
      1.f,
  };

  auto global_opacities = ComputeGlobalOpacityValues(topology_vector, parent_indices, uber_structs);
  EXPECT_THAT(global_opacities, ::testing::ElementsAreArray(expected_opacities));
}

// Test a more complicated scenario with multiple parent-child relationships and make
// sure all of the opacity values are being inherited properly.
TEST(GlobalImageDataTest, GlobalImagesIncludeParentImage) {
  UberStruct::InstanceMap uber_structs;

  // Make a global topology representing the following graph:
  //
  // 1:0 - 1:1 - 1:2
  //     \
  //       1:3 - 1:4
  GlobalTopologyData::TopologyVector topology_vector = {{1, 0}, {1, 1}, {1, 2}, {1, 3}, {1, 4}};
  GlobalTopologyData::ParentIndexVector parent_indices = {0, 0, 1, 0, 3};

  auto uber_struct = std::make_unique<UberStruct>();

  const float opacities[] = {0.9f, 0.8f, 0.7f, 0.6f, 0.5f};

  uber_struct->local_opacity_values[{1, 0}] = opacities[0];

  uber_struct->local_opacity_values[{1, 1}] = opacities[1];
  uber_struct->local_opacity_values[{1, 2}] = opacities[2];

  uber_struct->local_opacity_values[{1, 3}] = opacities[3];
  uber_struct->local_opacity_values[{1, 4}] = opacities[4];

  uber_structs[1] = std::move(uber_struct);

  std::vector<float> expected_opacities = {
      opacities[0],
      opacities[0] * opacities[1],
      opacities[0] * opacities[1] * opacities[2],
      opacities[0] * opacities[3],
      opacities[0] * opacities[3] * opacities[4],
  };

  auto global_opacities = ComputeGlobalOpacityValues(topology_vector, parent_indices, uber_structs);
  EXPECT_THAT(global_opacities, ::testing::ElementsAreArray(expected_opacities));
}

TEST(GlobalImageDataTest, GlobalImagesMultipleUberStructs) {
  UberStruct::InstanceMap uber_structs;

  // Make a global topology representing the following graph:
  //
  // 1:0 - 2:0
  //     \
  //       1:1
  GlobalTopologyData::TopologyVector topology_vector = {{1, 0}, {2, 0}, {1, 1}};
  GlobalTopologyData::ParentIndexVector parent_indices = {0, 0, 0};

  auto uber_struct1 = std::make_unique<UberStruct>();
  auto uber_struct2 = std::make_unique<UberStruct>();

  const float opacity_values[] = {0.5f, 0.3f, 0.9f};

  uber_struct1->local_opacity_values[{1, 0}] = opacity_values[0];
  uber_struct2->local_opacity_values[{2, 0}] = opacity_values[1];
  uber_struct1->local_opacity_values[{1, 1}] = opacity_values[2];

  uber_structs[1] = std::move(uber_struct1);
  uber_structs[2] = std::move(uber_struct2);

  std::vector<float> expected_opacity_values = {opacity_values[0],
                                                opacity_values[0] * opacity_values[1],
                                                opacity_values[0] * opacity_values[2]};

  auto global_opacity_values =
      ComputeGlobalOpacityValues(topology_vector, parent_indices, uber_structs);
  EXPECT_THAT(global_opacity_values, ::testing::ElementsAreArray(expected_opacity_values));
}

TEST(GlobalImageDataTest, OutputVectorVariantPopulatesCorrectly) {
  UberStruct::InstanceMap uber_structs;
  GlobalTopologyData::TopologyVector topology_vector = {{1, 0}, {2, 0}, {1, 1}};
  GlobalTopologyData::ParentIndexVector parent_indices = {0, 0, 0};

  auto uber_struct1 = std::make_unique<UberStruct>();
  auto uber_struct2 = std::make_unique<UberStruct>();

  const float opacity_values[] = {0.5f, 0.3f, 0.9f};

  uber_struct1->local_opacity_values[{1, 0}] = opacity_values[0];
  uber_struct2->local_opacity_values[{2, 0}] = opacity_values[1];
  uber_struct1->local_opacity_values[{1, 1}] = opacity_values[2];

  uber_structs[1] = std::move(uber_struct1);
  uber_structs[2] = std::move(uber_struct2);

  std::vector<float> expected_opacity_values = {opacity_values[0],
                                                opacity_values[0] * opacity_values[1],
                                                opacity_values[0] * opacity_values[2]};

  GlobalOpacityVector output;
  output.push_back(-1.0);  // verify that vector is cleared by ComputeGlobalOpacityValues().
  ComputeGlobalOpacityValues(output, topology_vector, parent_indices, uber_structs);
  EXPECT_THAT(output, ::testing::ElementsAreArray(expected_opacity_values));
}

}  // namespace test
}  // namespace flatland
