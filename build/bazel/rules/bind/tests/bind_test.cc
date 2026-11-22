// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <string_view>

// `<bind/test/bindlib/child/cpp/bind.h>` includes
// `<bind/test/bindlib/parent/cpp/bind.h>` via `using test.bindlib.parent;`.
#include <bind/test/bindlib/child/cpp/bind.h>

static_assert(bind_test_bindlib_parent::SIZE_LARGE == 2);
static_assert(bind_test_bindlib_child::SIZE_MEDIUM == 3);

int main() {
  // The string constants are not `constexpr`, so check them at runtime.
  if (std::string_view(bind_test_bindlib_parent::FLAVOR) != "test.bindlib.parent.Flavor" ||
      std::string_view(bind_test_bindlib_parent::FLAVOR_VANILLA) != "vanilla" ||
      std::string_view(bind_test_bindlib_child::ENABLED) != "test.bindlib.child.enabled") {
    return 1;
  }
  return 0;
}
