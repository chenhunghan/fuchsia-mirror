// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/llvm-profdata/llvm-profdata.h>
#include <stddef.h>
#include <stdint.h>
#include <zircon/compiler.h>

#include <kernel/ffi.h>
#include <ktl/byte.h>
#include <ktl/type_traits.h>

#include "private.h"

#include <ktl/enforce.h>

static_assert(sizeof(LlvmProfdata) == 72);
static_assert(alignof(LlvmProfdata) == 8);
static_assert(ktl::is_trivially_copyable_v<LlvmProfdata>);

static_assert(sizeof(LlvmProfdata::LiveData) == 48);
static_assert(alignof(LlvmProfdata::LiveData) == 8);
static_assert(ktl::is_trivially_copyable_v<LlvmProfdata::LiveData>);

static_assert(sizeof(LlvmProfdata::LiveDataBounds) == 16);
static_assert(alignof(LlvmProfdata::LiveDataBounds) == 8);
static_assert(offsetof(LlvmProfdata::LiveDataBounds, offset) == 0);
static_assert(offsetof(LlvmProfdata::LiveDataBounds, size) == 8);
static_assert(ktl::is_trivially_copyable_v<LlvmProfdata::LiveDataBounds>);
static_assert(ktl::is_standard_layout_v<LlvmProfdata::LiveDataBounds>);

__BEGIN_CDECLS

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_llvm_profdata_new(ffi::Uninitialized<LlvmProfdata>* out) {
  out->Initialize();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_llvm_profdata_init(LlvmProfdata* profdata, const uint8_t* build_id,
                                              size_t build_id_len) {
  profdata->Init({reinterpret_cast<const ktl::byte*>(build_id), build_id_len});
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE size_t cpp_llvm_profdata_size_bytes(const LlvmProfdata* profdata) {
  return profdata->size_bytes();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_llvm_profdata_write_fixed_data(
    LlvmProfdata* profdata, uint8_t* data, size_t data_len,
    ffi::Uninitialized<LlvmProfdata::LiveData>* out) {
  out->Initialize(profdata->WriteFixedData({reinterpret_cast<ktl::byte*>(data), data_len}));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_llvm_profdata_bounds(
    const LlvmProfdata* profdata, ffi::Uninitialized<LlvmProfdata::LiveDataBounds>* out) {
  out->Initialize(profdata->bounds());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_llvm_profdata_live_data_for_bounds(
    const LlvmProfdata* profdata, uint8_t* bounds_data, size_t bounds_size,
    ffi::Uninitialized<LlvmProfdata::LiveData>* out) {
  out->Initialize(
      profdata->LiveDataForBounds({reinterpret_cast<ktl::byte*>(bounds_data), bounds_size}));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_llvm_profdata_copy_live_data(LlvmProfdata* profdata,
                                                        const LlvmProfdata::LiveData* data) {
  profdata->CopyLiveData(*data);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_llvm_profdata_use_live_data(const LlvmProfdata::LiveData* data) {
  LlvmProfdata::UseLiveData(*data);
}

__END_CDECLS
