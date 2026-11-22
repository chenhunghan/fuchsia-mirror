// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#define SHLIB_EXPORT extern "C" __attribute__((visibility("default")))

#if defined(SHLIB_ROLE_CALLER)
extern int get_library_value();
extern "C" int get_pic_obj_value();
SHLIB_EXPORT int shlib_call_link_deps() { return get_library_value() + get_pic_obj_value(); }

#elif defined(SHLIB_ROLE_UNREFERENCED_ALWAYSLINK)
SHLIB_EXPORT int shlib_unreferenced_alwayslink_symbol() { return 77; }

#elif defined(SHLIB_ROLE_PIC_OBJ)
extern "C" int get_pic_obj_value() { return 23; }

#elif defined(SHLIB_ROLE_DUP_CALLER)
extern "C" int shlib_unreferenced_alwayslink_symbol();
SHLIB_EXPORT int shlib_call_dup_alwayslink() { return shlib_unreferenced_alwayslink_symbol(); }

#elif defined(SHLIB_ROLE_PRIVATE_BACKING)
int shlib_private_backing_value() { return 55; }

#elif defined(SHLIB_ROLE_DYN_EXPORTED)
extern int shlib_private_backing_value();
SHLIB_EXPORT int get_dyn_exported_value() { return shlib_private_backing_value() + 1; }

#elif defined(SHLIB_ROLE_DYN_DIRECT_CALLER)
extern "C" int get_dyn_exported_value();
SHLIB_EXPORT int shlib_call_direct_dyn_dedup() { return get_dyn_exported_value(); }

#elif defined(SHLIB_ROLE_DYN_MID)
SHLIB_EXPORT int shlib_mid_noop() { return 1; }

#elif defined(SHLIB_ROLE_DYN_TRANSITIVE_CALLER)
extern "C" int shlib_mid_noop();
SHLIB_EXPORT int shlib_call_transitive_dyn_dedup() { return shlib_mid_noop(); }

#elif defined(SHLIB_ROLE_HIDDEN_RUNTIME)
int shlib_hidden_per_dso_helper() { return 9; }

#elif defined(SHLIB_ROLE_HIDDEN_RUNTIME_USER)
extern int shlib_hidden_per_dso_helper();
int shlib_hidden_user_helper() { return shlib_hidden_per_dso_helper(); }

#elif defined(SHLIB_ROLE_PER_DSO_CALLER)
extern int shlib_hidden_per_dso_helper();
SHLIB_EXPORT int shlib_call_per_dso_hidden_runtime() { return shlib_hidden_per_dso_helper(); }

#elif defined(SHLIB_ROLE_EXEC_ISOLATION)
extern int get_library_value();
SHLIB_EXPORT int shlib_call_exec_isolation() { return get_library_value(); }

#else
#error "Define exactly one SHLIB_ROLE_* macro."
#endif
