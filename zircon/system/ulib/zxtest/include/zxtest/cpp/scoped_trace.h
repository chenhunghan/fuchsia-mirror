// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef ZXTEST_CPP_SCOPED_TRACE_H_
#define ZXTEST_CPP_SCOPED_TRACE_H_

#include <string_view>

#include <zxtest/base/types.h>

namespace zxtest {

class ScopedTrace final {
 public:
  ScopedTrace() = delete;
  ScopedTrace(SourceLocation location, std::string_view message);
  ScopedTrace(const ScopedTrace&) = delete;
  ScopedTrace& operator=(const ScopedTrace&) = delete;
  ~ScopedTrace();
};

}  // namespace zxtest

#endif  // ZXTEST_CPP_SCOPED_TRACE_H_
