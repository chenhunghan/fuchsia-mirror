// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef BUILD_BAZEL_RULES_DEVICETREE_TESTS_VISITOR_REGISTRY_H_
#define BUILD_BAZEL_RULES_DEVICETREE_TESTS_VISITOR_REGISTRY_H_

inline int& GetRegisteredVisitorCount() {
  static int count = 0;
  return count;
}

#endif  // BUILD_BAZEL_RULES_DEVICETREE_TESTS_VISITOR_REGISTRY_H_
