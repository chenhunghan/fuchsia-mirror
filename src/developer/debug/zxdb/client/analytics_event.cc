// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/client/analytics_event.h"

#include <cstdlib>

#include "src/developer/debug/shared/logging/logging.h"
#include "src/lib/fxl/strings/string_printf.h"

namespace zxdb {

std::string ObfuscateUser(std::string_view input) {
  const char* username = std::getenv("USER");
  if (!username || username[0] == '\0') {
    return std::string(input);
  }

  constexpr std::string_view kReplacement = "$USER";
  const std::string_view username_sv(username);
  std::string ret(input);
  size_t pos = ret.find(username_sv);
  while (pos != std::string::npos) {
    ret.replace(pos, username_sv.size(), kReplacement);
    pos = ret.find(username_sv, pos + kReplacement.size());
  }
  return ret;
}

// AnalyticsEvent ----------------------------------------------------------------------------------
AnalyticsEvent::AnalyticsEvent(const std::string& name, const std::string& session_id)
    : analytics::core_dev_tools::Ga4Event(name) {
  // Note: this is a special name, and is processed by analytics differently than typical custom
  // parameters. This surfaces in the data as "ga_session_id", which (should) associate it with
  // other built-in session metrics.
  SetParameter("session_id", session_id);
}

// SessionStarted ----------------------------------------------------------------------------------
SessionStarted::SessionStarted(const std::string& session_id)
    : AnalyticsEvent("session_started", session_id) {}

void SessionStarted::SetRunByAgent(bool run_by_agent) {
  // This name is consistent with the parameter that ffx reports.
  SetParameter("is_agent_env", run_by_agent);
}

SessionConnected::SessionConnected(const std::string& session_id)
    : AnalyticsEvent("session_connected", session_id) {}

// SessionConnected --------------------------------------------------------------------------------
void SessionConnected::SetRemoteType(SessionConnected::RemoteType type) {
  std::string remote_type_string;
  switch (type) {
    case RemoteType::kRemoteAgent:
      remote_type_string = "remote agent";
      break;
    case RemoteType::kLocalAgent:
      remote_type_string = "local agent";
      break;
    case RemoteType::kMinidump:
      remote_type_string = "minidump";
      break;
  }

  SetParameter("remote_type", remote_type_string);
}

// SessionEnded ------------------------------------------------------------------------------------
SessionEnded::SessionEnded(const std::string& session_id)
    : AnalyticsEvent("session_ended", session_id) {}

void SessionEnded::SetSessionTime(std::chrono::milliseconds session_time) {
  SetParameter("session_length_ms", session_time.count());
}

// CommandEvent ------------------------------------------------------------------------------------
CommandEvent::CommandEvent(const std::string& session_id) : AnalyticsEvent("command", session_id) {}

// The CommandReport only fills in verbs, nouns, and switches using the well known strings for the
// defined set of the respective group. We will never receive user information in any of those
// strings. Arguments and error messages however, can contain arbitrary text, which must not contain
// user identifiable data.
void CommandEvent::FromCommandReport(const CommandReport& report) {
  SetParameter("verb_id", report.verb_id);
  SetParameter("verb", report.verb);
  SetParameter("command_group", report.command_group);

  SetParameter("noun_count", static_cast<int64_t>(report.nouns.size()));
  for (size_t i = 0; i < report.nouns.size(); i++) {
    SetParameter(fxl::StringPrintf("noun%zu", i), report.nouns[i].name);
    SetParameter(fxl::StringPrintf("noun%zu_id", i), report.nouns[i].id);
    SetParameter(fxl::StringPrintf("noun%zu_index", i), report.nouns[i].index);
  }

  SetParameter("argument_count", static_cast<int64_t>(report.arguments.size()));
  for (size_t i = 0; i < report.arguments.size(); i++) {
    SetParameter(fxl::StringPrintf("argument%zu", i), ObfuscateUser(report.arguments[i]));
  }

  SetParameter("switch_count", static_cast<int64_t>(report.switches.size()));
  for (size_t i = 0; i < report.switches.size(); i++) {
    SetParameter(fxl::StringPrintf("switch%zu", i), report.switches[i].name);
    SetParameter(fxl::StringPrintf("switch%zu_id", i), report.switches[i].id);
    SetParameter(fxl::StringPrintf("switch%zu_value", i), report.switches[i].value);
  }

  SetParameter("has_error", report.err.has_error());
  SetParameter("error_code", static_cast<int>(report.err.type()));
  SetParameter("error_string", ErrTypeToString(report.err.type()));
  SetParameter("error_message", ObfuscateUser(report.err.msg()));
}

// ConsoleType -------------------------------------------------------------------------------------
ConsoleType::ConsoleType(const std::string& session_id)
    : AnalyticsEvent("console_type", session_id) {}

void ConsoleType::SetConsoleType(Type type) {
  std::string console_type_string;

  switch (type) {
    case Type::kCommandLine: {
      console_type_string = "command_line";
      break;
    }
    case Type::kCommandLineEmbedded: {
      console_type_string = "command_line_embedded";
      break;
    }
    case Type::kDebugAdapter: {
      console_type_string = "debug_adapter";
      break;
    }
  }
  SetParameter("type", console_type_string);
}

}  // namespace zxdb
