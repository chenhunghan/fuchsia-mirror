// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// A synthetic dispatcher used to test babeldiff. It is not part of Zircon.

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_DOORBELL_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_DOORBELL_DISPATCHER_H_

#include <object/dispatcher.h>

class DoorbellDispatcher : public SoloDispatcher<DoorbellDispatcher, ZX_DEFAULT_DOORBELL_RIGHTS> {
 public:
  ~DoorbellDispatcher() override;
  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_DOORBELL; }

  // Rings the doorbell with |tone|.
  //
  // Returns ZX_ERR_OUT_OF_RANGE if |tone| is above kMaxTone.
  virtual zx_status_t Ring(uint32_t tone) = 0;

  // Returns how many times the doorbell has rung.
  uint64_t RingCount() const;

 protected:
  DoorbellDispatcher();
  void RecordRingLocked() TA_REQ(get_lock());

 private:
  uint64_t rings_ TA_GUARDED(get_lock()) = 0;
};

class ChimeDoorbellDispatcher final : public DoorbellDispatcher {
 public:
  static zx_status_t Create(uint32_t notes, KernelHandle<DoorbellDispatcher>* handle,
                            zx_rights_t* rights);
  zx_status_t Ring(uint32_t tone) override;

 private:
  explicit ChimeDoorbellDispatcher(uint32_t notes);
  const uint32_t notes_;
};

class BuzzerDoorbellDispatcher final : public DoorbellDispatcher {
 public:
  static zx_status_t Create(zx_duration_t length, KernelHandle<DoorbellDispatcher>* handle,
                            zx_rights_t* rights);
  zx_status_t Ring(uint32_t tone) override;

 private:
  explicit BuzzerDoorbellDispatcher(zx_duration_t length);
  const zx_duration_t length_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_DOORBELL_DISPATCHER_H_
