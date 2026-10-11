// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <kernel/ffi.h>
#include <object/handle.h>
#include <object/handle_table.h>
#include <object/process_dispatcher.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void* cpp_handle_table_lock(const HandleTable* handle_table) {
  return handle_table->get_lock();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_koid_t cpp_handle_table_koid(const HandleTable* handle_table) {
  return handle_table->get_koid();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_handle_t cpp_handle_table_map_handle_to_value(const HandleTable* handle_table,
                                                                   const Handle* handle) {
  return handle_table->MapHandleToValue(handle);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Handle* cpp_handle_table_get_handle_locked(
    HandleTable* handle_table, ProcessDispatcher* caller,
    zx_handle_t handle_value) TA_NO_THREAD_SAFETY_ANALYSIS {
  return handle_table->GetHandleLocked(*caller, handle_value);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_handle_table_add_handle_locked(HandleTable* handle_table, Handle* handle)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  handle_table->AddHandleLocked(HandleOwner(handle));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Handle* cpp_handle_table_remove_handle_locked(
    HandleTable* handle_table, Handle* handle) TA_NO_THREAD_SAFETY_ANALYSIS {
  return handle_table->RemoveHandleLocked(handle).release();
}

}  // extern "C"
