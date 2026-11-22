// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/ubsan-custom/handlers.h>
#include <stdarg.h>
#include <stddef.h>

#include <ktl/type_traits.h>

extern "C" {

void rust_ubsan_report_start(const char* check, const ubsan::SourceLocation* loc, void* caller,
                             void* frame);
void rust_ubsan_report_end();
void rust_ubsan_vprintf(const char* fmt, va_list args);

}  // extern "C"

static_assert(sizeof(ubsan::SourceLocation) == 16);
static_assert(alignof(ubsan::SourceLocation) == 8);
static_assert(offsetof(ubsan::SourceLocation, filename) == 0);
static_assert(offsetof(ubsan::SourceLocation, line) == 8);
static_assert(offsetof(ubsan::SourceLocation, column) == 12);
static_assert(ktl::is_trivial_v<ubsan::SourceLocation>);

ubsan::Report::Report(const char* check, const ubsan::SourceLocation& loc,  //
                      void* caller, void* frame) {
  rust_ubsan_report_start(check, &loc, caller, frame);
}

ubsan::Report::~Report() { rust_ubsan_report_end(); }

void ubsan::VPrintf(const char* fmt, va_list args) { rust_ubsan_vprintf(fmt, args); }
