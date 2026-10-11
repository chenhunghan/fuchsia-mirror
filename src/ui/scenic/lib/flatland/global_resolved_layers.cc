// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/global_resolved_layers.h"

#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>

#include <algorithm>
#include <array>
#include <cmath>
#include <limits>
#include <optional>

#include "src/ui/scenic/lib/flatland/global_matrix_data.h"

namespace flatland {
namespace {

// Decodes the rotation of a transform node's `global_matrix` based on where it sends the +x
// axis.  Node matrices hold only quarter turns and positive scales (see `ResolvedLayerStack`),
// so +x lands on one axis.  Runs once per stack-hosting node, not per layer.
types::RotateFlip DecodeNodeRotation(const glm::mat3& matrix) {
  const float x = matrix[0][0];
  const float y = matrix[0][1];

  // Picks the axis by comparing products, not by taking an angle with `atan2()`.  `Flatland`
  // builds a quarter turn from `sin()` and `cos()` of a float angle that is not exactly a quarter
  // turn, leaving a tiny nonzero value where the exact matrix has zero; unequal x and y scales can
  // magnify it until an angle rounds to the wrong turn.  Both products carry the same scale
  // factors, so comparing them works at any scale; double avoids float overflow and underflow.
  if (std::abs(static_cast<double>(x) * matrix[1][1]) >=
      std::abs(static_cast<double>(y) * matrix[1][0])) {
    return x > 0.f ? types::RotateFlip::kIdentity() : types::RotateFlip::kRotateCcw180();
  }
  // View space has +y pointing down, so a counter-clockwise quarter turn takes +x to -y.
  return y < 0.f ? types::RotateFlip::kRotateCcw90() : types::RotateFlip::kRotateCcw270();
}

// Projects `display_rect` into screen space using `node_global_matrix`, clips against
// `node_clip_region`, and proportionally shrinks `unclipped_src` using `leaf_transform`.
//
// Per `LayerProperties.display_rect` (flatland2.fidl) and `UberStructLayer::CommonProperties`:
// `display_rect` is already the post-rotation destination rectangle in the hosting node's local
// coordinate space. `leaf_transform` (`image.transform.RotatedBy(entry.node_rotation)`) determines
// how the sampled region maps onto `display_rect` (and therefore which UV edges shrink when clipped
// in screen space), but does not rotate or swap the dimensions of `display_rect` itself.
std::optional<SrcToDest> ComputeClippedLayerGeometry(const glm::mat3& node_global_matrix,
                                                     const TransformClipRegion& node_clip_region,
                                                     const types::Rectangle& display_rect,
                                                     types::RotateFlip leaf_transform,
                                                     const types::RectangleF& unclipped_src) {
  float min_x, min_y, max_x, max_y;
  {
    const float rx = static_cast<float>(display_rect.x());
    const float ry = static_cast<float>(display_rect.y());
    const float rw = static_cast<float>(display_rect.width());
    const float rh = static_cast<float>(display_rect.height());

    const std::array<glm::vec2, 4> verts = {
        node_global_matrix * glm::vec3(rx, ry, 1.f),
        node_global_matrix * glm::vec3(rx + rw, ry, 1.f),
        node_global_matrix * glm::vec3(rx + rw, ry + rh, 1.f),
        node_global_matrix * glm::vec3(rx, ry + rh, 1.f),
    };

    min_x = verts[0].x;
    min_y = verts[0].y;
    max_x = verts[0].x;
    max_y = verts[0].y;
    for (size_t i = 1; i < 4; ++i) {
      min_x = std::min(min_x, verts[i].x);
      min_y = std::min(min_y, verts[i].y);
      max_x = std::max(max_x, verts[i].x);
      max_y = std::max(max_y, verts[i].y);
    }
  }

  const glm::vec2 origin(min_x, min_y);
  const glm::vec2 extent(max_x - min_x, max_y - min_y);
  if (extent.x <= 0.f || extent.y <= 0.f) {
    return std::nullopt;
  }

  glm::vec2 clipped_origin = origin;
  glm::vec2 clipped_extent = extent;
  if (node_clip_region != kUnclippedRegion) {
    const float clip_min_x = static_cast<float>(node_clip_region.x());
    const float clip_min_y = static_cast<float>(node_clip_region.y());
    const float clip_max_x = static_cast<float>(node_clip_region.x() + node_clip_region.width());
    const float clip_max_y = static_cast<float>(node_clip_region.y() + node_clip_region.height());

    clipped_origin.x = std::max(clip_min_x, origin.x);
    clipped_origin.y = std::max(clip_min_y, origin.y);
    clipped_extent.x = std::min(clip_max_x, origin.x + extent.x) - clipped_origin.x;
    clipped_extent.y = std::min(clip_max_y, origin.y + extent.y) - clipped_origin.y;

    if (clipped_extent.x <= 0.f || clipped_extent.y <= 0.f) {
      return std::nullopt;
    }
  }

  const types::RectangleF clipped_dest({
      .x = clipped_origin.x,
      .y = clipped_origin.y,
      .width = clipped_extent.x,
      .height = clipped_extent.y,
  });

  // Nothing to shrink when the clip left the destination whole,
  // or when the source is empty (solid-color layer samples nothing).
  // Either way the source passes through unchanged.
  if ((clipped_origin == origin && clipped_extent == extent) ||
      (unclipped_src.width() == 0.f && unclipped_src.height() == 0.f)) {
    return SrcToDest(unclipped_src, clipped_dest, leaf_transform);
  }

  // The destination rectangle was partially clipped, so the source (texel)
  // rectangle shrinks by the same ratios.  The clip ran in dst (screen) space
  // and yielded one ratio per edge; each ratio applies to whichever source edge
  // `leaf_transform` pairs with its dst edge:
  //   - a 0 or 180 degree rotation maps dst-x to source-u and dst-y to source-v;
  //   - a 90 or 270 degree rotation maps dst-x to source-v and dst-y to source-u;
  //   - each value also reverses none, one, or both source axes, and a reversed
  //     axis swaps which end each ratio shrinks:
  //     - `kRotateCcw180` reverses both
  //     - `kRotateCcw90ReflectX` neither
  const float x_lerp = glm::clamp((clipped_origin.x - origin.x) / extent.x, 0.f, 1.f);
  const float y_lerp = glm::clamp((clipped_origin.y - origin.y) / extent.y, 0.f, 1.f);
  const float w_lerp =
      glm::clamp((clipped_origin.x + clipped_extent.x - origin.x) / extent.x, 0.f, 1.f);
  const float h_lerp =
      glm::clamp((clipped_origin.y + clipped_extent.y - origin.y) / extent.y, 0.f, 1.f);

  float u_min_ratio = 0.f;
  float u_max_ratio = 1.f;
  float v_min_ratio = 0.f;
  float v_max_ratio = 1.f;

  switch (leaf_transform.enum_value()) {
    case types::RotateFlip::Enum::kIdentity:
      u_min_ratio = x_lerp;
      u_max_ratio = w_lerp;
      v_min_ratio = y_lerp;
      v_max_ratio = h_lerp;
      break;
    case types::RotateFlip::Enum::kReflectX:
      u_min_ratio = x_lerp;
      u_max_ratio = w_lerp;
      v_min_ratio = 1.f - h_lerp;
      v_max_ratio = 1.f - y_lerp;
      break;
    case types::RotateFlip::Enum::kReflectY:
      u_min_ratio = 1.f - w_lerp;
      u_max_ratio = 1.f - x_lerp;
      v_min_ratio = y_lerp;
      v_max_ratio = h_lerp;
      break;
    case types::RotateFlip::Enum::kRotateCcw180:
      u_min_ratio = 1.f - w_lerp;
      u_max_ratio = 1.f - x_lerp;
      v_min_ratio = 1.f - h_lerp;
      v_max_ratio = 1.f - y_lerp;
      break;
    case types::RotateFlip::Enum::kRotateCcw90:
      u_min_ratio = 1.f - h_lerp;
      u_max_ratio = 1.f - y_lerp;
      v_min_ratio = x_lerp;
      v_max_ratio = w_lerp;
      break;
    case types::RotateFlip::Enum::kRotateCcw90ReflectX:
      u_min_ratio = y_lerp;
      u_max_ratio = h_lerp;
      v_min_ratio = x_lerp;
      v_max_ratio = w_lerp;
      break;
    case types::RotateFlip::Enum::kRotateCcw90ReflectY:
      u_min_ratio = 1.f - h_lerp;
      u_max_ratio = 1.f - y_lerp;
      v_min_ratio = 1.f - w_lerp;
      v_max_ratio = 1.f - x_lerp;
      break;
    case types::RotateFlip::Enum::kRotateCcw270:
      u_min_ratio = y_lerp;
      u_max_ratio = h_lerp;
      v_min_ratio = 1.f - w_lerp;
      v_max_ratio = 1.f - x_lerp;
      break;
  }

  const types::RectangleF clipped_src({
      .x = unclipped_src.x() + u_min_ratio * unclipped_src.width(),
      .y = unclipped_src.y() + v_min_ratio * unclipped_src.height(),
      .width = (u_max_ratio - u_min_ratio) * unclipped_src.width(),
      .height = (v_max_ratio - v_min_ratio) * unclipped_src.height(),
  });

  return SrcToDest(clipped_src, clipped_dest, leaf_transform);
}

template <typename VectorType>
void CullLayersInPlaceImpl(VectorType* layers_in_out, uint64_t display_width,
                           uint64_t display_height) {
  TRACE_DURATION("gfx", "CullLayersInPlace");
  FX_DCHECK(layers_in_out);
  auto is_occluder = [display_width, display_height](const ResolvedLayer& layer) -> bool {
    // Only cull if the rect is opaque.
    auto is_opaque = layer.blend_mode == BlendMode::kReplace();

    // If the rect is full screen (or larger), and opaque, clear the output vectors.
    return (is_opaque && layer.geometry.dest.x() <= 0 && layer.geometry.dest.y() <= 0 &&
            layer.geometry.dest.width() >= static_cast<float>(display_width) &&
            layer.geometry.dest.height() >= static_cast<float>(display_height));
  };

  // Find the index of the last occluder.
  size_t occluder_index = 0;
  for (size_t i = 0; i < layers_in_out->size(); i++) {
    if (is_occluder((*layers_in_out)[i])) {
      occluder_index = i;
    }
  }

  // Move all of the remaining renderable data into the output vectors. Entries get erased
  // if they occur before the last occluder index, or if the geometry at that entry is empty.
  const auto is_geometry_empty = [](const SrcToDest& geometry) {
    return geometry.dest.width() <= 0.f || geometry.dest.height() <= 0.f;
  };

  layers_in_out->erase(std::remove_if(layers_in_out->begin(), layers_in_out->end(),
                                      [index = static_cast<size_t>(0), occluder_index,
                                       &is_geometry_empty](const ResolvedLayer& layer) mutable {
                                        auto curr_index = index++;
                                        return curr_index < occluder_index ||
                                               is_geometry_empty(layer.geometry);
                                      }),
                       layers_in_out->end());
}

template <typename VectorType>
void ComputeGlobalResolvedLayersImpl(VectorType& output,
                                     std::span<const ResolvedLayerStack> layer_stacks,
                                     const UberStruct::InstanceMap& snapshot) {
  TRACE_DURATION("gfx", "ComputeGlobalResolvedLayers");
  output.clear();
  if (layer_stacks.empty()) {
    return;
  }

  for (const ResolvedLayerStack& entry : layer_stacks) {
    if (entry.opacity == 0.f) {
      FX_DCHECK(false) << "ResolvedLayerStack entry for a zero-opacity node: " << entry.handle;
      continue;
    }

    auto uber_struct_kv = snapshot.find(entry.handle.GetInstanceId());
    if (uber_struct_kv == snapshot.end()) {
      FX_DCHECK(false) << "no corresponding UberStruct for ResolvedLayerStack entry: "
                       << entry.handle;
      continue;
    }
    const auto& uber_struct = uber_struct_kv->second;
    FX_CHECK(uber_struct->flatland_version == 1u || uber_struct->flatland_version == 2u)
        << "unknown UberStruct::flatland_version: " << uber_struct->flatland_version;

    auto layer_stack_it = uber_struct->layer_stacks.find(entry.handle);
    // The transform stage emits entries only for nodes that host a stack,
    // so a miss here means the entries are stale relative to `snapshot`.
    if (layer_stack_it == uber_struct->layer_stacks.end()) {
      FX_DCHECK(false) << "no layer stack for ResolvedLayerStack entry: " << entry.handle;
      continue;
    }

    // Helper lambda to append to `output` a `ResolvedLayer` corresponding to an image layer,
    // or to skip it e.g. if completely clipped.
    auto process_image_layer = [&entry, &output, flatland_version = uber_struct->flatland_version](
                                   const UberStructLayer& layer) {
      const auto& image = std::get<UberStructLayer::ImageModeProperties>(layer.content);
      if (image.image_id == allocation::kInvalidImageId) {
        return;
      }

      const types::RotateFlip leaf_transform = image.transform.RotatedBy(entry.node_rotation);
      auto clipped_geometry =
          ComputeClippedLayerGeometry(entry.global_matrix, entry.clip_region,
                                      layer.common.display_rect, leaf_transform, image.sample_rect);
      if (!clipped_geometry) {
        return;
      }

      const auto [blend_mode, multiply_color] =
          ResolveBlendAndOpacity(layer.common.blend_mode, layer.common.opacity * entry.opacity,
                                 /*pin_replace=*/flatland_version == 1);

      output.push_back(ResolvedLayer{
          .geometry = *clipped_geometry,
          .multiply_color = multiply_color,
          .blend_mode = blend_mode,
          .content =
              ResolvedLayer::ImageContent{
                  .image_id = image.image_id,
                  .width = image.image_width,
                  .height = image.image_height,
              },
          .topology_index = entry.topology_index,
      });
    };

    // Helper lambda to append to `output` a `ResolvedLayer` corresponding to a solid color layer,
    // or to skip it e.g. if completely clipped.
    auto process_solid_color_layer = [&entry, &output](const UberStructLayer& layer) {
      // A solid-color layer has no orientation, so its leaf transform is the identity regardless
      // of the hosting node's rotation.
      auto clipped_geometry = ComputeClippedLayerGeometry(
          entry.global_matrix, entry.clip_region, layer.common.display_rect,
          types::RotateFlip::kIdentity(), types::RectangleF());
      if (!clipped_geometry) {
        return;
      }

      // In the UberStructLayer, a solid's `color` is straight (non-premultiplied) RGBA.
      // However, downstream of here in the ResolvedLayer, the blend mode must match the
      // encoded content.  Because blending premultiplied content is slightly more efficient,
      // we adjust STRAIGHT_ALPHA to PREMULTIPLIED_ALPHA here; the corresponding adjustment
      // is made to `content_color` below.  NOTE: this optimization is only applicable to
      // solid color content, because image content pixels cannot be mutated analogously.
      const types::BlendMode normalized_blend =
          layer.common.blend_mode == types::BlendMode::kStraightAlpha()
              ? types::BlendMode::kPremultipliedAlpha()
              : layer.common.blend_mode;

      const auto [blend_mode, multiply_color] = ResolveBlendAndOpacity(
          normalized_blend, layer.common.opacity * entry.opacity, /*pin_replace=*/false);

      // The blend mode computed above will not be STRAIGHT_ALPHA, so we need to compute
      // the premultiplied `content_color` from the straight-alpha color received from the
      // Flatland session.  See DESIGN-solid_fill_encoding (ratified).
      // TODO(https://fxbug.dev/523371761): the ratified DESIGN-blend_mode_and_opacity
      // decision must match the behavior implemented here.
      FX_DCHECK(blend_mode != types::BlendMode::kStraightAlpha());
      const auto& solid = std::get<UberStructLayer::SolidColorModeProperties>(layer.content);
      const float a = solid.color[3];
      const std::array<float, 4> content_color = {solid.color[0] * a, solid.color[1] * a,
                                                  solid.color[2] * a, a};

      output.push_back(ResolvedLayer{
          .geometry = *clipped_geometry,
          .multiply_color = multiply_color,
          .blend_mode = blend_mode,
          .content =
              ResolvedLayer::SolidColorContent{
                  .color = content_color,
              },
          .topology_index = entry.topology_index,
      });
    };

    // For every layer in the stack, process it according to its content type, and (if the layer
    // isn't invisible for some reason) emit a `ResolvedLayer` into `output`.
    for (const auto& layer_handle : layer_stack_it->second) {
      auto layer_it = uber_struct->layers.find(layer_handle);
      FX_CHECK(layer_it != uber_struct->layers.end());
      const auto& layer = layer_it->second;

      if (layer.common.display_rect.width() <= 0 || layer.common.display_rect.height() <= 0 ||
          layer.common.opacity == 0.f) {
        // Invisible.
        continue;
      }

      if (std::holds_alternative<UberStructLayer::ImageModeProperties>(layer.content)) {
        process_image_layer(layer);
      } else if (std::holds_alternative<UberStructLayer::SolidColorModeProperties>(layer.content)) {
        process_solid_color_layer(layer);
      }
      static_assert(3 == std::variant_size_v<decltype(UberStructLayer::content)>,
                    "Must handle all UberStructLayer content types");
    }
  }
}

}  // namespace

void CullLayersInPlace(std::pmr::vector<ResolvedLayer>* layers_in_out, uint64_t display_width,
                       uint64_t display_height) {
  CullLayersInPlaceImpl(layers_in_out, display_width, display_height);
}

void CullLayersInPlace(std::vector<ResolvedLayer>* layers_in_out, uint64_t display_width,
                       uint64_t display_height) {
  CullLayersInPlaceImpl(layers_in_out, display_width, display_height);
}

// Encapsulates the difference between how Flatland1 and Flatland2 APIs treat REPLACE blend mode
// when `opacity < 1`; `pin_replace` is the selector for this differing behavior.
//
// In Flatland1 *for images only*, RGB is scaled and blend stays REPLACE (fade toward "black").
//
// Flatland2, and Flatland1 for solid color fills, "demote" REPLACE to PREMULTIPLIED_ALPHA,
// so there is no visual discontinuity at `opacity == 0` (where the layer is treated as invisible);
// this allows e.g. a window manager to fade out a child app even if it uses REPLACE.
//
// NOTE: `effective_opacity` combines layer opacity with inherited transform opacity.  It does not
// involve "content opacity", neither the alpha of a solid color fill, nor the alpha channel of
// image pixels.
// TODO(https://fxbug.dev/523371761): ratified DESIGN-blend_mode_and_opacity
// decision must match the behavior implemented here.
ResolvedBlend ResolveBlendAndOpacity(types::BlendMode stored_blend, float effective_opacity,
                                     bool pin_replace) {
  types::BlendMode blend_mode = stored_blend;
  if (blend_mode == types::BlendMode::kReplace() && effective_opacity < 1.f && !pin_replace) {
    blend_mode = types::BlendMode::kPremultipliedAlpha();
  }
  if (blend_mode == types::BlendMode::kStraightAlpha()) {
    return ResolvedBlend{
        .blend_mode = blend_mode,
        .multiply_color = {1.f, 1.f, 1.f, effective_opacity},
    };
  }
  return ResolvedBlend{
      .blend_mode = blend_mode,
      .multiply_color = {effective_opacity, effective_opacity, effective_opacity,
                         effective_opacity},
  };
}

void ComputeGlobalResolvedLayerStacks(std::vector<ResolvedLayerStack>& output,
                                      const GlobalTopologyData& topology,
                                      const UberStruct::InstanceMap& snapshot,
                                      const GlobalMatrixVector& global_matrices,
                                      const GlobalTransformClipRegionVector& clip_regions,
                                      const GlobalOpacityVector& inherited_opacities) {
  TRACE_DURATION("gfx", "ComputeGlobalResolvedLayerStacks");
  FX_DCHECK(topology.topology_vector.size() == global_matrices.size());
  FX_DCHECK(topology.topology_vector.size() == clip_regions.size());
  FX_DCHECK(topology.topology_vector.size() == inherited_opacities.size());
  FX_CHECK(topology.topology_vector.size() <=
           static_cast<size_t>(std::numeric_limits<int32_t>::max()));

  output.clear();
  if (topology.topology_vector.empty()) {
    return;
  }

  for (size_t i = 0; i < topology.topology_vector.size(); ++i) {
    const float inherited_opacity = inherited_opacities[i];
    if (inherited_opacity == 0.f) {
      continue;
    }

    const TransformHandle& handle = topology.topology_vector[i];
    auto uber_struct_kv = snapshot.find(handle.GetInstanceId());
    if (uber_struct_kv == snapshot.end()) {
      FX_DCHECK(false) << "no corresponding UberStruct for global topology entry: " << handle;
      continue;
    }
    const auto& uber_struct = uber_struct_kv->second;
    if (!uber_struct->layer_stacks.contains(handle)) {
      continue;
    }

    output.push_back(ResolvedLayerStack{
        .handle = handle,
        .topology_index = static_cast<int32_t>(i),
        .node_rotation = DecodeNodeRotation(global_matrices[i]),
        .global_matrix = global_matrices[i],
        .clip_region = clip_regions[i],
        .opacity = inherited_opacity,
    });
  }
}

void ComputeGlobalResolvedLayers(std::pmr::vector<ResolvedLayer>& output,
                                 std::span<const ResolvedLayerStack> layer_stacks,
                                 const UberStruct::InstanceMap& snapshot) {
  ComputeGlobalResolvedLayersImpl(output, layer_stacks, snapshot);
}

void ComputeGlobalResolvedLayers(std::vector<ResolvedLayer>& output,
                                 std::span<const ResolvedLayerStack> layer_stacks,
                                 const UberStruct::InstanceMap& snapshot) {
  ComputeGlobalResolvedLayersImpl(output, layer_stacks, snapshot);
}

void ComputeGlobalResolvedLayers(std::vector<ResolvedLayer>& output,
                                 const GlobalTopologyData& topology,
                                 const UberStruct::InstanceMap& snapshot,
                                 const GlobalMatrixVector& global_matrices,
                                 const GlobalTransformClipRegionVector& clip_regions,
                                 const GlobalOpacityVector& inherited_opacities) {
  std::vector<ResolvedLayerStack> layer_stacks;
  ComputeGlobalResolvedLayerStacks(layer_stacks, topology, snapshot, global_matrices, clip_regions,
                                   inherited_opacities);
  ComputeGlobalResolvedLayers(output, layer_stacks, snapshot);
}

GlobalOpacityVector ComputeGlobalOpacityValues(
    const GlobalTopologyData::TopologyVector& global_topology,
    const GlobalTopologyData::ParentIndexVector& parent_indices,
    const UberStruct::InstanceMap& uber_structs) {
  GlobalOpacityVector output;
  ComputeGlobalOpacityValues(output, global_topology, parent_indices, uber_structs);
  return output;
}

void ComputeGlobalOpacityValues(GlobalOpacityVector& output,
                                const GlobalTopologyData::TopologyVector& global_topology,
                                const GlobalTopologyData::ParentIndexVector& parent_indices,
                                const UberStruct::InstanceMap& uber_structs) {
  TRACE_DURATION("gfx", "ComputeGlobalOpacityValues");
  FX_DCHECK(global_topology.size() == parent_indices.size());

  output.clear();
  if (global_topology.empty()) {
    return;
  }

  output.reserve(global_topology.size());

  // The root entry's parent pointer points to itself, so special case it.
  const auto& root_handle = global_topology.front();
  const auto root_uber_struct_kv = uber_structs.find(root_handle.GetInstanceId());
  FX_DCHECK(root_uber_struct_kv != uber_structs.end());

  const auto root_opacity_kv = root_uber_struct_kv->second->local_opacity_values.find(root_handle);
  if (root_opacity_kv == root_uber_struct_kv->second->local_opacity_values.end()) {
    output.emplace_back(1.f);
  } else {
    output.emplace_back(root_opacity_kv->second);
  }

  for (size_t i = 1; i < global_topology.size(); ++i) {
    const TransformHandle& handle = global_topology[i];
    const size_t parent_index = parent_indices[i];

    // Every entry in the global topology comes from an UberStruct.
    const auto uber_stuct_kv = uber_structs.find(handle.GetInstanceId());
    FX_DCHECK(uber_stuct_kv != uber_structs.end());

    const auto opacity_kv = uber_stuct_kv->second->local_opacity_values.find(handle);
    if (opacity_kv == uber_stuct_kv->second->local_opacity_values.end()) {
      output.emplace_back(output[parent_index]);
    } else {
      output.emplace_back(output[parent_index] * opacity_kv->second);
    }
  }
}

}  // namespace flatland
