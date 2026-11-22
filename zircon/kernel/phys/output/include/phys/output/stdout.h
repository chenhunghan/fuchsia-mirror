// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_OUTPUT_INCLUDE_PHYS_OUTPUT_STDOUT_H_
#define ZIRCON_KERNEL_PHYS_OUTPUT_INCLUDE_PHYS_OUTPUT_STDOUT_H_

#include <stdio.h>

#include <ktl/string_view.h>

// Returns a FILE that discards everything written to it.
FILE* NullFile();

// The object behind stdout. Output is written to the console (which is the
// UART in practice), unless verbosity is disabled, and then to the log, if
// any. Both sinks are represented as plain FILEs; what they are is up to the
// phys program. Verbosity only mutes the console; the log, if set, always sees
// everything.
class PhysStdout {
 public:
  // Sets the console sink, which is null by default.
  void set_console(FILE console) { console_ = console; }

  // Mutes (false) or unmutes (true) the console. The log is unaffected.
  void set_verbosity(bool verbose) { verbose_ = verbose; }

  // Sets the log sink, which sees all output regardless of verbosity.
  void set_log(FILE log) { log_ = log; }

  // Removes the log sink.
  void clear_log() { log_ = FILE{}; }

  // This is the FILE-compatible API; stdout is FILE{&gPhysStdout}.
  int Write(ktl::string_view str);

 private:
  FILE console_;
  FILE log_;
  bool verbose_ = true;
};

extern PhysStdout gPhysStdout;

#endif  // ZIRCON_KERNEL_PHYS_OUTPUT_INCLUDE_PHYS_OUTPUT_STDOUT_H_
