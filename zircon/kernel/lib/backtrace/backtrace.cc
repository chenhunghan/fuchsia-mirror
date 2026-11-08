// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "lib/backtrace.h"

#include <lib/object-constants.h>

#include <kernel/ffi.h>
#include <phys/handoff/boot-constants.h>

static_assert(sizeof(Backtrace) == kBacktraceSize);
static_assert(alignof(Backtrace) == kBacktraceAlign);
static_assert(Backtrace::kMaxSize == 32);

static constexpr const char* kFormat = "{{{bt:%zu:%p:%s}}}\n";
static constexpr const char* kRa = "ra";
static constexpr const char* kPc = "pc";

void Backtrace::Print(FILE* file) const {
  file->Write(kBootConstants.kernel_debug_ident.get());
  PrintWithoutVersion(file);
}

void Backtrace::PrintWithoutVersion(FILE* file) const {
  for (size_t i = 0; i < size_; ++i) {
    const char* type = kRa;
    if ((first_frame_type_ == FrameType::PreciseLocation) && (i == 0)) {
      type = kPc;
    }
    fprintf(file, kFormat, i, reinterpret_cast<void*>(addr_[i]), type);
  }
}

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_backtrace_print(const Backtrace* bt) { bt->Print(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_backtrace_print_without_version(const Backtrace* bt, FILE* file) {
  bt->PrintWithoutVersion(file);
}

FFI_ALWAYS_INLINE const vaddr_t* cpp_backtrace_data(const Backtrace* bt) { return bt->data(); }

FFI_ALWAYS_INLINE size_t cpp_backtrace_size(const Backtrace* bt) { return bt->size(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_backtrace_reset(Backtrace* bt) { bt->reset(); }

}  // extern "C"
