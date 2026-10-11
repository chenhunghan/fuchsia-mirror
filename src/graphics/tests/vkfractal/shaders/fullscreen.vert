// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#version 450

// Draws one triangle that covers the whole viewport: vertices 0, 1, 2 map to (-1, -1), (3, -1)
// and (-1, 3). No vertex buffer is needed, and the parts outside the viewport are clipped before
// any fragments are shaded. A two-triangle quad would cost more: fragments are shaded in 2x2
// groups for derivatives, so the groups along the shared diagonal would be shaded twice.
void main() {
  vec2 uv = vec2((gl_VertexIndex << 1) & 2, gl_VertexIndex & 2);
  gl_Position = vec4(uv * 2.0 - 1.0, 0.0, 1.0);
}
