// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/loop.h>
#include <lib/syslog/cpp/log_settings.h>

#include "adb-file-sync.h"

int main(int argc, char** argv) {
  async::Loop loop{&kAsyncLoopConfigNeverAttachToThread};
  fuchsia_logging::LogSettingsBuilder builder;
  builder.WithTags({"adb"}).WithDispatcher(loop.dispatcher()).BuildAndInitialize();

  return adb_file_sync::AdbFileSync::StartService(
      adb_file_sync_config::Config::TakeFromStartupHandle(), loop);
}
