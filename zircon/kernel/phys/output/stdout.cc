// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <stdio.h>

#include <ktl/string_view.h>
#include <phys/output/stdout.h>

#include <ktl/enforce.h>

namespace {

int NullWrite(void*, const char*, size_t len) { return static_cast<int>(len); }

constinit FILE gNullFile{NullWrite, nullptr};

}  // namespace

constinit PhysStdout gPhysStdout;

// This is the stdout of <stdio.h>.  It never changes; the sinks behind it do.
constinit FILE gStdout{&gPhysStdout};

FILE* NullFile() { return &gNullFile; }

int PhysStdout::Write(ktl::string_view str) {
  if (verbose_ && console_) {
    console_.Write(str);
  }
  if (log_) {
    log_.Write(str);
  }
  return static_cast<int>(str.size());
}
