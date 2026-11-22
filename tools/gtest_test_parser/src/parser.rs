// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::borrow::Cow;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use crate::schema::{FailureReason, TestCaseResult, TestStatus};

static ANSI_ESCAPE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").unwrap());

/// Matches leading log prefixes to strip before parsing, following the
/// host -> device prefix hierarchy:
///
/// 1. An optional host/runner wall-clock timestamp at column 0 (Botanist
///    `NewTimestampWriter`, Go standard logger, or ISO 8601 / RFC 3339, e.g.
///    `2026-11-16 23:40:03.634259 `, `2026/11/16 15:49:39.711011 `,
///    `2026-11-16T23:40:03.123456Z `), optionally followed by a single space
///    and at most one bracketed/Zircon device prefix.
/// 2. Or at most one standalone bracketed prefix at column 0:
///    - Bracketed elapsed seconds, optionally followed by a Zircon debuglog
///      `pid:tid>` header (e.g. `[ 12.345 ] `, `[00012.100] `,
///      `[00000.078] 01166:01169> `)
///    - Bracketed clock time (e.g. `[ 12:34:56.789 ] `, `[12:34:56] `)
///    - Bracketed ISO 8601 / Go timestamps (e.g. `[ 2026-08-28T12:34:56Z ] `,
///      `[2026-08-28 12:34:56.789] `, `[2026/08/28 12:34:56.789] `)
///
/// Matching this bounded hierarchy explicitly ensures payload tokens and
/// leading indentation after the log header (such as `[1.5] expected != actual`,
/// `[31:16] bitfield`, or indented timestamps in failure output) are preserved.
static LOG_PREFIX_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    // ISO 8601 / RFC 3339 / Botanist / Go logger date-time timestamp.
    const DATETIME: &str =
        r"\d{4}[-/]\d{2}[-/]\d{2}[T\s]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?";
    // 24-hour clock time (HH:MM or HH:MM:SS with optional fractional seconds).
    const CLOCK_TIME: &str = r"(?:[01]?\d|2[0-3]):[0-5]\d(?::[0-5]\d)?(?:\.\d+)?";
    // Require trailing whitespace or EOL so tokens glued to punctuation/text are left intact.
    // Collapse prefix-only lines with trailing spaces (`\s+$`), otherwise consume only the
    // single delimiter space (`\s`) to preserve failure-message indentation.
    const DELIM: &str = r"(?:\s+$|\s|$)";

    let bracketed_prefix = format!(
        r"(?:\[\s*\d+\.\d+\s*\](?:\s\d+:\d+>)?|\[\s*(?:{CLOCK_TIME}|{DATETIME})\s*\]){DELIM}"
    );
    let pattern =
        format!(r"^(?:{DATETIME}(?:\s+$|\s(?:{bracketed_prefix})?|$)|{bracketed_prefix})");
    Regex::new(&pattern).unwrap()
});

static GTEST_RUN_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[\s*RUN\s*\]\s*(\S+?)\.(\S+)$").unwrap());

static GTEST_COMPLETION_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[\s*(OK|FAILED|SKIPPED|DISABLED)\s*\]\s*(\S+?)\.(\S+?)(?:\s*\((\d+)\s*ms\))?$")
        .unwrap()
});

const LEGACY_TEST_STDOUT_PREFIX: &str = "[stdout - legacy_test]";
const GTEST_FORMAT_NAME: &str = "GoogleTest";

fn sanitize_line<'a>(raw_line: &'a str) -> Cow<'a, str> {
    let after_ansi = ANSI_ESCAPE_PATTERN.replace_all(raw_line, "");
    let Some(m) = LOG_PREFIX_PATTERN.find(&after_ansi) else {
        return after_ansi;
    };
    let strip_len = m.end();
    match after_ansi {
        Cow::Borrowed(s) => Cow::Borrowed(&s[strip_len..]),
        Cow::Owned(mut s) => {
            s.drain(..strip_len);
            Cow::Owned(s)
        }
    }
}

