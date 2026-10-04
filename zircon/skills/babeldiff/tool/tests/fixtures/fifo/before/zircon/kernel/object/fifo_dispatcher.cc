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
  canary_.Assert();

  Guard<CriticalMutex> guard{get_lock()};
  if (!peer()) {
    return ZX_ERR_PEER_CLOSED;
  }
  return peer()->WriteSelfLocked(elem_size, ptr, count, actual);
}

zx_status_t FifoDispatcher::WriteSelfLocked(size_t elem_size, user_in_ptr<const uint8_t> ptr,
                                            size_t count, size_t* actual) {
  canary_.Assert();

  if (elem_size != elem_size_) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  if (count == 0) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  uint32_t old_head = head_;

  // Total number of available empty slots in the fifo.
  size_t avail = elem_count_ - (head_ - tail_);

  if (avail == 0) {
    return ZX_ERR_SHOULD_WAIT;
  }

  bool was_empty = (head_ == tail_);

  if (count > avail) {
    count = avail;
  }

  while (count > 0) {
    uint32_t offset = (head_ % elem_count_);

    // Number of slots from target to end of fifo.
    size_t n = (elem_count_ - offset);
    if (n > count) {
      n = count;
    }

    zx_status_t status = ptr.copy_array_from_user(&data_[offset * elem_size_], n * elem_size_);
    if (status != ZX_OK) {
      // Roll back, in case this is the second copy.
      head_ = old_head;
      return ZX_ERR_INVALID_ARGS;
    }

    head_ += static_cast<uint32_t>(n);
    count -= n;
    ptr = ptr.byte_offset(n * elem_size_);
  }

  if (was_empty) {
    UpdateStateLocked(0u, ZX_FIFO_READABLE);
  }

  if (IsFullLocked()) {
    UpdateStateLocked(ZX_FIFO_WRITABLE, 0u);
  }

  *actual = (head_ - old_head);
  return ZX_OK;
}

zx_status_t FifoDispatcher::ReadToUser(size_t elem_size, user_out_ptr<uint8_t> ptr, size_t count,
                                       size_t* actual) {
  canary_.Assert();

  if (elem_size != elem_size_) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  if (count == 0) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  Guard<CriticalMutex> guard{get_lock()};

  // Total number of available entries to read from the fifo.
  size_t avail = (head_ - tail_);

  if (avail == 0) {
    return peer() ? ZX_ERR_SHOULD_WAIT : ZX_ERR_PEER_CLOSED;
  }

  bool was_full = IsFullLocked();

  if (count > avail) {
    count = avail;
  }

  uint32_t old_tail = tail_;
  while (count > 0) {
    uint32_t offset = (tail_ % elem_count_);

    // Number of slots from target to end of fifo.
    size_t n = (elem_count_ - offset);
    if (n > count) {
      n = count;
    }

    zx_status_t status = ptr.copy_array_to_user(&data_[offset * elem_size_], n * elem_size_);
    if (status != ZX_OK) {
      // Roll back, in case this is the second copy.
      tail_ = old_tail;
      return ZX_ERR_INVALID_ARGS;
    }

    tail_ += static_cast<uint32_t>(n);
    count -= n;
    ptr = ptr.byte_offset(n * elem_size_);
  }

  // If the fifo was full, it is now writable.
  if (was_full && peer()) {
    peer()->UpdateStateLocked(0u, ZX_FIFO_WRITABLE);
  }

  *actual = (tail_ - old_tail);
  return ZX_OK;
}

bool FifoDispatcher::IsFullLocked() const { return head_ - tail_ == elem_count_; }
