// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_ADB_BIN_ADB_STATE_CONTROLLER_H_
#define SRC_DEVELOPER_ADB_BIN_ADB_STATE_CONTROLLER_H_

#include <fidl/fuchsia.hardware.adb/cpp/fidl.h>
#include <lib/fit/function.h>

namespace adb {

class StateControllerServer : public fidl::Server<fuchsia_hardware_adb::StateController> {
 public:
  // Called when the system type changes. Returns an error if adb could not be reset, in which case
  // the previous system type is restored.
  using ResetCallback = fit::function<zx_status_t()>;
  void set_reset_callback(ResetCallback callback) { reset_callback_ = std::move(callback); }
  void SetSystemType(SetSystemTypeRequest& request,
                     SetSystemTypeCompleter::Sync& completer) override;
  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_adb::StateController> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;

 private:
  ResetCallback reset_callback_ = nullptr;
  bool reset_failed_ = false;
};

}  // namespace adb

#endif  // SRC_DEVELOPER_ADB_BIN_ADB_STATE_CONTROLLER_H_
