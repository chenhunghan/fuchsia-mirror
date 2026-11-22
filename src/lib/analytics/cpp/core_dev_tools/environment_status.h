// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_ANALYTICS_CPP_CORE_DEV_TOOLS_ENVIRONMENT_STATUS_H_
#define SRC_LIB_ANALYTICS_CPP_CORE_DEV_TOOLS_ENVIRONMENT_STATUS_H_

namespace analytics::core_dev_tools {

bool IsRunByBot();

struct BotInfo {
  const char* const environment = nullptr;
  const char* const name = nullptr;

  bool IsRunByBot() const;
};

// Returns information of the CI bot. When IsRunByBot() returns false, it returns {nullptr,
// nullptr, nullptr}
BotInfo GetBotInfo();

// Returns true if the current process appears to be invoked by an AI agent. An agent is detected
// when any of the environment variables listed in //tools/devshell/lib/agents.txt or
// //tools/devshell/lib/agent_session_ids.txt is set to a non-empty value.
bool IsRunByAgent();

bool IsDisabledByEnvironment();

}  // namespace analytics::core_dev_tools

#endif  // SRC_LIB_ANALYTICS_CPP_CORE_DEV_TOOLS_ENVIRONMENT_STATUS_H_
