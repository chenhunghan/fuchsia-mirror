//! Where converted code lands: which Rust files each C++ file's functions
//! went to. Each C++ file should become (roughly) one Rust file named after
//! it, so a reviewer can put the two side by side. A C++ file split across
//! Rust files, or a function moved into an unrelated file, makes that
//! harder and is reported as a `file-placement` lint.

use crate::analyze::{CppOrigin, Link, PairReport};
use crate::check::Severity;
use crate::lint::{Lint, LintKind};
use crate::normalize;

/// The Rust files one C++ file's functions went to.
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub cpp_path: String,
    pub targets: Vec<Target>,
}

/// One Rust file that received functions from a C++ file.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub rust_path: String,
    /// C++ function names with their Rust line.
    pub functions: Vec<(String, usize)>,
    /// Named like the C++ file (`foo.cc` to `foo.rs`, `dir_foo.rs`, or a
    /// `foo_*.rs` part), as opposed to an unrelated file.
    pub expected: bool,
}

/// A file name without directory, extension or `_ffi` suffix.
fn stem(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    let s = base.split('.').next().unwrap_or(base);
    normalize::ident(s.strip_suffix("_ffi").unwrap_or(s))
}

/// The directory names of a path, innermost first, without the ones that
/// only organize sources (`src`, `include`, `rust`).
fn dirs(path: &str) -> Vec<String> {
    let mut v: Vec<&str> = path.split('/').collect();
    v.pop();
    v.into_iter()
        .rev()
        .filter(|d| !matches!(*d, "src" | "include" | "rust" | "lib" | "cpp" | "c"))
        .map(normalize::ident)
        .collect()
}

/// Whether a Rust file is where a C++ file's code would be expected:
/// `dir/foo.cc` to `foo.rs`, `dir_foo.rs` (a flattened directory),
/// `foo/mod.rs` or `foo/lib.rs`, `x86_foo.rs`, or a part named `foo_*.rs`.
pub fn expected(cpp: &str, rust: &str) -> bool {
    let (cs, rs) = (test_subject(&stem(cpp)), stem(rust));
    if cs.is_empty() || rs.is_empty() {
        return true;
    }
    if rs == cs
        || rs.ends_with(&format!("_{cs}"))
        || rs.starts_with(&format!("{cs}_"))
        // A subclass folded into its base: `virtual_foo.cc` into `foo.rs`.
        || cs.ends_with(&format!("_{rs}"))
        || same_first_word(&cs, &rs)
    {
        return true;
    }
    let cpp_dirs = dirs(cpp);
    let parent = cpp_dirs.first();
    // A directory's main file: `motmot/power.cc` into `motmot/motmot.rs`.
    if parent == Some(&rs) {
        return true;
    }
    if matches!(rs.as_str(), "mod" | "lib" | "main") {
        // A crate or module named after the file (`foo/mod.rs`,
        // `topology/lib.rs` for `system-topology.cc`). A crate root that
        // collects several files is not.
        return dirs(rust).first().is_some_and(|d| {
            d == &cs || cs.starts_with(&format!("{d}_")) || cs.ends_with(&format!("_{d}"))
        });
    }
    let Some(parent) = parent else {
        return false;
    };
    // `hypervisor/vcpu.cc` to `hypervisor_vcpu.rs` or `hypervisor_vcpu_irq.rs`.
    let rs = rs
        .strip_prefix(&format!("{parent}_"))
        .unwrap_or(&rs)
        .to_string();
    rs == cs || rs.starts_with(&format!("{cs}_")) || same_first_word(&cs, &rs)
}

/// The file a C++ test file tests (`foo_tests` for `foo`): Rust keeps its
/// tests next to the code.
fn test_subject(stem: &str) -> String {
    for suffix in ["_unittests", "_unittest", "_tests", "_test"] {
        if let Some(s) = stem.strip_suffix(suffix) {
            return s.to_string();
        }
    }
    stem.to_string()
}

fn stem_is_ffi(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.split('.').next().is_some_and(|s| s.ends_with("_ffi"))
}

fn is_test_file(path: &str) -> bool {
    let s = stem(path);
    test_subject(&s) != s || s.starts_with("test_") || s == "tests"
}

/// Whether a Rust file is named for the object its functions handle:
/// `clock.rs` holds `sys_clock_*`, along with helpers that moved with them,
/// and `thread_dispatcher.rs` holds `ThreadDispatcher` methods.
/// A part named after the C++ file (`mp_ipi.rs` for `mp.cc`) is a split
/// by topic, not by object.
fn by_object(cpp_path: &str, t: &Target) -> bool {
    let file = normalize::words(&stem(&t.rust_path));
    let cpp = normalize::words(&test_subject(&stem(cpp_path)));
    !file.is_empty()
        && !cpp.iter().any(|w| file.contains(w))
        && t.functions.iter().any(|(name, _)| {
            // `ThreadDispatcher::GetExceptionReport` counts for its class.
            let words = normalize::words(&name.replace("::", "_"));
            file.iter().all(|w| words.contains(w))
        })
}

