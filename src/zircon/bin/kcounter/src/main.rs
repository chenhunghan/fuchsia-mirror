// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `kcounter` prints the values of the kernel's internal counters.

use counters_abi::Type;
use kcounter_lib::vmo::{
    ARENA_VMO_FILE, Arena, ArenaError, DESCRIPTOR_VMO_FILE, Descriptors, DescriptorsError,
};
use kcounter_lib::{kcounter_parse_cmdline, kcounter_usage};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::ExitCode;

const VMO_FILE_DIR: &str = "/boot/kernel";

/// Like `std::print!`, but ignores errors (e.g. a closed pipe) as `printf()`
/// does, rather than panicking.
macro_rules! print {
    ($($arg:tt)*) => {{
        let _ = write!(io::stdout(), $($arg)*);
    }};
}

/// Like `std::println!`, but ignores errors (e.g. a closed pipe) as `printf()`
/// does, rather than panicking.
macro_rules! println {
    ($($arg:tt)*) => {{
        let _ = writeln!(io::stdout(), $($arg)*);
    }};
}

/// Like `std::eprintln!`, but ignores errors as `fprintf(stderr, ...)` does,
/// rather than panicking.
macro_rules! eprintln {
    ($($arg:tt)*) => {{
        let _ = writeln!(io::stderr(), $($arg)*);
    }};
}

/// Returns the `strerror()` text for an I/O error, without the
/// " (os error N)" suffix that `io::Error`'s `Display` adds.
fn strerror(error: &io::Error) -> String {
    let text = error.to_string();
    match error.raw_os_error() {
        Some(code) => match text.strip_suffix(&format!(" (os error {code})")) {
            Some(text) => text.to_owned(),
            None => text,
        },
        None => text,
    }
}

/// Returns the `zx_status_get_string()` text for a status.
fn status_string(status: zx::Status) -> String {
    match status.as_str() {
        "UNKNOWN" => "(UNKNOWN)".to_owned(),
        name => format!("ZX_ERR_{name}"),
    }
}

/// Returns true if `name` should be shown: either no prefixes were given, or
/// it starts with one of `prefixes`, in which case the first such prefix is
/// recorded in `matched`.
fn matches(name: &[u8], prefixes: &[Vec<u8>], matched: &mut [bool]) -> bool {
    if prefixes.is_empty() {
        return true;
    }
    for (i, prefix) in prefixes.iter().enumerate() {
        if name.starts_with(prefix) {
            matched[i] = true;
            return true;
        }
    }
    false
}

/// Opens `/boot/kernel/<name>`, returning its exact VMO handle and VMO size,
/// or `None` after printing an error to `stderr`.
fn open_kernel_vmo(name: &str) -> Option<(zx::Vmo, u64)> {
    let file = match File::open(Path::new(VMO_FILE_DIR).join(name)) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("{VMO_FILE_DIR}/{name}: {}", strerror(&error));
            return None;
        }
    };
    let vmo = match fdio::get_vmo_exact_from_file(&file) {
        Ok(vmo) => vmo,
        Err(status) => {
            eprintln!("fdio_get_vmo_exact: {name}: {}", status_string(status));
            return None;
        }
    };
    let size = match vmo.get_size() {
        Ok(size) => size,
        Err(status) => {
            eprintln!("cannot get {name} VMO size: {}", status_string(status));
            return None;
        }
    };
    Some((vmo, size))
}

