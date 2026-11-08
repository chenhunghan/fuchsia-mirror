#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Helper function to check if the current environment appears to be invoked by an AI agent.
# Returns 0 if invoked by an agent, 1 otherwise.
is_invoked_by_agent() {
  # Find the directory where this script lives.
  # We assume it stays in the same directory as agents.txt and agent_session_ids.txt.
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
  local agents_file="${script_dir}/agents.txt"
  local session_ids_file="${script_dir}/agent_session_ids.txt"

  local file
  for file in "$agents_file" "$session_ids_file"; do
    if [[ ! -f "$file" ]]; then
      echo "Error: ${file##*/} not found at ${file}" >&2
      return 1
    fi
  done

  for file in "$agents_file" "$session_ids_file"; do
    while IFS= read -r agent || [[ -n "$agent" ]]; do
      # Ignore empty lines and comments
      [[ -z "$agent" || "$agent" =~ ^# ]] && continue

      # Indirect expansion to check if the variable is set
      if [[ -n "${!agent}" ]]; then
        return 0
      fi
    done < "$file"
  done

  return 1
}
