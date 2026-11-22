// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "test.h"

// The build compiles this in several different ways, setting ADDEND and
// possibly ADDEND_BIAS differently in each.
#ifndef ADDEND_BIAS
#define ADDEND_BIAS 0
#endif

extern "C" uint64_t add_value(uint64_t x) { return x + ADDEND + ADDEND_BIAS; }
