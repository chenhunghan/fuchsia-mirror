// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/adb/bin/adb/state-controller.h"

#include <lib/syslog/cpp/macros.h>

#include "src/developer/adb/third_party/adb/adb-protocol.h"

namespace adb {

void StateControllerServer::SetSystemType(SetSystemTypeRequest& request,
                                          SetSystemTypeCompleter::Sync& completer) {
  ConnectionState new_state;
  if (request.system_type() == fuchsia_hardware_adb::SystemType::kSideload) {
    new_state = kCsSideload;
  } else if (request.system_type() == fuchsia_hardware_adb::SystemType::kRecovery) {
    new_state = kCsRecovery;
  } else {
    new_state = kCsDevice;
  }

  ConnectionState old_state = get_system_type();
  if (old_state != new_state || reset_failed_) {
    set_system_type(new_state);
    if (reset_callback_) {
      if (zx_status_t status = reset_callback_(); status != ZX_OK) {
        // Restore the previous system type since switching failed, and record the failure so the
        // next SetSystemType() call retries resetting adb even if it requests |old_state|. The
        // caller gets the error, so this is only a warning here.
        FX_PLOGS(WARNING, status) << "Failed to reset adb for system type " << to_string(new_state)
                                  << ", keeping " << to_string(old_state);
        reset_failed_ = true;
        set_system_type(old_state);
        completer.Reply(fit::error(status));
        return;
      }
    }
    reset_failed_ = false;
  }
  completer.Reply(fit::ok());
}

void StateControllerServer::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_hardware_adb::StateController> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {}

}  // namespace adb
