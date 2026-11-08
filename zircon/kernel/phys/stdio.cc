// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/boot-options/boot-options.h>
#include <stdarg.h>
#include <stdio.h>

#include <phys/stdio.h>

namespace {

int NullWrite(void*, const char*, size_t len) { return static_cast<int>(len); }

constinit FILE gNullFile{NullWrite, nullptr};

FILE* gNonVerboseOutput;

}  // namespace

constinit FILE gStdout{NullWrite, nullptr};

FILE* NullFile() { return &gNullFile; }

void SetNonVerboseOutput(FILE* f) { gNonVerboseOutput = f; }

void debugf(const char* fmt, ...) {
  FILE* outfile = stdout;

  // If kernel.phys.verbose=false then only write to the non-verbose output.
  if (BootOptions::Get() && !BootOptions::Get()->phys_verbose) {
    if (!gNonVerboseOutput) {
      return;
    }
    outfile = gNonVerboseOutput;
  }

  va_list args;
  va_start(args, fmt);
  vfprintf(outfile, fmt, args);
  va_end(args);
}