/// Parses stdout from a GoogleTest program (or serial stream) and returns structured results.
pub fn parse(stdout: &[u8]) -> Vec<TestCaseResult> {
    parse_str(&String::from_utf8_lossy(stdout))
}

/// Parses stdout string from a GoogleTest program and returns structured results.
pub fn parse_str(stdout: &str) -> Vec<TestCaseResult> {
    let mut results = Vec::new();
    let mut current_suite = String::new();
    let mut current_case = String::new();
    let mut output_for_case = Vec::new();

    let flush_unfinished_test = |results: &mut Vec<TestCaseResult>,
                                 suite: &mut String,
                                 case: &mut String,
                                 output: &mut Vec<String>| {
        if !case.is_empty() {
            let display_name = format!("{suite}.{case}");
            let failure_reason = if !output.is_empty() {
                FailureReason::from_message(&output.join("\n"))
            } else {
                None
            };
            results.push(TestCaseResult {
                display_name,
                suite_name: std::mem::take(suite),
                case_name: std::mem::take(case),
                status: TestStatus::Fail,
                duration: Duration::ZERO,
                format: GTEST_FORMAT_NAME.to_string(),
                failure_reason,
                output_files: None,
                output_dir: None,
            });
            output.clear();
        }
    };

    for raw_line in stdout.lines() {
        let line = sanitize_line(raw_line);
        let trimmed_line = line.trim();

        if trimmed_line.starts_with('[') {
            if let Some(captures) = GTEST_RUN_PATTERN.captures(trimmed_line) {
                flush_unfinished_test(
                    &mut results,
                    &mut current_suite,
                    &mut current_case,
                    &mut output_for_case,
                );
                current_suite = captures[1].to_string();
                current_case = captures[2].to_string();
                output_for_case.clear();
                continue;
            }

            if let Some(captures) = GTEST_COMPLETION_PATTERN.captures(trimmed_line) {
                let duration_match = captures.get(4);
                let has_duration = duration_match.is_some();
                let is_disabled = &captures[1] == "DISABLED";

                if current_case.is_empty() && !has_duration && !is_disabled {
                    continue;
                }

                if is_disabled {
                    flush_unfinished_test(
                        &mut results,
                        &mut current_suite,
                        &mut current_case,
                        &mut output_for_case,
                    );
                }

                let status = match &captures[1] {
                    "OK" => TestStatus::Pass,
                    "FAILED" => TestStatus::Fail,
                    "SKIPPED" | "DISABLED" => TestStatus::Skip,
                    _ => TestStatus::Fail,
                };

                let suite_name = captures[2].to_string();
                let case_name = captures[3].to_string();
                let display_name = format!("{suite_name}.{case_name}");

                let duration = duration_match
                    .and_then(|m| m.as_str().parse::<u64>().ok())
                    .map(Duration::from_millis)
                    .unwrap_or(Duration::ZERO);

                let failure_reason = if status.is_failure() && !output_for_case.is_empty() {
                    FailureReason::from_message(&output_for_case.join("\n"))
                } else {
                    None
                };

                results.push(TestCaseResult {
                    display_name,
                    suite_name,
                    case_name,
                    status,
                    duration,
                    format: GTEST_FORMAT_NAME.to_string(),
                    failure_reason,
                    output_files: None,
                    output_dir: None,
                });

                current_suite.clear();
                current_case.clear();
                output_for_case.clear();
                continue;
            }
        }

        if !current_case.is_empty() && !trimmed_line.starts_with(LEGACY_TEST_STDOUT_PREFIX) {
            output_for_case.push(line.into_owned());
        }
    }

    flush_unfinished_test(
        &mut results,
        &mut current_suite,
        &mut current_case,
        &mut output_for_case,
    );

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_parsed_cases(stdout: &str, expected: Vec<TestCaseResult>) {
        let results = parse(stdout.as_bytes());
        assert_eq!(results, expected);
    }

    #[test]
    fn test_parse_empty() {
        assert_parsed_cases("", vec![]);
    }

    #[test]
    fn test_parse_no_test_cases() {
        assert_parsed_cases("non-test output without any markers", vec![]);
    }

    #[test]
    fn test_parse_google_test() {
        let stdout = r#"
Sometimes there is weird stuff in stdout.
[==========] Running 4 tests from 1 test suite.
[----------] Global test environment set-up.
[----------] 4 tests from ChannelTest
[ RUN      ] ChannelTest.ReadWrite
[       OK ] ChannelTest.ReadWrite (4 ms)
[ RUN      ] ChannelTest.InvalidHandle
../../src/zircon/tests/channel_test.cc:45: Failure
Expected equality of these values:
  status
    Which is: ZX_ERR_BAD_HANDLE (-11)
  ZX_OK
    Which is: 0
[  FAILED  ] ChannelTest.InvalidHandle (5 ms)
[ RUN      ] ChannelTest.InterleavedLogs
Sometimes tests print to stdout.
Their prints get interleaved with the results.
[       OK ] ChannelTest.InterleavedLogs (3 ms)
[ RUN      ] ChannelTest.SkipFeature
[  SKIPPED ] ChannelTest.SkipFeature (1 ms)
[----------] 4 tests from ChannelTest (13 ms total)
[----------] Global test environment tear-down
[==========] 4 tests from 1 test suite ran. (15 ms total)
[  PASSED  ] 2 tests.
[  SKIPPED ] 1 test.
[  FAILED  ] 1 test.
"#;

        let expected = vec![
            TestCaseResult {
                display_name: "ChannelTest.ReadWrite".to_string(),
                suite_name: "ChannelTest".to_string(),
                case_name: "ReadWrite".to_string(),
                status: TestStatus::Pass,
                duration: Duration::from_millis(4),
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
            TestCaseResult {
                display_name: "ChannelTest.InvalidHandle".to_string(),
                suite_name: "ChannelTest".to_string(),
                case_name: "InvalidHandle".to_string(),
                status: TestStatus::Fail,
                duration: Duration::from_millis(5),
                format: "GoogleTest".to_string(),
                failure_reason: FailureReason::from_message(
                    "../../src/zircon/tests/channel_test.cc:45: Failure\nExpected equality of these values:\n  status\n    Which is: ZX_ERR_BAD_HANDLE (-11)\n  ZX_OK\n    Which is: 0",
                ),
                output_files: None,
                output_dir: None,
            },
            TestCaseResult {
                display_name: "ChannelTest.InterleavedLogs".to_string(),
                suite_name: "ChannelTest".to_string(),
                case_name: "InterleavedLogs".to_string(),
                status: TestStatus::Pass,
                duration: Duration::from_millis(3),
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
            TestCaseResult {
                display_name: "ChannelTest.SkipFeature".to_string(),
                suite_name: "ChannelTest".to_string(),
                case_name: "SkipFeature".to_string(),
                status: TestStatus::Skip,
                duration: Duration::from_millis(1),
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
        ];

        assert_parsed_cases(stdout, expected);
    }

    #[test]
    fn test_parse_serial_google_test() {
        let stdout = "\n[00012.100] [==========] Running 2 tests from 1 test suite.\n\
[00012.105] [----------] Global test environment set-up.\n\
[00012.110] [----------] 2 tests from SerialTest\n\
[00012.120] \x1b[0;32m[ RUN      ]\x1b[m SerialTest.PassTest\n\
[00012.125] \x1b[0;32m[       OK ]\x1b[m SerialTest.PassTest (5 ms)\n\
[00012.130] \x1b[0;31m[ RUN      ]\x1b[m SerialTest.FailTest\n\
[00012.135] assertion failed: expected 42 but got 0\n\
[00012.140] \x1b[0;31m[  FAILED  ]\x1b[m SerialTest.FailTest (10 ms)\n\
[00012.145] [----------] 2 tests from SerialTest (15 ms total)\n\
[00012.150] [==========] 2 tests ran.\n";

        let expected = vec![
            TestCaseResult {
                display_name: "SerialTest.PassTest".to_string(),
                suite_name: "SerialTest".to_string(),
                case_name: "PassTest".to_string(),
                status: TestStatus::Pass,
                duration: Duration::from_millis(5),
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
            TestCaseResult {
                display_name: "SerialTest.FailTest".to_string(),
                suite_name: "SerialTest".to_string(),
                case_name: "FailTest".to_string(),
                status: TestStatus::Fail,
                duration: Duration::from_millis(10),
                format: "GoogleTest".to_string(),
                failure_reason: FailureReason::from_message(
                    "assertion failed: expected 42 but got 0",
                ),
                output_files: None,
                output_dir: None,
            },
        ];

        assert_parsed_cases(stdout, expected);
    }

    #[test]
    fn test_parse_disabled_google_test() {
        let stdout = r#"
[==========] Running 3 tests from 1 test suite.
[----------] 3 tests from DisabledSuite
[ DISABLED ] DisabledSuite.FeatureOff
[ RUN      ] DisabledSuite.FeatureActive
[       OK ] DisabledSuite.FeatureActive (8 ms)
[ DISABLED ] DisabledSuite.AnotherOff
[----------] 3 tests from DisabledSuite (8 ms total)
"#;

        let expected = vec![
            TestCaseResult {
                display_name: "DisabledSuite.FeatureOff".to_string(),
                suite_name: "DisabledSuite".to_string(),
                case_name: "FeatureOff".to_string(),
                status: TestStatus::Skip,
                duration: Duration::ZERO,
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
            TestCaseResult {
                display_name: "DisabledSuite.FeatureActive".to_string(),
                suite_name: "DisabledSuite".to_string(),
                case_name: "FeatureActive".to_string(),
                status: TestStatus::Pass,
                duration: Duration::from_millis(8),
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
            TestCaseResult {
                display_name: "DisabledSuite.AnotherOff".to_string(),
                suite_name: "DisabledSuite".to_string(),
                case_name: "AnotherOff".to_string(),
                status: TestStatus::Skip,
                duration: Duration::ZERO,
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
        ];

        assert_parsed_cases(stdout, expected);
    }

    #[test]
    fn test_parse_flexible_timestamps() {
        let stdout = r#"
[2026-08-28 03:47:19.123] [==========] Running 2 tests from 1 test suite.
[2026-08-28 03:47:19.124] [ RUN      ] TimeSuite.IsoTest
[2026-08-28 03:47:19.129] [       OK ] TimeSuite.IsoTest (5 ms)
[12:34:56.789] [ RUN      ] TimeSuite.ClockTest
[12:34:56.790] [       OK ] TimeSuite.ClockTest (1 ms)
"#;

        let expected = vec![
            TestCaseResult {
                display_name: "TimeSuite.IsoTest".to_string(),
                suite_name: "TimeSuite".to_string(),
                case_name: "IsoTest".to_string(),
                status: TestStatus::Pass,
                duration: Duration::from_millis(5),
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
            TestCaseResult {
                display_name: "TimeSuite.ClockTest".to_string(),
                suite_name: "TimeSuite".to_string(),
                case_name: "ClockTest".to_string(),
                status: TestStatus::Pass,
                duration: Duration::from_millis(1),
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
        ];

        assert_parsed_cases(stdout, expected);
    }

    #[test]
    fn test_parse_parameterized_google_test() {
        let stdout = r#"
[==========] Running 2 tests from 1 test suite.
[----------] 2 tests from SocketSuite/ParamTest
[ RUN      ] SocketSuite/ParamTest.DataTransfer/0
[       OK ] SocketSuite/ParamTest.DataTransfer/0 (12 ms)
[ RUN      ] SocketSuite/ParamTest.DataTransfer/1
[       OK ] SocketSuite/ParamTest.DataTransfer/1 (11 ms)
[----------] 2 tests from SocketSuite/ParamTest (23 ms total)
"#;

        let expected = vec![
            TestCaseResult {
                display_name: "SocketSuite/ParamTest.DataTransfer/0".to_string(),
                suite_name: "SocketSuite/ParamTest".to_string(),
                case_name: "DataTransfer/0".to_string(),
                status: TestStatus::Pass,
                duration: Duration::from_millis(12),
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
            TestCaseResult {
                display_name: "SocketSuite/ParamTest.DataTransfer/1".to_string(),
                suite_name: "SocketSuite/ParamTest".to_string(),
                case_name: "DataTransfer/1".to_string(),
                status: TestStatus::Pass,
                duration: Duration::from_millis(11),
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
        ];

        assert_parsed_cases(stdout, expected);
    }

    #[test]
    fn test_parse_crash_google_test() {
        let stdout = r#"
[==========] Running 2 tests from 1 test suite.
[----------] 2 tests from CrashSuite
[ RUN      ] CrashSuite.TestPass
[       OK ] CrashSuite.TestPass (2 ms)
[ RUN      ] CrashSuite.TestCrash
kernel panic: segmentation fault at 0xdeadbeef
{{{bt:0:0x123456}}}
"#;

        let expected = vec![
            TestCaseResult {
                display_name: "CrashSuite.TestPass".to_string(),
                suite_name: "CrashSuite".to_string(),
                case_name: "TestPass".to_string(),
                status: TestStatus::Pass,
                duration: Duration::from_millis(2),
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
            TestCaseResult {
                display_name: "CrashSuite.TestCrash".to_string(),
                suite_name: "CrashSuite".to_string(),
                case_name: "TestCrash".to_string(),
                status: TestStatus::Fail,
                duration: Duration::ZERO,
                format: "GoogleTest".to_string(),
                failure_reason: FailureReason::from_message(
                    "kernel panic: segmentation fault at 0xdeadbeef\n{{{bt:0:0x123456}}}",
                ),
                output_files: None,
                output_dir: None,
            },
        ];

        assert_parsed_cases(stdout, expected);
    }

    #[test]
    fn test_parse_bracketed_non_timestamp_logs() {
        let stdout = r#"
[==========] Running 1 test from 1 test suite.
[----------] 1 test from BracketSuite
[ RUN      ] BracketSuite.BracketTest
[0] array element value
[1/5] progress step
[42] answer
[       OK ] BracketSuite.BracketTest (3 ms)
[----------] 1 test from BracketSuite (3 ms total)
"#;

        let expected = vec![TestCaseResult {
            display_name: "BracketSuite.BracketTest".to_string(),
            suite_name: "BracketSuite".to_string(),
            case_name: "BracketTest".to_string(),
            status: TestStatus::Pass,
            duration: Duration::from_millis(3),
            format: "GoogleTest".to_string(),
            failure_reason: None,
            output_files: None,
            output_dir: None,
        }];

        assert_parsed_cases(stdout, expected);
    }

    #[test]
    fn test_parse_botanist_and_zircon_debuglog_prefixes() {
        let stdout = "\
2026-11-16 23:40:03.634000 [00000.078] 01166:01169> [==========] Running 4 tests from 1 test suite.
2026-11-16 23:40:03.634100 [00000.078] 01166:01169> [----------] 4 tests from CoreTest
2026-11-16 23:40:03.634259 [00000.078] 01166:01169> [ RUN      ] CoreTest.PassWithDebuglog
2026-11-16 23:40:03.637696 [00000.078] 01166:01169> [       OK ] CoreTest.PassWithDebuglog (0 ms)
2026-11-16 23:40:03.640000 [00012.345] [ RUN      ] CoreTest.StackedBotanistAndMonotonic
2026-11-16 23:40:03.645000 [00012.350] [       OK ] CoreTest.StackedBotanistAndMonotonic (5 ms)
2026-11-16 23:40:03.650000 [ RUN      ] CoreTest.BotanistOnly
2026-11-16 23:40:03.652000 [  SKIPPED ] CoreTest.BotanistOnly  (2 ms)
2026-11-16 23:40:42.056081 [00039.393] 01166:493182> \x1b[0;31m[ RUN      ]\x1b[m CoreTest.FailPreservesIndentation
2026-11-16 23:40:42.056100 [00039.393] 01166:493182> Expected equality of these values:
2026-11-16 23:40:42.056110 [00039.393] 01166:493182>   actual
2026-11-16 23:40:42.056120 [00039.393] 01166:493182>     Which is: 1
2026-11-16 23:40:42.056130 [00039.393] 01166:493182> \x1b[0;31m[  FAILED  ]\x1b[m CoreTest.FailPreservesIndentation (3 ms)
2026-11-16 23:40:42.056200 [00039.394] 01166:493182> [----------] 4 tests from CoreTest (10 ms total)
";

        let expected = vec![
            TestCaseResult {
                display_name: "CoreTest.PassWithDebuglog".to_string(),
                suite_name: "CoreTest".to_string(),
                case_name: "PassWithDebuglog".to_string(),
                status: TestStatus::Pass,
                duration: Duration::from_millis(0),
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
            TestCaseResult {
                display_name: "CoreTest.StackedBotanistAndMonotonic".to_string(),
                suite_name: "CoreTest".to_string(),
                case_name: "StackedBotanistAndMonotonic".to_string(),
                status: TestStatus::Pass,
                duration: Duration::from_millis(5),
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
            TestCaseResult {
                display_name: "CoreTest.BotanistOnly".to_string(),
                suite_name: "CoreTest".to_string(),
                case_name: "BotanistOnly".to_string(),
                status: TestStatus::Skip,
                duration: Duration::from_millis(2),
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
            TestCaseResult {
                display_name: "CoreTest.FailPreservesIndentation".to_string(),
                suite_name: "CoreTest".to_string(),
                case_name: "FailPreservesIndentation".to_string(),
                status: TestStatus::Fail,
                duration: Duration::from_millis(3),
                format: "GoogleTest".to_string(),
                failure_reason: FailureReason::from_message(
                    "Expected equality of these values:\n  actual\n    Which is: 1",
                ),
                output_files: None,
                output_dir: None,
            },
        ];

        assert_parsed_cases(stdout, expected);
    }

    #[test]
    fn test_parse_unbracketed_and_zircon_standalone_prefixes() {
        let stdout = r#"
[00000.078] 01166:01169> [ RUN      ] PrefixSuite.ZirconDebuglogOnly
[00000.080] 01166:01169> [       OK ] PrefixSuite.ZirconDebuglogOnly (2 ms)
2026/11/16 15:49:39.711011 [ RUN      ] PrefixSuite.GoLoggerFormat
2026/11/16 15:49:39.714011 [       OK ] PrefixSuite.GoLoggerFormat (3 ms)
2026-11-16T23:40:03.123456Z [00012.345] [ RUN      ] PrefixSuite.Rfc3339Utc
2026-11-16T23:40:03.127456Z [00012.349] [       OK ] PrefixSuite.Rfc3339Utc (4 ms)
2026-11-16T23:40:03-08:00 [ RUN      ] PrefixSuite.Rfc3339Offset
2026-11-16T23:40:03-08:00 [       OK ] PrefixSuite.Rfc3339Offset (1 ms)
"#;

        let expected = vec![
            TestCaseResult {
                display_name: "PrefixSuite.ZirconDebuglogOnly".to_string(),
                suite_name: "PrefixSuite".to_string(),
                case_name: "ZirconDebuglogOnly".to_string(),
                status: TestStatus::Pass,
                duration: Duration::from_millis(2),
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
            TestCaseResult {
                display_name: "PrefixSuite.GoLoggerFormat".to_string(),
                suite_name: "PrefixSuite".to_string(),
                case_name: "GoLoggerFormat".to_string(),
                status: TestStatus::Pass,
                duration: Duration::from_millis(3),
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
            TestCaseResult {
                display_name: "PrefixSuite.Rfc3339Utc".to_string(),
                suite_name: "PrefixSuite".to_string(),
                case_name: "Rfc3339Utc".to_string(),
                status: TestStatus::Pass,
                duration: Duration::from_millis(4),
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
            TestCaseResult {
                display_name: "PrefixSuite.Rfc3339Offset".to_string(),
                suite_name: "PrefixSuite".to_string(),
                case_name: "Rfc3339Offset".to_string(),
                status: TestStatus::Pass,
                duration: Duration::from_millis(1),
                format: "GoogleTest".to_string(),
                failure_reason: None,
                output_files: None,
                output_dir: None,
            },
        ];

        assert_parsed_cases(stdout, expected);
    }

    #[test]
    fn test_sanitize_line_preserves_non_prefixes() {
        // Non-prefix tokens must not be stripped.
        assert_eq!(sanitize_line("12.345 bare float"), "12.345 bare float");
        assert_eq!(sanitize_line("12:34:56 bare clock"), "12:34:56 bare clock");
        assert_eq!(sanitize_line("2026-11-16 bare date"), "2026-11-16 bare date");
        assert_eq!(
            sanitize_line("01166:01169> standalone pid:tid"),
            "01166:01169> standalone pid:tid"
        );
        assert_eq!(
            sanitize_line("2026-11-16T23:40:03Z, expected 2026-11-16T23:40:04Z"),
            "2026-11-16T23:40:03Z, expected 2026-11-16T23:40:04Z"
        );
        assert_eq!(sanitize_line("[0] array element value"), "[0] array element value");
        assert_eq!(sanitize_line("[1/5] progress step"), "[1/5] progress step");
        assert_eq!(sanitize_line("[42] answer"), "[42] answer");
        assert_eq!(sanitize_line("[31:16] bitfield"), "[31:16] bitfield");

        // Bracketed tokens glued to punctuation or text must not be stripped.
        assert_eq!(sanitize_line("[1.5], expected"), "[1.5], expected");
        assert_eq!(sanitize_line("[12:34:56]: timeout"), "[12:34:56]: timeout");
        assert_eq!(sanitize_line("[12.345]ms"), "[12.345]ms");
        assert_eq!(
            sanitize_line("[2026-08-28T12:34:56Z], expected"),
            "[2026-08-28T12:34:56Z], expected"
        );
        assert_eq!(sanitize_line("2026-11-16 23:40:03.634259 [1.5], expected"), "[1.5], expected");

        // Payload tokens and indentation after a log header must be preserved.
        assert_eq!(
            sanitize_line(
                "2026-11-16 23:40:03.634259 [00000.078] 01166:01169> [1.5] expected != actual"
            ),
            "[1.5] expected != actual"
        );
        assert_eq!(
            sanitize_line("2026-11-16 23:40:03.634259 [00000.078] 01166:01169> [31:16] bitfield"),
            "[31:16] bitfield"
        );
        assert_eq!(
            sanitize_line("2026-11-16 23:40:03.634259 [00000.078] 01166:01169> [12:34] bitfield"),
            "[12:34] bitfield"
        );
        assert_eq!(
            sanitize_line(
                "2026-11-16 23:40:03.634259 [00000.078] 01166:01169>   2026-11-16T23:40:03Z == expected"
            ),
            "  2026-11-16T23:40:03Z == expected"
        );
        assert_eq!(
            sanitize_line("2026-11-16 23:40:03.634259   [1.5] expected != actual"),
            "  [1.5] expected != actual"
        );
        assert_eq!(
            sanitize_line("2026-11-16 23:40:03.634259   2026-11-16T23:40:03Z == expected"),
            "  2026-11-16T23:40:03Z == expected"
        );
        assert_eq!(
            sanitize_line("  2026-11-16T23:40:03Z == expected"),
            "  2026-11-16T23:40:03Z == expected"
        );

        // Prefix-only lines collapse to empty; indentation after a single delimiter space is kept.
        assert_eq!(sanitize_line("2026-11-16 23:40:03.634259   "), "");
        assert_eq!(sanitize_line("2026-11-16 23:40:03.634259 [00000.078] 01166:01169>   "), "");
        assert_eq!(
            sanitize_line("2026-11-16 23:40:03.634259 [00000.078] 01166:01169>   indented"),
            "  indented"
        );
    }
}
