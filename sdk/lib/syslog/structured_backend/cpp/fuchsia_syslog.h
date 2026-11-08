// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_SYSLOG_STRUCTURED_BACKEND_CPP_FUCHSIA_SYSLOG_H_
#define LIB_SYSLOG_STRUCTURED_BACKEND_CPP_FUCHSIA_SYSLOG_H_

#include <zircon/availability.h>

// Excluded at HEAD so bazel_sdk's all-headers test (which builds at HEAD) can
// still compile every header in the SDK.
#if FUCHSIA_API_LEVEL_AT_LEAST(NEXT) && FUCHSIA_API_LEVEL_LESS_THAN(HEAD)
#error "<lib/syslog/structured_backend/cpp/fuchsia_syslog.h> is obsolete and must not be included."
#endif

#endif  // LIB_SYSLOG_STRUCTURED_BACKEND_CPP_FUCHSIA_SYSLOG_H_
