// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_OUTPUT_INCLUDE_PHYS_OUTPUT_LOG_H_
#define ZIRCON_KERNEL_PHYS_OUTPUT_INCLUDE_PHYS_OUTPUT_LOG_H_

#include <stdio.h>

#include <ktl/span.h>
#include <ktl/string_view.h>
#include <phys/allocation/allocation.h>

// The Log is a FILE-compatible object that accumulates output in a contiguous
// buffer of whole pages suitable for later handoff.  It knows nothing about
// stdout; to capture stdout, install FILE{&log} with gPhysStdout.set_log()
// (see <phys/output/stdout.h>).  Page allocation failures cause a panic; the
// panic path detaches the log from stdout before printing anything.
class Log {
 public:
  // Appends to the log.  This is what FILE{&log} calls.
  int Write(ktl::string_view str);

  // This can be used to track the current total size of the accumulated log.
  size_t size_bytes() const { return size_; }

  bool empty() const { return size_ == 0; }

  // This takes ownership of the log's contiguous pages.  After this, no other
  // methods can be used.  In particular, size_bytes() should be taken first to
  // get the exact content size rather than the page-rounded buffer size.  The
  // log should be detached from stdout before this is called.
  Allocation TakeBuffer() && {
    size_ = 0;
    return ktl::move(buffer_);
  }

  // This borrows a snapshot of the accumulated log text.  **NOTE!** The view
  // returned here must be used and discarded before anything might append to
  // the log in any fashion, which many paths can do indirectly.
  ktl::string_view BorrowSnapshot() const {
    return {reinterpret_cast<const char*>(buffer_.get()), size_};
  }

 private:
  ktl::span<char> buffer_chars() {
    return {reinterpret_cast<char*>(buffer_.get()), buffer_.size_bytes()};
  }

  ktl::span<char> buffer_chars_left() { return buffer_chars().subspan(size_); }

  Allocation buffer_;
  size_t size_ = 0;
};

// The log being accumulated for handoff, if any.
extern Log* gLog;

#endif  // ZIRCON_KERNEL_PHYS_OUTPUT_INCLUDE_PHYS_OUTPUT_LOG_H_
