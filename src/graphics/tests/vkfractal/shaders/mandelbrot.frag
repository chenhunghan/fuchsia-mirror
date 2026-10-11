// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#version 450

// LINT.IfChange(push_constants)
layout(push_constant) uniform Params {
  vec2 center;          // Complex coordinate at the center of the image.
  vec2 half_extent;     // Image size / 2, in pixels.
  vec2 step;            // Complex units per pixel, horizontally and vertically.
  uint max_iterations;
}
params;
// LINT.ThenChange(//src/graphics/tests/vkfractal/vkfractal.cc:push_constants)

layout(location = 0) out vec4 out_color;

void main() {
  // Compute the small per-pixel offset first so it keeps its precision, then add the center.
  // Imaginary values increase upward on screen.
  vec2 offset = (gl_FragCoord.xy - params.half_extent) * vec2(params.step.x, -params.step.y);
  vec2 c = params.center + offset;

  vec2 z = vec2(0.0);
  uint i = 0u;
  for (; i < params.max_iterations; ++i) {
    z = vec2(z.x * z.x - z.y * z.y, 2.0 * z.x * z.y) + c;
    if (dot(z, z) > 4.0) {
      break;
    }
  }

  out_color = vec4(0.0, 0.0, 0.0, 1.0);
  if (i < params.max_iterations) {
    // Cheap cosine palette; negligible next to the iteration loop.
    float t = float(i) / 64.0;
    out_color.rgb = 0.5 + 0.5 * cos(6.2831853 * (t + vec3(0.0, 0.33, 0.67)));
  }
}
