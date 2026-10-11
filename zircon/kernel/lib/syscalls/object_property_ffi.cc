// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/user_copy/user_ptr.h>
#include <zircon/syscalls/object.h>
#include <zircon/types.h>

#include <object/dispatcher.h>
#include <object/stream_dispatcher.h>

#include "object_property_priv.h"

#include <ktl/enforce.h>

extern "C" {

zx_status_t cpp_object_get_property_cpp_types(const Dispatcher* dispatcher, uint32_t property,
                                              void* _value, size_t size) {
  user_out_ptr<void> value(_value);
  if (property == ZX_PROP_STREAM_MODE_APPEND) {
    if (size < sizeof(uint8_t)) {
      return ZX_ERR_BUFFER_TOO_SMALL;
    }
    auto stream = DownCastDispatcher<const StreamDispatcher>(dispatcher);
    if (!stream) {
      return ZX_ERR_WRONG_TYPE;
    }
    uint8_t val = stream->IsInAppendMode();
    return value.reinterpret<uint8_t>().copy_to_user(val);
  }
  return ZX_ERR_NOT_SUPPORTED;
}

zx_status_t cpp_object_set_property_cpp_types(Dispatcher* dispatcher, uint32_t property,
                                              const void* _value, size_t size, zx_rights_t rights) {
  user_in_ptr<const void> value(_value);
  if (property == ZX_PROP_STREAM_MODE_APPEND) {
    if (size < sizeof(uint8_t)) {
      return ZX_ERR_BUFFER_TOO_SMALL;
    }
    auto stream = DownCastDispatcher<StreamDispatcher>(dispatcher);
    if (!stream) {
      return ZX_ERR_WRONG_TYPE;
    }
    uint8_t val = 0;
    zx_status_t status = value.reinterpret<const uint8_t>().copy_from_user(&val);
    if (status != ZX_OK) {
      return status;
    }
    return stream->SetAppendMode(val);
  }
  return ZX_ERR_NOT_SUPPORTED;
}

}  // extern "C"
