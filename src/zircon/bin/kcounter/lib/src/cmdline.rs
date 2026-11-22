// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Command line parsing for `kcounter`.
//!
//! Supports `kcounter`'s short and long flags (`-h`/`--help`, `-l`/`--list`,
//! `-t`/`--terse`, `-v`/`--verbose`, `-w`/`--watch[=period]`, and
//! `-c`/`--cpuid[=]id`), unambiguous long-option prefixes, `--` option
//! termination, and interleaved positional `PREFIX` arguments (which are moved
//! to the tail of `argv` starting at [`KcounterCmdline::unparsed_args_start`]).
//!
//! Arguments are handled as raw bytes, as in C, so they need not be UTF-8.

use std::io::{self, Write};

/// Default `--watch` period in seconds.
const DEFAULT_PERIOD: u32 = 3;

/// Recognized `kcounter` command-line flags.
#[derive(Clone, Copy)]
enum Flag {
    Help,
    List,
    Terse,
    Verbose,
    Watch,
    CpuId,
}

/// Long option names and their corresponding [`Flag`] variants.
const LONG_FLAGS: [(&str, Flag); 6] = [
    ("help", Flag::Help),
    ("list", Flag::List),
    ("terse", Flag::Terse),
    ("verbose", Flag::Verbose),
    ("watch", Flag::Watch),
    ("cpuid", Flag::CpuId),
];

/// The parsed `kcounter` command line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KcounterCmdline {
    /// `--help`: display the usage text and exit.
    pub help: bool,
    /// `--list`: show names and types rather than values.
    pub list: bool,
    /// `--terse`: show only values and no names.
    pub terse: bool,
    /// `--verbose`: show the per-CPU values.
    pub verbose: bool,
    /// Watch period in seconds, or 0 if not in watch mode.
    pub period: u32,
    /// Index into the (permuted) `argv` of the first non-option argument.
    pub unparsed_args_start: usize,
    /// The CPU chosen with `--cpuid`, or `None` if no CPU ID was chosen.
    pub cpuid: Option<u32>,
}

/// Prints program usage to `into`, using `myname` for the application name.
pub fn kcounter_usage(myname: &[u8], into: &mut dyn Write) {
    let _ = into.write_all(b"Usage: ");
    let _ = into.write_all(myname);
    let _ = write!(
        into,
        " [-hltvw] [--help] [--list] [--terse] [--verbose] [--watch[=period]] [PREFIX...]
Prints one counter per line.
With --help or -h, display this help and exit.
With --list or -l, show names and types rather than values.
With --terse or -t, show only values and no names.
With --verbose or -v, show space-separated lists of per-CPU values.
With --watch or -w, keep showing the values every [period] seconds, default is {DEFAULT_PERIOD} \
seconds.
With --cpuid or -c, show only values for the chosen CPU ID.
Otherwise values are aggregated summaries across all CPUs.
If PREFIX arguments are given, only matching names are shown.
Results are always sorted by name.
"
    );
}

/// Writes `parts` to `err` in order, ignoring errors.
fn write_parts(err: &mut dyn Write, parts: &[&[u8]]) {
    for part in parts {
        let _ = err.write_all(part);
    }
}

/// Sets `--cpuid` from `raw`, writing an error to `err` on negative values.
fn set_cpuid(cmdline: &mut KcounterCmdline, raw: &[u8], err: &mut dyn Write) -> bool {
    match u32::try_from(parse_int_prefix(raw)) {
        Ok(cpuid) => {
            cmdline.cpuid = Some(cpuid);
            true
        }
        Err(_) => {
            let _ = writeln!(err, "CPU ID must be non-negative");
            false
        }
    }
}

/// Sets `--watch` period from `raw` (or [`DEFAULT_PERIOD`] if `None`), writing
/// an error to `err` if `< 1`.
fn set_watch_period(
    cmdline: &mut KcounterCmdline,
    raw: Option<&[u8]>,
    err: &mut dyn Write,
) -> bool {
    // Default to every 3 seconds.
    let period = raw.map_or(DEFAULT_PERIOD as i32, parse_int_prefix);
    match u32::try_from(period) {
        Ok(period) if period >= 1 => {
            cmdline.period = period;
            true
        }
        _ => {
            let _ = writeln!(err, "watch period must be greater than 1");
            false
        }
    }
}