/// `exceptions_c` and `exceptions_pf`, or `vmx_cpu_state` and `vmx_cpu`:
/// parts of one file that was split, rather than an unrelated file.
fn same_first_word(a: &str, b: &str) -> bool {
    let first = |s: &str| s.split('_').next().unwrap_or("").to_string();
    let w = first(a);
    w.len() >= 3 && w == first(b)
}

/// A pairing by similarity below this score doesn't count as a move.
const SURE: f64 = 0.6;

/// Maps each C++ file to the Rust files its paired functions went to, and
/// reports splits and unexpected destinations.
pub fn placement(pairs: &[PairReport]) -> (Vec<Placement>, Vec<Lint>) {
    let mut out: Vec<Placement> = Vec::new();
    for p in pairs {
        // C++ the change didn't touch has its own place already; a second
        // copy of a Rust function says nothing about where the port went.
        if p.origin != CppOrigin::Changed || p.duplicate_of.is_some() {
            continue;
        }
        // FFI glue on either side (`cpp_*` shims, `rust_*` trampolines)
        // is not ported code, and a weak pairing by similarity is too
        // unsure to count as a move.
        let key = |b: &str| normalize::call(b).unwrap_or_else(|| normalize::ident(b));
        let glue = p.cpp.base.starts_with("cpp_") || p.rust.base.starts_with("rust_");
        let unsure =
            p.link == Link::Similarity && p.score < SURE && key(&p.cpp.base) != key(&p.rust.base);
        if glue || unsure {
            continue;
        }
        let cpp_path = &p.cpp.path;
        // `_ffi.cc` files hold the C++ side of FFI shims, not code to port.
        let ffi_cc = cpp_path
            .rsplit('/')
            .next()
            .is_some_and(|b| b.split('.').next().is_some_and(|s| s.ends_with("_ffi")));
        if stem(cpp_path).is_empty() || ffi_cc {
            continue;
        }
        let i = match out.iter().position(|x| &x.cpp_path == cpp_path) {
            Some(i) => i,
            None => {
                out.push(Placement {
                    cpp_path: cpp_path.clone(),
                    targets: Vec::new(),
                });
                out.len() - 1
            }
        };
        let targets = &mut out[i].targets;
        let t = match targets.iter().position(|t| t.rust_path == p.rust.path) {
            Some(t) => t,
            None => {
                targets.push(Target {
                    rust_path: p.rust.path.clone(),
                    functions: Vec::new(),
                    expected: expected(cpp_path, &p.rust.path),
                });
                targets.len() - 1
            }
        };
        targets[t]
            .functions
            .push((p.cpp.name.clone(), p.rust.start_line));
    }
    for pl in &mut out {
        pl.targets.sort_by(|a, b| {
            b.functions
                .len()
                .cmp(&a.functions.len())
                .then(a.rust_path.cmp(&b.rust_path))
        });
    }
    out.sort_by(|a, b| a.cpp_path.cmp(&b.cpp_path));

    let mut lints = Vec::new();
    let file = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
    let first_line = |cpp_path: &str, names: &[(String, usize)]| {
        pairs
            .iter()
            .filter(|p| p.cpp.path == cpp_path && names.iter().any(|(n, _)| *n == p.cpp.name))
            .map(|p| p.cpp.start_line)
            .min()
            .unwrap_or(1)
    };
    for pl in &out {
        // A companion `foo_ffi.rs` holds the shims for `foo.rs`; test files
        // go wherever the tests of the code they test go.
        // A file split by the object each function handles (`task.cc`'s
        // `sys_process_*` into `process.rs`) keeps each part easy to find.
        let homes: Vec<&Target> = pl
            .targets
            .iter()
            .filter(|t| !stem_is_ffi(&t.rust_path) && !by_object(&pl.cpp_path, t))
            .collect();
        if homes.len() >= 2 && !is_test_file(&pl.cpp_path) {
            let parts: Vec<String> = homes
                .iter()
                .map(|t| {
                    format!(
                        "{} ({}{})",
                        file(&t.rust_path),
                        t.functions.len(),
                        if t.expected {
                            ""
                        } else {
                            ", not named after it"
                        }
                    )
                })
                .collect();
            lints.push(Lint {
                kind: LintKind::FilePlacement,
                severity: Severity::Issue,
                path: pl.cpp_path.clone(),
                line: 1,
                message: format!(
                    "{} was split across {} Rust files: {}; convert each C++ file into one Rust file",
                    file(&pl.cpp_path),
                    homes.len(),
                    parts.join(", ")
                ),
                related: None,
            });
            continue;
        }
        // Tests may move next to the code they test.
        let severity = if is_test_file(&pl.cpp_path) {
            Severity::Note
        } else {
            Severity::Issue
        };
        for t in homes.iter().filter(|t| !t.expected) {
            let names: Vec<&str> = t.functions.iter().map(|(n, _)| n.as_str()).collect();
            let shown = if names.len() > 4 {
                format!("{}, and {} more", names[..4].join(", "), names.len() - 4)
            } else {
                names.join(", ")
            };
            lints.push(Lint {
                kind: LintKind::FilePlacement,
                severity,
                path: t.rust_path.clone(),
                line: t.functions.iter().map(|(_, l)| *l).min().unwrap_or(1),
                message: format!(
                    "{shown} from {} landed in {}, which isn't named after it",
                    file(&pl.cpp_path),
                    file(&t.rust_path)
                ),
                related: Some((pl.cpp_path.clone(), first_line(&pl.cpp_path, &t.functions))),
            });
        }
    }
    // A Rust file that collects several C++ files.
    let mut rust_files: Vec<(&str, Vec<&str>)> = Vec::new();
    for pl in out.iter().filter(|pl| !is_test_file(&pl.cpp_path)) {
        for t in &pl.targets {
            match rust_files.iter_mut().find(|(r, _)| *r == t.rust_path) {
                Some((_, v)) => v.push(&pl.cpp_path),
                None => rust_files.push((&t.rust_path, vec![&pl.cpp_path])),
            }
        }
    }
    for (rust, cpps) in rust_files {
        let mut stems: Vec<String> = cpps.iter().map(|c| stem(c)).collect();
        stems.sort();
        stems.dedup();
        if stems.len() >= 3 {
            let names: Vec<String> = cpps.iter().map(|c| file(c)).collect();
            lints.push(Lint {
                kind: LintKind::FilePlacement,
                severity: Severity::Note,
                path: rust.to_string(),
                line: 1,
                message: format!(
                    "{} collects code from {} C++ files: {}",
                    file(rust),
                    names.len(),
                    names.join(", ")
                ),
                related: None,
            });
        }
    }
    (out, lints)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_by_object_are_accepted() {
        let t = |path: &str, names: &[&str]| Target {
            rust_path: path.into(),
            functions: names.iter().map(|n| (n.to_string(), 1)).collect(),
            expected: false,
        };
        let clock = t("syscalls/clock.rs", &["sys_clock_create", "copy_out"]);
        assert!(by_object("syscalls/zircon.cc", &clock));
        let state = t("x86/debugger_state.rs", &["arch_get_single_step"]);
        assert!(!by_object("x86/debugger.cc", &state));
        let ipi = t("x86/mp_ipi.rs", &["arch_mp_send_ipi"]);
        assert!(!by_object("x86/mp.cc", &ipi));
    }

    #[test]
    fn expected_destinations() {
        assert!(expected("kernel/object/foo.cc", "kernel/object/foo.rs"));
        assert!(expected(
            "kernel/object/foo.cc",
            "kernel/object/rust/src/foo.rs"
        ));
        assert!(expected(
            "kernel/object/foo_dispatcher.cc",
            "kernel/object/foo_dispatcher.rs"
        ));
        assert!(expected(
            "arch/x86/hypervisor/pv.cc",
            "arch/x86/src/hypervisor_pv.rs"
        ));
        assert!(expected("arch/x86/mmu.cc", "arch/x86/src/mmu_tlb.rs"));
        assert!(expected(
            "arch/x86/feature.cc",
            "arch/x86/src/x86_feature.rs"
        ));
        assert!(expected(
            "lib/counters/counters.cc",
            "lib/counters/src/lib.rs"
        ));
        assert!(expected(
            "kernel/foo/include/foo/bar.h",
            "kernel/foo/bar.rs"
        ));
        assert!(!expected(
            "arch/x86/feature.cc",
            "arch/x86/src/timer_freq.rs"
        ));
        assert!(!expected(
            "arch/x86/registers.cc",
            "arch/x86/src/debugger_state.rs"
        ));
        assert!(!expected("arch/x86/restricted.cc", "arch/x86/src/mod.rs"));
        assert!(!expected(
            "arch/x86/hypervisor/vcpu.cc",
            "arch/x86/src/hypervisor_vmcs.rs"
        ));
        assert!(expected(
            "arch/x86/exceptions_c.cc",
            "arch/x86/src/exceptions_pf.rs"
        ));
        assert!(expected(
            "object/interrupts_test.cc",
            "object/interrupts.rs"
        ));
        assert!(expected(
            "object/virtual_interrupt_dispatcher.cc",
            "object/interrupt_dispatcher.rs"
        ));
        assert!(expected(
            "lib/topology/system-topology.cc",
            "lib/topology/src/mod.rs"
        ));
        assert!(expected(
            "dev/power/motmot/power.cc",
            "dev/power/motmot/motmot.rs"
        ));
        assert!(expected(
            "arch/x86/hypervisor/vmx_cpu_state.cc",
            "arch/x86/src/hypervisor_vmx_cpu.rs"
        ));
    }
}