fn main() -> ExitCode {
    let mut argv: Vec<Vec<u8>> = std::env::args_os().map(OsStringExt::into_vec).collect();
    let Some(cmdline) = kcounter_parse_cmdline(&mut argv, Some(&mut io::stderr())) else {
        return ExitCode::from(1);
    };
    let argv0 = argv.first().map_or(&b""[..], Vec::as_slice);

    if cmdline.help {
        kcounter_usage(argv0, &mut io::stdout());
        return ExitCode::SUCCESS;
    }

    if cmdline.period != 0 {
        println!("watch mode every {} seconds", cmdline.period);
    }

    if let Err(error) =
        OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY).open(VMO_FILE_DIR)
    {
        eprintln!("{VMO_FILE_DIR}: {}", strerror(&error));
        return ExitCode::from(2);
    }

    let desc = {
        let Some((vmo, size)) = open_kernel_vmo(DESCRIPTOR_VMO_FILE) else {
            return ExitCode::from(2);
        };
        match Descriptors::from_vmo(&vmo, size) {
            Ok(desc) => desc,
            Err(DescriptorsError::Read(status)) => {
                eprintln!("cannot read {DESCRIPTOR_VMO_FILE} VMO: {}", status_string(status));
                return ExitCode::from(2);
            }
            Err(DescriptorsError::BadMagic { magic }) => {
                eprintln!(
                    "{DESCRIPTOR_VMO_FILE}: magic number {magic} != expected {}",
                    counters_abi::DescriptorVmo::MAGIC
                );
                return ExitCode::from(2);
            }
            Err(DescriptorsError::TooSmall { size, descriptor_table_size }) => {
                eprintln!(
                    "{DESCRIPTOR_VMO_FILE} size {size:#x} too small for \
                     {descriptor_table_size} bytes of descriptor table"
                );
                return ExitCode::from(2);
            }
        }
    };
    let max_cpus = desc.header().max_cpus;
    let num_counters = desc.num_counters();

    let arena = if !cmdline.list {
        let Some((vmo, size)) = open_kernel_vmo(ARENA_VMO_FILE) else {
            return ExitCode::from(2);
        };
        // SAFETY: `/boot/kernel/counters/arena` is the Zircon kernel counter
        // arena VMO: its 8-byte-aligned slots are updated concurrently by the
        // kernel exclusively via 64-bit atomic operations (without page-replacing
        // VMO range ops or cache-policy changes), and nothing in this process
        // unmaps, overwrites, or reprotects the mapping in `vmar_root_self()`
        // while `arena` is live.
        match unsafe { Arena::map(&vmo, size, &desc) } {
            Ok(arena) => Some(arena),
            Err(ArenaError::TooSmall) => {
                eprintln!(
                    "{ARENA_VMO_FILE} size {size:#x} too small for {max_cpus} CPUS * \
                     {num_counters} counters"
                );
                return ExitCode::from(2);
            }
            Err(ArenaError::Map(status)) => {
                eprintln!("cannot map {ARENA_VMO_FILE} VMO: {}", status_string(status));
                return ExitCode::from(2);
            }
        }
    } else {
        None
    };

    let unparsed_args = &argv[cmdline.unparsed_args_start..];
    let mut matched = vec![false; unparsed_args.len()];

    let mut times: usize = 1;
    let mut deadline = zx::MonotonicInstant::get().into_nanos();
    let mut match_failed = false;

    if let Some(cpuid) = cmdline.cpuid {
        // The command line parser should have already ensured that this value is
        // non-negative, which the u32 type of `cpuid` guarantees.

        // The command line parser should have made certain we did not select both
        // --cpuid and --verbose.
        debug_assert!(!cmdline.verbose);

        if u64::from(cpuid) >= max_cpus {
            eprintln!("CPU ID {cpuid} is out of range.  Descriptor reports max_cpus as {max_cpus}");
            return ExitCode::from(1);
        }
        println!("Dumping counters for CPU ID {cpuid}.");
    }

    if cmdline.period != 0 {
        println!("Dumping counters every {} seconds.  Press any key to stop.", cmdline.period);
    }

    let (cpu_range_start, cpu_range_end) = match cmdline.cpuid {
        Some(cpuid) => (u64::from(cpuid), u64::from(cpuid) + 1),
        None => (0, max_cpus),
    };

    let mut match_count: usize = 0;
    let mut max_name_length: usize = 0;
    for entry in desc.table() {
        if matches(entry.name_bytes(), unparsed_args, &mut matched) {
            max_name_length = max_name_length.max(entry.name_bytes().len());
            match_count += 1;
        }
    }
    let mut previous_values = vec![0i64; if cmdline.verbose { 0 } else { match_count }];

    // Set last sample time to zero so that the system and period averages match
    // on the first iteration.
    let mut last_sample_time: i64 = 0;

    // Names are printed left-justified in a field of `max_name_length` bytes.
    let name_field_width = max_name_length;

    loop {
        if cmdline.period != 0 {
            deadline += zx::MonotonicDuration::from_seconds(cmdline.period.into()).into_nanos();
            println!("[{times}]");
        }

        if !cmdline.terse && !cmdline.verbose && !cmdline.list {
            print!(
                "{:<name_field_width$}     {:<10}     {:<9}   {:<10}\n",
                "Counter", "Value", "Sys Avg", "Period Avg"
            );
        }

        let sample_time = zx::MonotonicInstant::get().into_nanos();
        let mut match_index: usize = 0;
        for (i, entry) in desc.table().iter().enumerate() {
            if matches(entry.name_bytes(), unparsed_args, &mut matched) {
                let entry_type = Type::try_from(entry.type_);
                if cmdline.list {
                    let _ = io::stdout().write_all(entry.name_bytes());
                    match entry_type {
                        Ok(Type::Sum) => println!(" sum"),
                        Ok(Type::Min) => println!(" min"),
                        Ok(Type::Max) => println!(" max"),
                        _ => eprintln!(" ??? unknown type {} ???", entry.type_),
                    }
                } else {
                    let arena = arena.as_ref().expect("arena is mapped unless --list");
                    if !cmdline.terse {
                        let _ = io::stdout().write_all(entry.name_bytes());
                        let padding = name_field_width.saturating_sub(entry.name_bytes().len());
                        let separator = if !cmdline.verbose {
                            " "
                        } else if entry_type == Ok(Type::Min) {
                            " min("
                        } else if entry_type == Ok(Type::Max) {
                            " max("
                        } else {
                            " "
                        };
                        print!("{:padding$} ={separator}", "");
                    }
                    let mut value: i64 = 0;
                    for cpu in cpu_range_start..cpu_range_end {
                        let cpu_value = arena.get(cpu, i);
                        if cmdline.verbose {
                            let separator = if cpu == 0 {
                                ""
                            } else if entry_type == Ok(Type::Sum) {
                                " + "
                            } else {
                                ", "
                            };
                            print!("{separator}{cpu_value}");
                        }
                        match entry_type {
                            Ok(Type::Min) => {
                                if cpu_value < value {
                                    value = cpu_value;
                                }
                            }
                            Ok(Type::Max) => {
                                if cpu_value > value {
                                    value = cpu_value;
                                }
                            }
                            _ => value = value.wrapping_add(cpu_value),
                        }
                    }
                    if cmdline.verbose {
                        let close = if entry_type == Ok(Type::Sum) { "" } else { ")" };
                        println!("{close} = {value}");
                    } else {
                        let (ev_per_nsec, overflow) = value.overflowing_mul(1_000_000_000);
                        let system_rate = ev_per_nsec.wrapping_div(sample_time);
                        let delta_value = value.wrapping_sub(previous_values[match_index]);
                        let delta_sample_time = sample_time - last_sample_time;
                        let period_rate =
                            delta_value.wrapping_mul(1_000_000_000).wrapping_div(delta_sample_time);

                        previous_values[match_index] = value;

                        if !overflow {
                            println!("{value:>12} [{system_rate:>8}/sec {period_rate:>8}/sec]");
                        } else {
                            println!("{value:>12} [{:>8}     {period_rate:>8}/sec]", "overflow");
                        }
                    }
                }

                match_index += 1;
            } // if matches(entry.name_bytes(), ...)
        } // for (i, entry) in desc.table().iter().enumerate()
        last_sample_time = sample_time;

        // Check that each prefix was actually used.
        if times == 1 {
            for (prefix, was_matched) in unparsed_args.iter().zip(&matched) {
                if !was_matched {
                    let _ =
                        io::stderr().write_all(&[&prefix[..], b": prefix not found\n"].concat());
                    match_failed = true;
                }
            }
        }

        if cmdline.period == 0 || match_failed {
            break;
        }

        let now = zx::MonotonicInstant::get().into_nanos();
        let timeout = deadline.saturating_sub(now);
        if timeout > 0 {
            let mut pfd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
            let msec = zx::MonotonicDuration::from_millis(1).into_nanos();
            let msec_timeout = i32::try_from((timeout + msec - 1) / msec).unwrap_or(i32::MAX);

            // SAFETY: `&mut pfd` points to 1 initialized, aligned, exclusively
            // borrowed `pollfd` valid for reads and writes during the call,
            // matching `nfds = 1`. `poll` does not retain the pointer after
            // returning or take ownership of `STDIN_FILENO`.
            let poll_result = unsafe { libc::poll(&mut pfd, 1, msec_timeout) };
            if poll_result > 0 {
                println!("Shutting down");
                break;
            }
        } else {
            // We are falling behind.   Reset our deadline to catch up
            deadline = now;
        }

        times += 1;
    } // loop

    if match_failed { ExitCode::from(1) } else { ExitCode::SUCCESS }
}
