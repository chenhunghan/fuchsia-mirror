// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/global_matrix_data.h"

#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>

#include <algorithm>
#include <limits>

#include "src/ui/scenic/lib/flatland/flatland_types.h"

namespace flatland {

constexpr TransformClipRegion kUnclippedRegion({.x = -(std::numeric_limits<int32_t>::max() / 2),
                                                .y = -(std::numeric_limits<int32_t>::max() / 2),
                                                .width = std::numeric_limits<int32_t>::max(),
                                                .height = std::numeric_limits<int32_t>::max()});

namespace {

// TODO(https://fxbug.dev/426028969): `types::RectangleF` exists now; consider using it here after
// adding helpers such as `Overlap()` or `Intersect(...).IsEmpty()`.  One concern is that we heavily
// use `glm::vec2` including matrix multiplication, so we want to verify that CPU performance isn't
// affected by switching formats.
bool Overlap(const TransformClipRegion& clip, const glm::vec2& origin, const glm::vec2& extent) {
  if (clip == kUnclippedRegion)
    return true;
  const types::Point2 opposite = clip.opposite();
  if (origin.x > static_cast<float>(opposite.x()))
    return false;
  if (origin.y > static_cast<float>(opposite.y()))
    return false;
  if (origin.x + extent.x < static_cast<float>(clip.x()))
    return false;
  if (origin.y + extent.y < static_cast<float>(clip.y()))
    return false;
  return true;
}

// TODO(https://fxbug.dev/426028969): add `types::RectangleF` and use it here.  An `Intersect`
// would be handy, too.
std::pair<glm::vec2, glm::vec2> ClipRectangle(const TransformClipRegion& clip,
                                              const glm::vec2& origin, const glm::vec2& extent) {
  if (!Overlap(clip, origin, extent)) {
    return {glm::vec2(0), glm::vec2(0)};
  }

  glm::vec2 result_origin, result_extent;
  result_origin.x = std::max(float(clip.x()), origin.x);
  result_extent.x = std::min(float(clip.x() + clip.width()), origin.x + extent.x) - result_origin.x;

  result_origin.y = std::max(float(clip.y()), origin.y);
  result_extent.y =
      std::min(float(clip.y() + clip.height()), origin.y + extent.y) - result_origin.y;

  return {result_origin, result_extent};
}

template <typename RectType>
RectType MatrixMultiplyRectHelper(const glm::mat3& matrix, const RectType& rect) {
  const float rx = static_cast<float>(rect.x());
  const float ry = static_cast<float>(rect.y());
  const float rw = static_cast<float>(rect.width());
  const float rh = static_cast<float>(rect.height());

  const glm::vec2 p0 = matrix * glm::vec3(rx, ry, 1.f);
  const glm::vec2 p1 = matrix * glm::vec3(rx + rw, ry, 1.f);
  const glm::vec2 p2 = matrix * glm::vec3(rx + rw, ry + rh, 1.f);
  const glm::vec2 p3 = matrix * glm::vec3(rx, ry + rh, 1.f);

  const float min_x = std::min({p0.x, p1.x, p2.x, p3.x});
  const float min_y = std::min({p0.y, p1.y, p2.y, p3.y});
  const float max_x = std::max({p0.x, p1.x, p2.x, p3.x});
  const float max_y = std::max({p0.y, p1.y, p2.y, p3.y});

  using CoordType = decltype(rect.x());
  return RectType({
      .x = static_cast<CoordType>(min_x),
      .y = static_cast<CoordType>(min_y),
      .width = static_cast<CoordType>(max_x - min_x),
      .height = static_cast<CoordType>(max_y - min_y),
  });
}

types::Rectangle MatrixMultiplyRect(const glm::mat3& matrix, const types::Rectangle& rect) {
  return MatrixMultiplyRectHelper(matrix, rect);
}

types::RectangleF MatrixMultiplyRectF(const glm::mat3& matrix, const types::RectangleF& rect) {
  return MatrixMultiplyRectHelper(matrix, rect);
}

}  // namespace

GlobalMatrixVector ComputeGlobalMatrices(
    const GlobalTopologyData::TopologyVector& global_topology,
    const GlobalTopologyData::ParentIndexVector& parent_indices,
    const UberStruct::InstanceMap& uber_structs) {
  GlobalMatrixVector output;
  ComputeGlobalMatrices(output, global_topology, parent_indices, uber_structs);
  return output;
}

void ComputeGlobalMatrices(GlobalMatrixVector& output,
                           const GlobalTopologyData::TopologyVector& global_topology,
                           const GlobalTopologyData::ParentIndexVector& parent_indices,
                           const UberStruct::InstanceMap& uber_structs) {
  TRACE_DURATION("gfx", "ComputeGlobalMatrices");

  output.clear();
  if (global_topology.empty()) {
    return;
  }

  output.reserve(global_topology.size());

  // The root entry's parent pointer points to itself, so special case it.
  const auto& root_handle = global_topology.front();
  const auto root_uber_struct_kv = uber_structs.find(root_handle.GetInstanceId());
  FX_DCHECK(root_uber_struct_kv != uber_structs.end());

  const auto root_matrix_kv = root_uber_struct_kv->second->local_matrices.find(root_handle);

  if (root_matrix_kv == root_uber_struct_kv->second->local_matrices.end()) {
    output.emplace_back(glm::mat3());
  } else {
    const auto& matrix = root_matrix_kv->second;
    output.emplace_back(matrix);
  }

  for (size_t i = 1; i < global_topology.size(); ++i) {
    const TransformHandle& handle = global_topology[i];
    const size_t parent_index = parent_indices[i];

    // Every entry in the global topology comes from an UberStruct.
    const auto uber_struct_kv = uber_structs.find(handle.GetInstanceId());
    FX_DCHECK(uber_struct_kv != uber_structs.end());

    const auto matrix_kv = uber_struct_kv->second->local_matrices.find(handle);

    if (matrix_kv == uber_struct_kv->second->local_matrices.end()) {
      // This is *definitely* safe because we reserve storage above, so there is no chance of
      // reallocation.  However, a close reading of the C++ spec requires this to be safe even
      // with reallocation.
      output.emplace_back(output[parent_index]);
    } else {
      // See comment above.  This is safe even without a close reading of the C++ spec, because the
      // argument is computed before `emplace_back()` is called.
      output.emplace_back(output[parent_index] * matrix_kv->second);
    }
  }
}

GlobalTransformClipRegionVector ComputeGlobalTransformClipRegions(
    const GlobalTopologyData::TopologyVector& global_topology,
    const GlobalTopologyData::ParentIndexVector& parent_indices,
    const GlobalMatrixVector& matrix_vector, const UberStruct::InstanceMap& uber_structs) {
  GlobalTransformClipRegionVector output;
  ComputeGlobalTransformClipRegions(output, global_topology, parent_indices, matrix_vector,
                                    uber_structs);
  return output;
}

void ComputeGlobalTransformClipRegions(GlobalTransformClipRegionVector& output,
                                       const GlobalTopologyData::TopologyVector& global_topology,
                                       const GlobalTopologyData::ParentIndexVector& parent_indices,
                                       const GlobalMatrixVector& matrix_vector,
                                       const UberStruct::InstanceMap& uber_structs) {
  TRACE_DURATION("gfx", "ComputeGlobalTransformClipRegions");
  FX_DCHECK(global_topology.size() == parent_indices.size());
  FX_DCHECK(global_topology.size() == matrix_vector.size());

  output.clear();
  if (global_topology.empty()) {
    return;
  }

  output.reserve(global_topology.size());

  // The root entry's parent pointer points to itself, so special case it.
  const auto& root_handle = global_topology.front();
  const auto root_uber_struct_kv = uber_structs.find(root_handle.GetInstanceId());
  FX_DCHECK(root_uber_struct_kv != uber_structs.end());

  const auto root_regions_kv = root_uber_struct_kv->second->local_clip_regions.find(root_handle);

  // Process the root separately from the rest of the tree.
  if (root_regions_kv == root_uber_struct_kv->second->local_clip_regions.end()) {
    output.emplace_back(kUnclippedRegion);
  } else {
    output.emplace_back(MatrixMultiplyRect(matrix_vector[0], root_regions_kv->second));
  }

  for (size_t i = 1; i < global_topology.size(); ++i) {
    const TransformHandle& handle = global_topology[i];
    const size_t parent_index = parent_indices[i];
    auto parent_clip = output[parent_index];

    // Every entry in the global topology comes from an UberStruct.
    const auto uber_stuct_kv = uber_structs.find(handle.GetInstanceId());
    FX_DCHECK(uber_stuct_kv != uber_structs.end());
    const auto regions_kv = uber_stuct_kv->second->local_clip_regions.find(handle);

    // A clip region is bounded to that of its parent region. If the current clip region
    // is empty, then it defaults to that of its parent. Otherwise, we must find the
    // intersection of the parent clip region and the current clip region, in the global
    // coordinate space.
    if (regions_kv == uber_stuct_kv->second->local_clip_regions.end()) {
      output.emplace_back(parent_clip);
    } else {
      // Calculate the global position of the current clip region.
      auto curr_clip = MatrixMultiplyRect(matrix_vector[i], regions_kv->second);

      // Calculate the intersection of the current clip with its parent.
      glm::vec2 curr_origin = {curr_clip.x(), curr_clip.y()};
      glm::vec2 curr_extent = {curr_clip.width(), curr_clip.height()};
      auto [clipped_origin, clipped_extent] = ClipRectangle(parent_clip, curr_origin, curr_extent);

      // Add the intersection to the global clip vector.
      output.emplace_back(TransformClipRegion({.x = static_cast<int>(clipped_origin.x),
                                               .y = static_cast<int>(clipped_origin.y),
                                               .width = static_cast<int>(clipped_extent.x),
                                               .height = static_cast<int>(clipped_extent.y)}));
    }
  }
}

GlobalHitRegionsMap ComputeGlobalHitRegions(
    const GlobalTopologyData::TopologyVector& global_topology,
    const GlobalTopologyData::ParentIndexVector& parent_indices,
    const GlobalMatrixVector& matrix_vector, const UberStruct::InstanceMap& uber_structs) {
  TRACE_DURATION("gfx", "ComputeGlobalHitRegions");
  FX_DCHECK(global_topology.size() == parent_indices.size());
  FX_DCHECK(global_topology.size() == matrix_vector.size());

  GlobalHitRegionsMap global_hit_regions;

  for (size_t i = 0; i < global_topology.size(); ++i) {
    const TransformHandle& handle = global_topology[i];

    // Every entry in the global topology comes from an UberStruct.
    const auto uber_struct_kv = uber_structs.find(handle.GetInstanceId());
    FX_DCHECK(uber_struct_kv != uber_structs.end());

    const auto& local_hit_regions_map = uber_struct_kv->second->local_hit_regions_map;
    const auto regions_vec_kv = local_hit_regions_map.find(handle);

    if (regions_vec_kv != local_hit_regions_map.end()) {
      auto& hit_regions = global_hit_regions[handle];
      hit_regions.reserve(regions_vec_kv->second.size());
      for (const auto& local_hit_region : regions_vec_kv->second) {
        if (local_hit_region.is_finite()) {
          // Usually: calculate the global position of the current hit region.
          auto global_rect = MatrixMultiplyRectF(matrix_vector[i], local_hit_region.region());
          hit_regions.emplace_back(global_rect, local_hit_region.interaction());
        } else {
          // Special case: preserve sentinel value for infinite hit region.
          hit_regions.push_back(local_hit_region);
        }
      }
    }
  }

  return global_hit_regions;
}

}  // namespace flatland
