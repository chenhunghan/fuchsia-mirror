// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#version 450

layout(set = 0, binding = 0) uniform sampler2D source;

layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 out_color;

void main() {
  out_color = texture(source, uv);
}
