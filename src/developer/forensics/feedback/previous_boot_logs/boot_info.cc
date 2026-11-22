// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/previous_boot_logs/boot_info.h"

#include <rapidjson/document.h>
#include <rapidjson/stringbuffer.h>
#include <rapidjson/writer.h>

namespace forensics::feedback {
namespace {

constexpr char kBootIdKey[] = "boot_id";
constexpr char kUptimeMsKey[] = "uptime_ms";
constexpr char kUtcBootDifferenceNanosKey[] = "utc_boot_difference_nanos";

}  // namespace

std::string BootInfo::ToJson() const {
  rapidjson::Document doc;
  doc.SetObject();
  auto& allocator = doc.GetAllocator();

  if (boot_id.has_value()) {
    doc.AddMember(kBootIdKey, *boot_id, allocator);
  }

  if (uptime.has_value()) {
    doc.AddMember(kUptimeMsKey, uptime->to_msecs(), allocator);
  }

  if (utc_boot_difference.has_value()) {
    doc.AddMember(kUtcBootDifferenceNanosKey, utc_boot_difference->to_nsecs(), allocator);
  }

  rapidjson::StringBuffer buffer;
  rapidjson::Writer<rapidjson::StringBuffer> writer(buffer);
  doc.Accept(writer);
  return buffer.GetString();
}

std::optional<BootInfo> BootInfo::FromJson(const std::string_view json) {
  rapidjson::Document doc;
  if (doc.Parse(json.data(), json.size()).HasParseError() || !doc.IsObject()) {
    return std::nullopt;
  }

  BootInfo info;
  if (doc.HasMember(kBootIdKey)) {
    if (!doc[kBootIdKey].IsString()) {
      return std::nullopt;
    }
    info.boot_id = std::string(doc[kBootIdKey].GetString(), doc[kBootIdKey].GetStringLength());
  }

  if (doc.HasMember(kUptimeMsKey)) {
    if (!doc[kUptimeMsKey].IsInt64()) {
      return std::nullopt;
    }
    info.uptime = zx::msec(doc[kUptimeMsKey].GetInt64());
  }

  if (doc.HasMember(kUtcBootDifferenceNanosKey)) {
    if (!doc[kUtcBootDifferenceNanosKey].IsInt64()) {
      return std::nullopt;
    }
    info.utc_boot_difference = zx::nsec(doc[kUtcBootDifferenceNanosKey].GetInt64());
  }

  return info;
}

}  // namespace forensics::feedback
