// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_CONSOLE_SWITCH_RECORD_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_CONSOLE_SWITCH_RECORD_H_

#include <concepts>

namespace zxdb {

// Information on a switch provided to a command. This is not for command-line
// switches passed to main().
struct SwitchRecord {
  SwitchRecord() = default;
  SwitchRecord(const SwitchRecord&) = default;
  SwitchRecord(int i, bool has_value, const char* n, char c = 0)
      : id(i), has_value(has_value), name(n), ch(c) {}

  // Deleting bool overloads prevents callers from accidentally passing a boolean flag that
  // implicitly converts to the short switch character parameter.
  template <std::same_as<bool> T>
  SwitchRecord(int i, bool has_value, const char* n, T) = delete;

  // MultiValue creates a switch that accepts an argument and can be specified multiple times.
  static SwitchRecord MultiValue(int id, const char* name, char ch = 0) {
    SwitchRecord record(id, true, name, ch);
    record.multi_value = true;
    return record;
  }

  template <std::same_as<bool> T>
  static SwitchRecord MultiValue(int id, const char* name, T) = delete;

  ~SwitchRecord() = default;

  int id = 0;

  // Indicates if this switch has a value. False means it's a bool.
  bool has_value = false;

  // Indicates if this switch can be specified multiple times to collect a list of values.
  bool multi_value = false;

  // Not including hyphens, e.g. "size" for the switch "--size".
  const char* name = nullptr;

  // 1-character shorthand switch. 0 means no short variant.
  char ch = 0;
};

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_CONSOLE_SWITCH_RECORD_H_
