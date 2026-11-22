// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_BACKTRACE_INCLUDE_LIB_BACKTRACE_H_
#define ZIRCON_KERNEL_LIB_BACKTRACE_INCLUDE_LIB_BACKTRACE_H_

#include <stdint.h>
#include <stdio.h>
#include <sys/types.h>

// Contains an array of return address values representing a thread's backtrace.
//
// This class is not thread-safe.
class Backtrace {
 public:
  // The maximum number of elements in a backtrace.  The value should be large enough to capture the
  // full call stack for most kernel crashes, but also small enough that it's reasonable to allocate
  // a Backtrace on the stack.
  static constexpr size_t kMaxSize = 32;

  // Returns the number of elements in this backtrace.
  size_t size() const { return size_; }

  // Returns a pointer to the underlying elements.
  const vaddr_t* data() const { return addr_; }

  // Resets the size to 0.
  void reset();

  // Adds one element to the array.  See also |set_first_frame_type|.
  void push_back(vaddr_t addr);

  // With the possible exception of the first frame, each address in the backtrace is a return
  // address.  The first frame may be either a return address or a precise location (think PC).  See
  // the "Presentation elements" section of //docs/reference/kernel/symbolizer_markup.md for
  // details.  Unless specified, the first frame is assumed to be a return address.
  enum FrameType : uint32_t { ReturnAddress = 0, PreciseLocation = 1 };
  void set_first_frame_type(FrameType type);

  // Pretty-prints this backtrace to |file|.
  void Print(FILE* file = stdout) const;
  void PrintWithoutVersion(FILE* file = stdout) const;

 private:
  friend struct BacktraceLayoutCheck;

  vaddr_t addr_[kMaxSize]{};
  size_t size_{};
  [[maybe_unused]] FrameType first_frame_type_{ReturnAddress};
};

extern "C" {

// TODO(https://fxbug.dev/507423574): Remove once all C++ callers of Backtrace are ported to Rust.
// Defined in Rust: //zircon/kernel/lib/backtrace/src/backtrace_ffi.rs.
void rust_backtrace_reset(Backtrace* bt);
void rust_backtrace_push_back(Backtrace* bt, vaddr_t addr);
void rust_backtrace_set_first_frame_type(Backtrace* bt, uint32_t type);
void rust_backtrace_print(const Backtrace* bt, FILE* file);
void rust_backtrace_print_without_version(const Backtrace* bt, FILE* file);

}  // extern "C"

inline void Backtrace::reset() { rust_backtrace_reset(this); }

inline void Backtrace::push_back(vaddr_t addr) { rust_backtrace_push_back(this, addr); }

inline void Backtrace::set_first_frame_type(FrameType type) {
  rust_backtrace_set_first_frame_type(this, static_cast<uint32_t>(type));
}

inline void Backtrace::Print(FILE* file) const { rust_backtrace_print(this, file); }

inline void Backtrace::PrintWithoutVersion(FILE* file) const {
  rust_backtrace_print_without_version(this, file);
}

#endif  // ZIRCON_KERNEL_LIB_BACKTRACE_INCLUDE_LIB_BACKTRACE_H_
