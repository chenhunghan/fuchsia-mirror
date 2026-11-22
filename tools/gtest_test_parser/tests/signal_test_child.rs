// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::io::{self, Write};
use std::{env, process, thread};

use nix::unistd::close;
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

const READY_MARKER: &str = "READY_FOR_SIGNAL";
const SERIALLISTENER_TIMEOUT_LOG: &str = "seriallistener FATAL: timed out before success string";
const STDOUT_FD: i32 = 1;

fn write_stdout_and_flush(text: &str) {
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes()).expect("write stdout");
    stdout.flush().expect("flush stdout");
}

fn write_stderr_and_flush(text: &str) {
    let mut stderr = io::stderr().lock();
    stderr.write_all(text.as_bytes()).expect("write stderr");
    stderr.flush().expect("flush stderr");
}

fn main() {
    let mode = env::args().nth(1).expect("expected test child mode argument");

    match mode.as_str() {
        "trap-sigterm" => {
            // Register the self-pipe signal handler before printing READY_MARKER so
            // SIGTERM cannot race with blocking on `signals.forever()`.
            let mut signals = Signals::new([SIGTERM]).expect("register SIGTERM handler");
            write_stdout_and_flush(&format!(
                "[ RUN      ] BootSuite.PassCase\n\
                 [       OK ] BootSuite.PassCase (4 ms)\n\
                 [ RUN      ] BootSuite.HangingCase\n\
                 {READY_MARKER}\n"
            ));
            let _ = signals.forever().next();
            write_stderr_and_flush(&format!(
                "{SERIALLISTENER_TIMEOUT_LOG} \"***Boot-test-successful!***\" was read from serial\n"
            ));
            write_stdout_and_flush("post-sigterm detail\n");
            process::exit(1);
        }
        "trap-sigint" => {
            let mut signals = Signals::new([SIGINT]).expect("register SIGINT handler");
            write_stdout_and_flush(&format!(
                "[ RUN      ] IntSuite.Done\n\
                 [       OK ] IntSuite.Done (2 ms)\n\
                 [ RUN      ] IntSuite.Interrupted\n\
                 {READY_MARKER}\n"
            ));
            let _ = signals.forever().next();
            write_stderr_and_flush("child caught SIGINT\n");
            process::exit(2);
        }
        "default-sigterm" => {
            // Leave SIGTERM at its inherited disposition (expected SIG_DFL after exec).
            write_stdout_and_flush(&format!(
                "[ RUN      ] DefaultSuite.Pass\n\
                 [       OK ] DefaultSuite.Pass (7 ms)\n\
                 [ RUN      ] DefaultSuite.Running\n\
                 {READY_MARKER}\n"
            ));
            loop {
                thread::park();
            }
        }
        "ignore-sigterm" => {
            // Absorb SIGTERM without exiting so the supervisor must escalate to SIGKILL.
            let _signals = Signals::new([SIGTERM]).expect("register SIGTERM handler");
            write_stdout_and_flush(&format!(
                "[ RUN      ] StubbornSuite.Pass\n\
                 [       OK ] StubbornSuite.Pass (3 ms)\n\
                 [ RUN      ] StubbornSuite.Hung\n\
                 {READY_MARKER}\n"
            ));
            loop {
                thread::park();
            }
        }
        "trap-sigterm-exit-zero" => {
            let mut signals = Signals::new([SIGTERM]).expect("register SIGTERM handler");
            write_stdout_and_flush(&format!(
                "[ RUN      ] ZeroExitSuite.Case\n\
                 {READY_MARKER}\n"
            ));
            let _ = signals.forever().next();
            process::exit(0);
        }
        "closed-stdout-trap-sigterm" => {
            let mut signals = Signals::new([SIGTERM]).expect("register SIGTERM handler");
            write_stdout_and_flush(
                "[ RUN      ] ClosedStdoutSuite.Pass\n\
                 [       OK ] ClosedStdoutSuite.Pass (5 ms)\n\
                 [ RUN      ] ClosedStdoutSuite.Waiting\n",
            );
            // Close stdout before signaling readiness on stderr so gtest_test_parser
            // observes stdout EOF and enters SupervisedChild::wait().
            close(STDOUT_FD).expect("close stdout");
            write_stderr_and_flush(&format!("{READY_MARKER}\n"));
            let _ = signals.forever().next();
            write_stderr_and_flush(&format!("{SERIALLISTENER_TIMEOUT_LOG}\n"));
            process::exit(1);
        }
        "normal-pass" => {
            write_stdout_and_flush(
                "[ RUN      ] NormalSuite.Pass\n\
                 [       OK ] NormalSuite.Pass (1 ms)\n",
            );
        }
        "normal-fail" => {
            write_stdout_and_flush(
                "[ RUN      ] NormalSuite.Fail\n\
                 boom\n\
                 [  FAILED  ] NormalSuite.Fail (2 ms)\n",
            );
            process::exit(40);
        }
        other => panic!("unknown signal_test_child mode: {other}"),
    }
}
