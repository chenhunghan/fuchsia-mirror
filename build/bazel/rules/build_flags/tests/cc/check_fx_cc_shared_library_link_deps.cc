// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <dlfcn.h>

extern "C" {
int shlib_call_link_deps();
int shlib_call_dup_alwayslink();
int shlib_call_direct_dyn_dedup();
int shlib_call_transitive_dyn_dedup();
int shlib_call_per_dso_hidden_runtime();
int shlib_call_exec_isolation();
}

int main() {
  if (shlib_call_link_deps() != 123) {
    return 1;
  }

  void* link_deps_dso =
      dlopen("libfx_cc_shared_library_with_link_deps.so", RTLD_LAZY | RTLD_NOLOAD);
  if (!link_deps_dso) {
    return 2;
  }

  auto* unreferenced_alwayslink =
      reinterpret_cast<int (*)()>(dlsym(link_deps_dso, "shlib_unreferenced_alwayslink_symbol"));
  if (!unreferenced_alwayslink || unreferenced_alwayslink() != 77) {
    dlclose(link_deps_dso);
    return 3;
  }

  auto* user_link_flag_marker =
      reinterpret_cast<int (*)()>(dlsym(link_deps_dso, "shlib_user_link_flag_marker"));
  if (!user_link_flag_marker || user_link_flag_marker() != 23) {
    dlclose(link_deps_dso);
    return 4;
  }
  dlclose(link_deps_dso);

  if (shlib_call_dup_alwayslink() != 77) {
    return 5;
  }

  if (shlib_call_direct_dyn_dedup() != 56) {
    return 6;
  }

  if (shlib_call_transitive_dyn_dedup() != 1) {
    return 7;
  }

  if (shlib_call_per_dso_hidden_runtime() != 9) {
    return 8;
  }

  if (shlib_call_exec_isolation() != 100) {
    return 9;
  }

  return 0;
}
