// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/fifo_dispatcher.h"

#include <string.h>
#include <zircon/rights.h>

#include <kernel/mutex.h>

zx_status_t FifoDispatcher::WriteFromUser(size_t elem_size, user_in_ptr<const uint8_t> ptr,
                                          size_t count, size_t* actual) {
  return rust_fifo_dispatcher_write_from_user(this, elem_size, ptr.get(), count, actual);
}

zx_status_t FifoDispatcher::ReadToUser(size_t elem_size, user_out_ptr<uint8_t> ptr, size_t count,
                                       size_t* actual) {
  return rust_fifo_dispatcher_read_to_user(this, elem_size, ptr.get(), count, actual);
}

bool FifoDispatcher::IsFullLocked() const { return head_ - tail_ == elem_count_; }
