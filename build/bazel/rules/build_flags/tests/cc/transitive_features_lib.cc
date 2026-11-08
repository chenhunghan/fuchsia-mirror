// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Returns 1 when the "pic" feature is disabled (omitting -fPIC, so Clang falls
// back to its default PIE mode defining __PIE__), or 0 when "pic" is enabled
// (passing -fPIC, which undefines __PIE__).
extern int is_lib_built_without_pic() {
#ifdef __PIE__
  return 1;
#else
  return 0;
#endif
}
