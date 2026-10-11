// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#version 450

// Set when the display is taller than it is wide: the rendered image is rotated 90 degrees
// clockwise onto it.
layout(constant_id = 0) const bool kRotateClockwise = false;

// Draws one triangle that covers the whole viewport, like fullscreen.vert, and passes each
// vertex's texture coordinate so that the image's corners land on the viewport's corners.
layout(location = 0) out vec2 out_uv;

void main() {
  // The vertex's position in the viewport: (0, 0) is the top left, (1, 1) the bottom right.
  vec2 position = vec2((gl_VertexIndex << 1) & 2, gl_VertexIndex & 2);
  // Rotating clockwise puts the image's top row on the viewport's right column and its left
  // column on the viewport's top row.
  out_uv = kRotateClockwise ? vec2(position.y, 1.0 - position.x) : position;
  gl_Position = vec4(position * 2.0 - 1.0, 0.0, 1.0);
}
