// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "test_visitor.h"

#include "visitor_registry.h"

int VisitorHelperSymbol(int x) { return x + 1; }

#ifdef FDF_DEVICETREE_STATIC_VISITOR

namespace {
__attribute__((constructor)) void RegisterTestVisitor() { GetRegisteredVisitorCount() += 1; }
}  // namespace

#else

// NOLINTNEXTLINE(bugprone-reserved-identifier)
extern "C" __attribute__((visibility("default"))) const int __devicetree_visitor_registration__ = 1;

#endif
