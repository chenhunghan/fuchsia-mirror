// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_FIFO_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_FIFO_DISPATCHER_H_

#include <lib/user_copy/user_ptr.h>
#include <zircon/types.h>

#include <object/dispatcher.h>

class FifoDispatcher final : public PeeredDispatcher<FifoDispatcher, ZX_DEFAULT_FIFO_RIGHTS> {
 public:
  // Writes |count| elements of |elem_size| bytes from |ptr| into the peer.
  //
  // Returns ZX_ERR_OUT_OF_RANGE if |elem_size| does not match the fifo.
  zx_status_t WriteFromUser(size_t elem_size, user_in_ptr<const uint8_t> ptr, size_t count,
                            size_t* actual);

  // Reads up to |count| elements of |elem_size| bytes into |ptr|.
  zx_status_t ReadToUser(size_t elem_size, user_out_ptr<uint8_t> ptr, size_t count,
                         size_t* actual);

 private:
  zx_status_t WriteSelfLocked(size_t elem_size, user_in_ptr<const uint8_t> ptr, size_t count,
                              size_t* actual) TA_REQ(get_lock());

  // Returns true if the fifo has no room for another element.
  bool IsFullLocked() const TA_REQ(get_lock());

  const uint32_t elem_count_;
  const uint32_t elem_size_;
  uint32_t head_ TA_GUARDED(get_lock()) = 0;
  uint32_t tail_ TA_GUARDED(get_lock()) = 0;
  ktl::unique_ptr<uint8_t[]> data_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_FIFO_DISPATCHER_H_
