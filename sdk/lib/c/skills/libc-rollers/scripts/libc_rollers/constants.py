# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Immutable configuration defaults, URLs, failure patterns, and CLI schemas."""

import re
import types

# BEGIN AUTO-GENERATED DEFAULT_ROLLERS
DEFAULT_ROLLERS = {
    "llvm-libc-roller": {
        "bucket": "global.roller",
        "manifest_project": "llvm-project/libc",
        "remote": "https://llvm.googlesource.com/llvm-project/libc",
        "path": "third_party/llvm-libc/src",
        "schedule": "with 4h interval",
    },
    "scudo-roller": {
        "bucket": "global.roller",
        "manifest_project": "scudo",
        "remote": "https://llvm.googlesource.com/scudo",
        "path": "third_party/scudo/src",
        "schedule": "0 5 * * *",
    },
    "gwp_asan-roller": {
        "bucket": "global.roller",
        "manifest_project": "gwp-asan",
        "remote": "https://llvm.googlesource.com/gwp-asan",
        "path": "third_party/scudo/gwp_asan",
        "schedule": "0 4 * * *",
    },
    "linux-syscall-support-roller": {
        "bucket": "global.roller",
        "manifest_project": "linux-syscall-support",
        "remote": "https://chromium.googlesource.com/linux-syscall-support",
        "path": "third_party/linux-syscall-support/src",
        "schedule": "triggered",
    },
}
# END AUTO-GENERATED DEFAULT_ROLLERS

MANIFEST_URL = (
    "https://fuchsia.googlesource.com/fuchsia/+/refs/heads/main/"
    "manifests/third_party/all?format=TEXT"
)

README_URL = (
    "https://fuchsia.googlesource.com/fuchsia/+/refs/heads/main/"
    "sdk/lib/c/README.md?format=TEXT"
)

MANUAL_ROLL_URL = (
    "https://fuchsia.googlesource.com/fuchsia/+/refs/heads/main/"
    "sdk/lib/c/manual-roll.sh?format=TEXT"
)

SCHEDULER_PRPC_BASE = (
    "https://luci-scheduler.appspot.com/prpc/scheduler.Scheduler"
)

GERRIT_BASE_URL = "https://fuchsia-review.googlesource.com"
BUILDBUCKET_BUILD_BASE_URL = "https://cr-buildbucket.appspot.com/build"

TOOL_ENV_VARS = types.MappingProxyType(
    {
        "bb": "BB_BIN",
        "luci-auth": "LUCI_AUTH_BIN",
    }
)

TOOL_PREBUILT_DIRS = types.MappingProxyType(
    {
        "bb": ("prebuilt", "tools", "buildbucket"),
        "luci-auth": ("prebuilt", "tools", "luci-auth"),
    }
)

DOWNSTREAM_WRAPPER_PATTERN = re.compile(
    r"(?:"
    r"\bdownstream presubmits? failed\b|"
    r"\bexternal tryjobs? failed\b|"
    r"\bfailed to roll changes:\s*presubmit failed\b"
    r")",
    re.IGNORECASE,
)

REAL_FAILURE_PATTERNS = re.compile(
    r"(?:"
    r"sdk/lib/c(?:/|\b)|"
    r"sdk/lib/ld(?:/|\b)|"
    r"src/lib/elfldltl|"
    r"third_party/llvm-libc|"
    r"third_party/scudo|"
    r"third_party/linux-syscall-support|"
    r"llvm-libc|"
    r"gwp[_-]asan|"
    r"linux[_-]syscall[_-]support|"
    r"\bscudo\b|"
    r"zircon/system/ulib/c(?:/|\b)|"
    r"zircon/third_party/ulib/musl|"
    r"zircon/system/utest/core|"
    r"(?<![A-Za-z0-9_-])(?:boot[_-])?libc[_-]unittests\b|"
    r"(?<![A-Za-z0-9_-])libc[_-](?:sanitizer[_-])?tests?\b|"
    r"(?<![A-Za-z0-9_-])c[_-]ulib[_-]tests?\b|"
    r"(?<![A-Za-z0-9_-])scudo[_-]unittests\b|"
    r"(?<![A-Za-z0-9_-])gwp[_-]asan[_-]tests?\b|"
    r"(?<![A-Za-z0-9_-])core[_-]tests\b|"
    r"\buserboot\b|"
    r"(?<![A-Za-z0-9_-])ld[_-](?:unittests|tests)\b|"
    r"(?<![A-Za-z0-9_-])dl[_-](?:unittests|tests)\b|"
    r"elfldltl|"
    r"\.ifs\b|"
    r"ifs_shared_library|"
    r"golden.*(?:sdk/lib/c|libc)|"
    r"undefined symbol|"
    r"duplicate symbol|"
    r"ld\.lld:\s*error:"
    r")",
    re.IGNORECASE,
)