/// Resolves `name` against [`LONG_FLAGS`] by exact match or unique prefix.
fn resolve_long_flag(name: &[u8]) -> Result<(&'static str, Flag), bool> {
    let mut matched: Option<(&'static str, Flag)> = None;
    for &(spec_name, flag) in &LONG_FLAGS {
        if spec_name.as_bytes() == name {
            return Ok((spec_name, flag));
        }
        if spec_name.as_bytes().starts_with(name) {
            if matched.is_some() {
                return Err(true);
            }
            matched = Some((spec_name, flag));
        }
    }
    matched.ok_or(false)
}

/// Returns the parsed command line if successful, or `None` otherwise with an
/// error printed to `err` (or discarded if `err` is `None`).
///
/// Interleaved non-option arguments are stably moved to the end of `argv`,
/// starting at the returned [`KcounterCmdline::unparsed_args_start`].
pub fn kcounter_parse_cmdline<S: AsRef<[u8]>>(
    argv: &mut [S],
    err: Option<&mut dyn Write>,
) -> Option<KcounterCmdline> {
    let mut sink = io::sink();
    let err: &mut dyn Write = match err {
        Some(err) => err,
        None => &mut sink,
    };

    let argv0 = argv.first().map_or(&b""[..], |s| s.as_ref());
    let mut cmdline = KcounterCmdline::default();
    let mut is_positional = vec![false; argv.len()];

    let mut idx = 1;
    while idx < argv.len() {
        let arg = argv[idx].as_ref();
        if arg == b"--" {
            idx += 1;
            is_positional[idx..].fill(true);
            break;
        }
        if let Some(long_body) = arg.strip_prefix(b"--") {
            let (name, attached_val) = match long_body.iter().position(|&b| b == b'=') {
                Some(eq) => (&long_body[..eq], Some(&long_body[eq + 1..])),
                None => (long_body, None),
            };
            let (spec_name, flag) = match resolve_long_flag(name) {
                Ok(found) => found,
                Err(true) => {
                    write_parts(err, &[argv0, b": option is ambiguous: ", long_body, b"\n"]);
                    return None;
                }
                Err(false) => {
                    write_parts(err, &[argv0, b": unrecognized option: ", long_body, b"\n"]);
                    return None;
                }
            };
            idx += 1;
            match flag {
                Flag::Help | Flag::List | Flag::Terse | Flag::Verbose => {
                    if attached_val.is_some() {
                        write_parts(
                            err,
                            &[
                                argv0,
                                b": option does not take an argument: ",
                                spec_name.as_bytes(),
                                b"\n",
                            ],
                        );
                        return None;
                    }
                    match flag {
                        Flag::Help => cmdline.help = true,
                        Flag::List => cmdline.list = true,
                        Flag::Terse => cmdline.terse = true,
                        Flag::Verbose => cmdline.verbose = true,
                        Flag::Watch | Flag::CpuId => unreachable!(),
                    }
                }
                Flag::Watch => {
                    if !set_watch_period(&mut cmdline, attached_val, err) {
                        return None;
                    }
                }
                Flag::CpuId => {
                    let val = match attached_val {
                        Some(val) => val,
                        None => {
                            let Some(next_arg) = argv.get(idx) else {
                                write_parts(
                                    err,
                                    &[
                                        argv0,
                                        b": option requires an argument: ",
                                        spec_name.as_bytes(),
                                        b"\n",
                                    ],
                                );
                                return None;
                            };
                            idx += 1;
                            next_arg.as_ref()
                        }
                    };
                    if !set_cpuid(&mut cmdline, val, err) {
                        return None;
                    }
                }
            }
        } else if let Some(short_cluster) = arg.strip_prefix(b"-").filter(|s| !s.is_empty()) {
            idx += 1;
            let mut rem = short_cluster;
            while !rem.is_empty() {
                let (ch_bytes, rest) = rem.split_at(next_char_len(rem));
                rem = rest;
                match ch_bytes {
                    b"h" => cmdline.help = true,
                    b"l" => cmdline.list = true,
                    b"t" => cmdline.terse = true,
                    b"v" => cmdline.verbose = true,
                    b"w" => {
                        let attached = (!rem.is_empty()).then(|| std::mem::take(&mut rem));
                        if !set_watch_period(&mut cmdline, attached, err) {
                            return None;
                        }
                    }
                    b"c" => {
                        let val = if !rem.is_empty() {
                            std::mem::take(&mut rem)
                        } else {
                            let Some(next_arg) = argv.get(idx) else {
                                write_parts(err, &[argv0, b": option requires an argument: c\n"]);
                                return None;
                            };
                            idx += 1;
                            next_arg.as_ref()
                        };
                        if !set_cpuid(&mut cmdline, val, err) {
                            return None;
                        }
                    }
                    _ => {
                        write_parts(err, &[argv0, b": unrecognized option: ", ch_bytes, b"\n"]);
                        return None;
                    }
                }
            }
        } else {
            is_positional[idx] = true;
            idx += 1;
        }
    }

    if u8::from(cmdline.list) + u8::from(cmdline.terse) + u8::from(cmdline.verbose) > 1 {
        write_parts(err, &[argv0, b": --list, --terse, and --verbose are mutually exclusive\n"]);
        kcounter_usage(argv0, err);
        return None;
    }

    if cmdline.verbose && cmdline.cpuid.is_some() {
        write_parts(err, &[argv0, b": --verbose and --cpuid mutually exclusive\n"]);
        kcounter_usage(argv0, err);
        return None;
    }

    if cmdline.list && cmdline.period > 0 {
        write_parts(err, &[argv0, b": --list and --watch are mutually exclusive\n"]);
        kcounter_usage(argv0, err);
        return None;
    }

    let mut write_non_pos = 1;
    for i in 1..argv.len() {
        if !is_positional[i] {
            argv[write_non_pos..=i].rotate_right(1);
            write_non_pos += 1;
        }
    }
    cmdline.unparsed_args_start = if argv.is_empty() { 0 } else { write_non_pos };
    Some(cmdline)
}

/// Parses a leading signed decimal integer from `s` in the style of C `atoi`:
/// skips leading ASCII whitespace, accepts an optional `'+'` or `'-'`, parses
/// consecutive ASCII digits while ignoring trailing non-digit bytes, and
/// clamps overflow to `i32` bounds.
fn parse_int_prefix(s: &[u8]) -> i32 {
    let Some(start) =
        s.iter().position(|b| !matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
    else {
        return 0;
    };
    let s = &s[start..];
    let (neg, digits) = match s.first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut acc: i64 = 0;
    for &d in digits.iter().take_while(|d| d.is_ascii_digit()) {
        acc = acc.saturating_mul(10).saturating_add(i64::from(d - b'0'));
    }
    let signed = if neg { acc.saturating_neg() } else { acc };
    signed.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// Returns the byte length of the first UTF-8 scalar in `bytes` (which must not
/// be empty), or `1` if `bytes` begins with an invalid UTF-8 sequence.
fn next_char_len(bytes: &[u8]) -> usize {
    let prefix = &bytes[..bytes.len().min(4)];
    match std::str::from_utf8(prefix) {
        Ok(valid) => valid.chars().next().map_or(1, char::len_utf8),
        Err(error) if error.valid_up_to() > 0 => {
            std::str::from_utf8(&prefix[..error.valid_up_to()])
                .ok()
                .and_then(|s| s.chars().next())
                .map_or(1, char::len_utf8)
        }
        Err(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses `args`, returning the result, the permuted arguments, and the
    /// error output.
    fn parse(args: &[&str]) -> (Option<KcounterCmdline>, Vec<String>, String) {
        let mut argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let mut err = Vec::new();
        let result = kcounter_parse_cmdline(&mut argv, Some(&mut err));
        (result, argv, String::from_utf8(err).unwrap())
    }

    #[test]
    fn defaults() {
        let (cmdline, _, err) = parse(&["self.exe"]);
        assert_eq!(
            cmdline,
            Some(KcounterCmdline { unparsed_args_start: 1, ..KcounterCmdline::default() })
        );
        assert_eq!(err, "");
    }

    #[test]
    fn empty_argv() {
        let (cmdline, argv, err) = parse(&[]);
        assert_eq!(
            cmdline,
            Some(KcounterCmdline { unparsed_args_start: 0, ..KcounterCmdline::default() })
        );
        assert!(argv.is_empty());
        assert_eq!(err, "");

        let (cmdline, _, err) = parse(&[]);
        assert_eq!(&argv[cmdline.unwrap().unparsed_args_start..], &[] as &[String]);
        assert_eq!(err, "");
    }

    #[test]
    fn null_err_sink() {
        let mut argv = ["self.exe", "-l", "-t"];
        assert_eq!(kcounter_parse_cmdline(&mut argv, None), None);
    }

    #[test]
    fn long_options() {
        let (cmdline, _, _) =
            parse(&["self.exe", "--help", "--terse", "--watch=7", "--cpuid", "2"]);
        assert_eq!(
            cmdline,
            Some(KcounterCmdline {
                help: true,
                terse: true,
                period: 7,
                cpuid: Some(2),
                unparsed_args_start: 6,
                ..KcounterCmdline::default()
            })
        );
        let (cmdline, _, _) = parse(&["self.exe", "--cpuid=3", "--list"]);
        assert_eq!(cmdline.as_ref().map(|c| (c.cpuid, c.list)), Some((Some(3), true)));
    }

    #[test]
    fn abbreviated_long_options() {
        let (cmdline, _, _) = parse(&["self.exe", "--verb", "--w"]);
        assert_eq!(cmdline.as_ref().map(|c| (c.verbose, c.period)), Some((true, DEFAULT_PERIOD)));
        let (cmdline, _, _) = parse(&["self.exe", "--wat=9"]);
        assert_eq!(cmdline.map(|c| c.period), Some(9));
        let (cmdline, _, _) = parse(&["self.exe", "--cpu=4"]);
        assert_eq!(cmdline.map(|c| c.cpuid), Some(Some(4)));
    }

    #[test]
    fn short_options() {
        let (cmdline, _, _) = parse(&["self.exe", "-tc", "5", "-w10"]);
        assert_eq!(
            cmdline,
            Some(KcounterCmdline {
                terse: true,
                cpuid: Some(5),
                period: 10,
                unparsed_args_start: 4,
                ..KcounterCmdline::default()
            })
        );
        let (cmdline, _, _) = parse(&["self.exe", "-c6"]);
        assert_eq!(cmdline.map(|c| c.cpuid), Some(Some(6)));
        // An optional argument must be attached.
        let (cmdline, _, _) = parse(&["self.exe", "-w", "10"]);
        assert_eq!(cmdline.map(|c| (c.period, c.unparsed_args_start)), Some((DEFAULT_PERIOD, 2)));
    }

    #[test]
    fn permutation() {
        let (cmdline, argv, _) = parse(&["self.exe", "foo", "-t", "bar", "-c", "1", "baz"]);
        let cmdline = cmdline.unwrap();
        assert_eq!(cmdline.unparsed_args_start, 4);
        assert_eq!(cmdline.cpuid, Some(1));
        assert_eq!(argv, ["self.exe", "-t", "-c", "1", "foo", "bar", "baz"]);
    }

    #[test]
    fn double_dash_ends_options() {
        let (cmdline, argv, _) = parse(&["self.exe", "foo", "--", "-t"]);
        let cmdline = cmdline.unwrap();
        assert!(!cmdline.terse);
        assert_eq!(cmdline.unparsed_args_start, 2);
        assert_eq!(argv, ["self.exe", "--", "foo", "-t"]);
        let (cmdline, argv, _) = parse(&["self.exe", "-", "-t"]);
        assert_eq!(cmdline.map(|c| (c.terse, c.unparsed_args_start)), Some((true, 2)));
        assert_eq!(argv, ["self.exe", "-t", "-"]);
    }

    #[test]
    fn errors() {
        let (cmdline, _, err) = parse(&["self.exe", "-x"]);
        assert_eq!(cmdline, None);
        assert_eq!(err, "self.exe: unrecognized option: x\n");

        let (cmdline, _, err) = parse(&["self.exe", "--bogus"]);
        assert_eq!(cmdline, None);
        assert_eq!(err, "self.exe: unrecognized option: bogus\n");

        let (cmdline, _, err) = parse(&["self.exe", "-c"]);
        assert_eq!(cmdline, None);
        assert_eq!(err, "self.exe: option requires an argument: c\n");

        let (cmdline, _, err) = parse(&["self.exe", "--cpuid"]);
        assert_eq!(cmdline, None);
        assert_eq!(err, "self.exe: option requires an argument: cpuid\n");

        let (cmdline, _, err) = parse(&["self.exe", "--list=1"]);
        assert_eq!(cmdline, None);
        assert_eq!(err, "self.exe: option does not take an argument: list\n");

        let (cmdline, _, err) = parse(&["self.exe", "--=1"]);
        assert_eq!(cmdline, None);
        assert_eq!(err, "self.exe: option is ambiguous: =1\n");

        let (cmdline, _, err) = parse(&["self.exe", "-:"]);
        assert_eq!(cmdline, None);
        assert_eq!(err, "self.exe: unrecognized option: :\n");

        let (cmdline, _, err) = parse(&["self.exe", "-c", "-1"]);
        assert_eq!(cmdline, None);
        assert_eq!(err, "CPU ID must be non-negative\n");

        let (cmdline, _, err) = parse(&["self.exe", "-w0"]);
        assert_eq!(cmdline, None);
        assert_eq!(err, "watch period must be greater than 1\n");

        let (cmdline, _, err) = parse(&["self.exe", "-v", "-c", "0"]);
        assert_eq!(cmdline, None);
        assert!(err.starts_with("self.exe: --verbose and --cpuid mutually exclusive\nUsage: "));
    }

    #[test]
    fn non_utf8_arguments() {
        let mut argv: [&[u8]; 4] = [b"self.exe", b"-\xff", b"-t", b"pre\xfefix"];
        let mut err = Vec::new();
        assert_eq!(kcounter_parse_cmdline(&mut argv, Some(&mut err)), None);
        assert_eq!(err, b"self.exe: unrecognized option: \xff\n");

        let mut argv: [&[u8]; 3] = [b"self.exe", b"pre\xfefix", b"-t"];
        let cmdline = kcounter_parse_cmdline(&mut argv, None).unwrap();
        assert!(cmdline.terse);
        assert_eq!(cmdline.unparsed_args_start, 2);
        assert_eq!(argv, [&b"self.exe"[..], b"-t", b"pre\xfefix"]);
    }

    #[test]
    fn usage() {
        let mut out = Vec::new();
        kcounter_usage(b"self.exe", &mut out);
        let out = String::from_utf8(out).unwrap();
        assert!(out.starts_with("Usage: self.exe [-hltvw] [--help] "), "{out}");
        assert!(out.contains("default is 3 seconds.\n"), "{out}");
        assert!(out.ends_with("Results are always sorted by name.\n"), "{out}");
    }

    #[test]
    fn int_prefix_semantics() {
        assert_eq!(parse_int_prefix(b"12abc"), 12);
        assert_eq!(parse_int_prefix(b"  -7"), -7);
        assert_eq!(parse_int_prefix(b"+3"), 3);
        assert_eq!(parse_int_prefix(b"abc"), 0);
        assert_eq!(parse_int_prefix(b""), 0);
        assert_eq!(parse_int_prefix(b"-2147483648"), i32::MIN);
        assert_eq!(parse_int_prefix(b"999999999999"), i32::MAX);
        assert_eq!(parse_int_prefix(b"-999999999999"), i32::MIN);
    }
}
