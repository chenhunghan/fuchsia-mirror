// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/ktrace.h>

// This works around a bug in the GCC build where all string categories have to be referenced
// somewhere in C++ in order for the interned string category registration to work.
void UnusedGccWorkaroundKtraceRegistration() {
  KTRACE_DURATION_BEGIN("kernel:restricted", "unused");
}