UNRELATED_FLAKE_PATTERNS = re.compile(
    r"(?:"
    r"Fastboot_error|"
    r"Timeout_attempting_to_reach_target|"
    r"Target_specification_.*_was_not_found|"
    r"botanist_ERROR|"
    r"could_not_resolve_target_IP_address|"
    r"failed to establish ssh|"
    r"sys_config:\s*ERROR|"
    r"astro_[a-z]+_release_to_canary|"
    r"fx_script_tests\.sh|"
    r"compare built artifacts in gen/build/bazel|"
    r"verify_bazel2gn|"
    r"out of sync with BUILD\.bazel|"
    r"bazel_target_can_only_be_referenced_by_one_bazel_action|"
    r"FAILED:\s*\[code=\d+\]\s*\S+\.clippy\b|"
    r"This failure may be a false rejection|"
    r"bot_died|"
    r"timed out|"
    r"mypy_checked|"
    r"bazel_tests_utils\.py|"
    r"screen-capture|"
    r"obj/vendor/"
    r")",
    re.IGNORECASE,
)

NINJA_AUX_SUFFIXES = (
    ".stamp",
    ".rmeta",
    ".d",
    ".rsp",
    ".debug_symbols.json",
    ".sym",
    ".map",
    ".toc",
)

# Path components too generic to identify a component in a revert subject.
GENERIC_DIRS = frozenset(
    {
        "core",
        "main",
        "test",
        "tests",
        "util",
        "utils",
        "common",
        "impl",
        "fuchsia",
        "commit",
        "change",
        "revert",
        "reverts",
        "review",
        "reason",
        "original",
    }
)

# Backoff schedule (in seconds) by streak count for persistent unrelated
# failures: streak 1 (non-persistent): 0s; streak 2: 2h; streak 3: 4h; 4+: 8h.
PERSISTENT_BACKOFF_SECONDS = types.MappingProxyType(
    {
        1: 0,
        2: 2 * 3600,
        3: 4 * 3600,
    }
)

MAX_PERSISTENT_BACKOFF_SECONDS = 8 * 3600

DEFAULT_SLO_CONFIG = types.MappingProxyType(
    {
        "llvm-libc-roller": types.MappingProxyType(
            {
                "target_hours": 12.0,
                "breach_hours": 24.0,
                "target_compliance_pct": 80.0,
                "cadence_note": (
                    "4h interval + Gitiles trigger "
                    "(1-2 attempts <= 12h, breach > 24h)"
                ),
            }
        ),
        "scudo-roller": types.MappingProxyType(
            {
                "target_hours": 18.0,
                "breach_hours": 36.0,
                "target_compliance_pct": 80.0,
                "cadence_note": (
                    "Gitiles trigger + daily 05:00 UTC fallback "
                    "(1-2 attempts <= 18h, breach > 36h)"
                ),
            }
        ),
        "gwp_asan-roller": types.MappingProxyType(
            {
                "target_hours": 18.0,
                "breach_hours": 36.0,
                "target_compliance_pct": 80.0,
                "cadence_note": (
                    "Gitiles trigger + daily 04:00 UTC fallback "
                    "(1-2 attempts <= 18h, breach > 36h)"
                ),
            }
        ),
        "linux-syscall-support-roller": types.MappingProxyType(
            {
                "target_hours": 18.0,
                "breach_hours": 36.0,
                "target_compliance_pct": 80.0,
                "cadence_note": (
                    "Gitiles trigger on refs/heads/main "
                    "(1-2 attempts <= 18h, breach > 36h)"
                ),
            }
        ),
    }
)

JSON_SCHEMA_EPILOG = """\
JSON Output Schema (--json):
  {
    "tree_status": {
      "tree": str, "state": str, "message": str, "epoch": float | null
    },
    "rollers": {
      "<roller_name>": {
        "luci_project": str, "bucket": str, "builder": str,
        "scheduler_job": str,
        "pending": {
          "pinned_revision": str, "upstream_head": str,
          "pending_count": int, "pending_commits": list
        },
        "last_landed": {
          "commit": str | null, "epoch": float | null,
          "cl_num": str | null, "cl_url": str | null
        },
        "slo": {
          "status": "healthy" | "warning" | "breach",
          "lag_hours": float, "stats": dict
        },
        "recent_builds": list[dict],
        "failure_classification": {
          "actionable": list[dict], "flakes": list[dict],
          "issue_key": str | null, "issue_title": str | null
        } | null,
        "retrigger_policy": {
          "should_retrigger": bool, "is_persistent": bool,
          "streak_count": int, "backoff_reasons": list[str]
        } | null
      }
    },
    "snapshot": {
      "updated_epoch": float, "rollers": list[dict],
      "watched_cls": list[dict], "markdown_table": str
    },
    "markdown_table": str,
    "skill_updates": list[str],
    "monitor_summary": {
      "retriggered": list[{"roller": str, "build_id": str, "flakes": str}],
      "retrigger_errors": list[{"roller": str, "build_id": str, "error": str}],
      "held_flakes": list[{
        "roller": str, "build_id": str, "reasons": list[str]
      }],
      "actionable_issues": list[{
        "roller": str, "builder": str, "issue_key": str, "issue_title": str,
        "roller_build_id": str, "roller_build_url": str,
        "roll_cl_num": str, "roll_cl_url": str,
        "failing_builders": list[dict], "pending_shas": list[str],
        "pinned_revision": str, "upstream_head": str,
        "upstream_remote": str, "target_path": str
      }]
    } | null
  }
"""
