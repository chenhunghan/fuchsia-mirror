//! Plain-text output, in the spirit of `diff -y`.

use crate::analyze::{CppOrigin, Link, PairReport, Report};
use crate::check::{Marker, Row};
use crate::model::{Function, Unit};
use std::fmt::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// C++ on the left, Rust on the right.
    SideBySide,
    /// C++ line(s), then the Rust line(s) they align with. No truncation;
    /// easier for tools and narrow terminals.
    Stacked,
}

#[derive(Clone, Debug)]
pub struct RenderOptions {
    pub layout: Layout,
    /// Total width for the side-by-side layout.
    pub width: usize,
    /// Show only the rows within this many rows of a marked row.
    pub context: Option<usize>,
    /// Omit the aligned source and print only the summaries and findings.
    pub summary_only: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        RenderOptions {
            layout: Layout::SideBySide,
            width: 160,
            context: None,
            summary_only: false,
        }
    }
}

/// Whether any C++ file went somewhere other than one Rust file named after
/// it, which is when the file mapping is worth showing.
pub fn placement_notable(report: &Report) -> bool {
    report
        .placement
        .iter()
        .any(|p| p.targets.len() > 1 || p.targets.iter().any(|t| !t.expected))
}

pub fn render(report: &Report, opts: &RenderOptions) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "babeldiff: {} pair{}, {} issue{}, {} note{}; {} C++ and {} Rust function{} unpaired{}",
        report.pairs.len(),
        plural(report.pairs.len()),
        report.issues(),
        plural(report.issues()),
        report.notes(),
        plural(report.notes()),
        report.unmatched_cpp.len(),
        report.unmatched_rust.len(),
        plural(report.unmatched_rust.len()),
        if report.rust_tests.is_empty() {
            String::new()
        } else {
            format!(
                ", plus {} Rust test{}",
                report.rust_tests.len(),
                plural(report.rust_tests.len())
            )
        },
    );
    let by_cat = crate::analyze::issues_by_category(report);
    if !by_cat.is_empty() {
        let parts: Vec<String> = by_cat
            .iter()
            .map(|(c, n)| format!("{} {n}", c.name()))
            .collect();
        let _ = writeln!(out, "issues by kind: {}", parts.join(", "));
    }
    if !report.lints.is_empty() {
        let mut kinds: Vec<(crate::lint::LintKind, usize)> = Vec::new();
        for l in &report.lints {
            match kinds.iter_mut().find(|(k, _)| *k == l.kind) {
                Some((_, n)) => *n += 1,
                None => kinds.push((l.kind, 1)),
            }
        }
        kinds.sort();
        let parts: Vec<String> = kinds
            .iter()
            .map(|(k, n)| format!("{} {n}", k.name()))
            .collect();
        let _ = writeln!(
            out,
            "rubric lints: {} issue{}, {} note{} ({}); see the end of the report",
            report.lint_issues(),
            plural(report.lint_issues()),
            report.lints.len() - report.lint_issues(),
            plural(report.lints.len() - report.lint_issues()),
            parts.join(", ")
        );
    }
    let _ = writeln!(
        out,
        "legend: = same  ~ note  ! issue  < only in C++  > only in Rust"
    );
    for p in &report.pairs {
        out.push('\n');
        render_pair(&mut out, p, opts);
    }
    if !report.unmatched_cpp.is_empty() || !report.unmatched_rust.is_empty() {
        out.push('\n');
        let _ = writeln!(out, "==== Unpaired functions");
        for f in &report.unmatched_cpp {
            let _ = writeln!(out, "  < C++  {}  {}", f.name, f.location());
        }
        for f in &report.unmatched_rust {
            let callers = report.callers(f);
            let helper = if callers.is_empty() {
                String::new()
            } else {
                format!("  (helper for {})", callers.join(", "))
            };
            let _ = writeln!(out, "  > Rust {}  {}{helper}", f.name, f.location());
        }
    }
    if !report.rust_tests.is_empty() {
        out.push('\n');
        let _ = writeln!(out, "==== Rust tests with no C++ counterpart");
        for f in &report.rust_tests {
            let _ = writeln!(out, "  > Rust {}  {}", f.name, f.location());
        }
    }
    if !report.removed_cpp_shims.is_empty() {
        out.push('\n');
        let _ = writeln!(out, "==== C++ FFI helpers for Rust, changed or removed");
        for f in &report.removed_cpp_shims {
            let _ = writeln!(out, "  < C++  {}  {}", f.name, f.location());
        }
    }
    if !report.rust_facades.is_empty() {
        out.push('\n');
        let _ = writeln!(out, "==== Rust that calls C++ through FFI");
        for (f, callee) in &report.rust_facades {
            let _ = writeln!(out, "  > Rust {} -> {}  {}", f.name, callee, f.location());
        }
    }
    if !report.cpp_changes.is_empty() {
        out.push('\n');
        let _ = writeln!(
            out,
            "==== C++ changed outside the port (stays C++; check by hand)"
        );
        for c in &report.cpp_changes {
            let span = if c.start_line == c.end_line {
                format!("{}:{}", c.path, c.start_line)
            } else {
                format!("{}:{}-{}", c.path, c.start_line, c.end_line)
            };
            let within = if c.functions.is_empty() {
                String::new()
            } else {
                format!("  in {}", c.functions.join(", "))
            };
            let _ = writeln!(out, "  {span}{within}  {}", c.text);
        }
    }
    if placement_notable(report) {
        out.push('\n');
        let _ = writeln!(out, "==== File placement");
        for pl in &report.placement {
            let parts: Vec<String> = pl
                .targets
                .iter()
                .map(|t| {
                    format!(
                        "{} ({}{})",
                        short(&t.rust_path),
                        t.functions.len(),
                        if t.expected { "" } else { ", unexpected" }
                    )
                })
                .collect();
            let _ = writeln!(out, "  {} -> {}", short(&pl.cpp_path), parts.join(", "));
        }
    }
    if !report.lints.is_empty() {
        out.push('\n');
        let _ = writeln!(out, "==== Rubric lints");
        for l in &report.lints {
            let related = l.related.as_ref().map_or(String::new(), |(p, n)| {
                format!("  (C++ at {}:{n})", short(p))
            });
            let _ = writeln!(
                out,
                "  {} {}:{}  [{}] {}{related}",
                l.severity.marker(),
                l.path,
                l.line,
                l.kind.name(),
                l.message
            );
        }
    }
    let shims: Vec<_> = report.shims.iter().collect();
    if !shims.is_empty() {
        out.push('\n');
        let _ = writeln!(out, "==== FFI shims");
        for s in shims {
            match &s.target {
                Some(t) => {
                    let _ = writeln!(out, "  {} -> {}  {}", s.shim.name, t, s.shim.location());
                }
                None if !s.ambiguous.is_empty() => {
                    let _ = writeln!(
                        out,
                        "  {} -> ambiguous: {}  {}  (pass --pair CPP=RUST to choose)",
                        s.shim.name,
                        s.ambiguous.join(" or "),
                        s.shim.location()
                    );
                }
                None => {
                    let _ = writeln!(out, "  {}  {}", s.shim.name, s.shim.location());
                }
            }
        }
    }
    out
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn render_pair(out: &mut String, p: &PairReport, opts: &RenderOptions) {
    let _ = writeln!(out, "==== {}  <->  {}", p.cpp.name, p.rust.name);
    let origin = match p.origin {
        CppOrigin::Changed => "",
        CppOrigin::Unchanged => "  (not changed by this diff)",
    };
    let _ = writeln!(out, "  C++   {}{}", p.cpp.location(), origin);
    let _ = writeln!(out, "  Rust  {}", p.rust.location());
    match &p.link {
        Link::Ffi {
            shim,
            shim_location,
        } => {
            let _ = writeln!(
                out,
                "  via   C++ now calls FFI shim {shim}  {shim_location}"
            );
        }
        Link::FfiName {
            shim,
            shim_location,
        } => {
            let _ = writeln!(out, "  via   FFI shim {shim}  {shim_location}");
        }
        Link::Forced => {
            let _ = writeln!(out, "  via   --pair");
        }
        Link::Similarity if !p.rationale.is_empty() => {
            let _ = writeln!(out, "  why   {}", p.rationale);
        }
        Link::Similarity => {}
    }
    for o in &p.overrides {
        let _ = writeln!(
            out,
            "  also  C++ {}  {}  (override in a related class, folded into this Rust function)",
            o.cpp.name,
            o.cpp.location()
        );
    }
    if let Some(f) = &p.forwarder {
        let _ = writeln!(
            out,
            "  via   Rust {} forwards here  {}",
            f.name,
            f.location()
        );
    }
    let ext: Vec<&String> = {
        let mut v: Vec<&String> = p.cpp.units.iter().filter_map(|u| u.file.as_ref()).collect();
        v.dedup();
        v
    };
    for f in ext {
        let _ = writeln!(
            out,
            "  note  C++ lines marked * are the declaration's comment in {f}"
        );
    }
    let _ = writeln!(
        out,
        "  similarity {:.2}, {} issue{}, {} note{}",
        p.score,
        p.issues(),
        plural(p.issues()),
        p.notes(),
        plural(p.notes())
    );

    if !opts.summary_only {
        out.push('\n');
        render_rows(out, &p.cpp, &p.rust, &p.rows, opts);
        for o in &p.overrides {
            let _ = writeln!(
                out,
                "\n  ---- override {}  <->  {}",
                o.cpp.name, p.rust.name
            );
            render_rows(out, &o.cpp, &p.rust, &o.rows, opts);
        }
    }

    out.push('\n');
    let s = &p.summary;
    let same = |a: &[String], b: &[String]| if a == b { "same" } else { "DIFFERENT" };
    let list = |v: &[String]| {
        if v.is_empty() {
            "none".to_string()
        } else {
            v.join(", ")
        }
    };
    let _ = writeln!(out, "  errors   {}", same(&s.cpp_errors, &s.rust_errors));
    if s.cpp_errors != s.rust_errors || !s.cpp_errors.is_empty() {
        let _ = writeln!(out, "           C++:  {}", list(&s.cpp_errors));
        let _ = writeln!(out, "           Rust: {}", list(&s.rust_errors));
    }
    let _ = writeln!(out, "  locks    {}", same(&s.cpp_locks, &s.rust_locks));
    if s.cpp_locks != s.rust_locks || !s.cpp_locks.is_empty() {
        let _ = writeln!(out, "           C++:  {}", list(&s.cpp_locks));
        let _ = writeln!(out, "           Rust: {}", list(&s.rust_locks));
    }
    let flow: Vec<String> = s
        .flow
        .iter()
        .map(|(k, a, b)| {
            if a == b {
                format!("{k} {a}")
            } else {
                format!("{k} {a}/{b}!")
            }
        })
        .collect();
    let differs = s.flow.iter().any(|(_, a, b)| a != b);
    let _ = writeln!(
        out,
        "  flow     {}{}",
        if flow.is_empty() {
            "none".into()
        } else {
            flow.join(", ")
        },
        if differs {
            "  (C++/Rust counts where they differ)"
        } else {
            ""
        }
    );
    let _ = writeln!(
        out,
        "  comments {} C++ comment{}: {} identical, {} reworded, {} missing in Rust",
        s.comments_total,
        plural(s.comments_total),
        s.comments_same,
        s.comments_changed,
        s.comments_total - s.comments_same - s.comments_changed
    );
    let mut findings: Vec<(&Function, &crate::check::Finding)> = p
        .findings
        .iter()
        .map(|f| (&p.cpp, f))
        .chain(
            p.overrides
                .iter()
                .flat_map(|o| o.findings.iter().map(move |f| (&o.cpp, f))),
        )
        .collect();
    findings.sort_by_key(|(_, f)| f.severity);
    if !findings.is_empty() {
        let _ = writeln!(out, "  findings");
        for (cf, f) in findings {
            let c = f.cpp_line.map_or("-".to_string(), |l| {
                let path = f.cpp_file.as_deref().unwrap_or(&cf.path);
                format!("{}:{}", short(path), l)
            });
            let r = f.rust_line.map_or("-".to_string(), |l| {
                format!("{}:{}", short(&p.rust.path), l)
            });
            let _ = writeln!(
                out,
                "    {} {} | {}  {}",
                f.severity.marker(),
                c,
                r,
                f.message
            );
        }
    }
}

fn short(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Lines of a unit to print, skipping those already printed on this side.
fn unit_lines(f: &Function, u: &Unit, last: &mut usize, indent: usize) -> Vec<(String, String)> {
    if u.file.is_some() {
        let ind = u.ext_lines.iter().map(|l| leading(l)).min().unwrap_or(0);
        return u
            .ext_lines
            .iter()
            .enumerate()
            .map(|(i, l)| (format!("{}*", u.start_line + i), cut(l, ind)))
            .collect();
    }
    let from = u.start_line.max(*last + 1);
    if from > u.end_line {
        // Already printed with an earlier unit on the same line.
        return Vec::new();
    }
    *last = u.end_line;
    (from..=u.end_line)
        .map(|l| (l.to_string(), cut(f.line(l), indent)))
        .collect()
}

fn leading(s: &str) -> usize {
    if s.trim().is_empty() {
        usize::MAX
    } else {
        s.len() - s.trim_start().len()
    }
}

fn cut(s: &str, n: usize) -> String {
    let k = leading(s).min(n);
    s[k.min(s.len())..].to_string()
}

fn signature_indent(f: &Function) -> usize {
    f.units
        .iter()
        .find(|u| u.file.is_none())
        .map(|u| leading(f.line(u.start_line)))
        .filter(|&n| n != usize::MAX)
        .unwrap_or(0)
}

fn render_rows(
    out: &mut String,
    cpp: &Function,
    rust: &Function,
    rows: &[Row],
    opts: &RenderOptions,
) {
    let (ci, ri) = (signature_indent(cpp), signature_indent(rust));
    let (mut last_c, mut last_r) = (0usize, 0usize);
    let visible: Vec<bool> = match opts.context {
        None => vec![true; rows.len()],
        Some(k) => {
            let marked: Vec<usize> = rows
                .iter()
                .enumerate()
                .filter(|(_, r)| r.marker != Marker::Same)
                .map(|(i, _)| i)
                .collect();
            (0..rows.len())
                .map(|i| marked.iter().any(|&m| m.abs_diff(i) <= k))
                .collect()
        }
    };
    let half = opts.width.saturating_sub(16) / 2;
    let mut skipped = false;
    for (idx, row) in rows.iter().enumerate() {
        let c = row
            .cpp
            .map(|i| unit_lines(cpp, &cpp.units[i], &mut last_c, ci))
            .unwrap_or_default();
        let r = row
            .rust
            .map(|j| unit_lines(rust, &rust.units[j], &mut last_r, ri))
            .unwrap_or_default();
        if !visible[idx] {
            skipped = true;
            continue;
        }
        if skipped {
            let _ = writeln!(out, "  ...");
            skipped = false;
        }
        let m = row.marker.symbol();
        if c.is_empty() && r.is_empty() {
            // Both sides were already printed; only the notes are new.
        } else {
            match opts.layout {
                Layout::SideBySide => {
                    let n = c.len().max(r.len());
                    for k in 0..n {
                        let (cl, ct) = c.get(k).cloned().unwrap_or_default();
                        let (rl, rt) = r.get(k).cloned().unwrap_or_default();
                        let mark = if k == 0 { m } else { ' ' };
                        let line = format!(
                            "{:>6} {} {} {:>6} {}",
                            cl,
                            pad(&ct, half),
                            mark,
                            rl,
                            fit(&rt, half)
                        );
                        let _ = writeln!(out, "{}", line.trim_end());
                    }
                }
                Layout::Stacked => {
                    let mut first = true;
                    for (l, t) in &c {
                        let mark = if first { m } else { ' ' };
                        first = false;
                        let _ = writeln!(out, "{mark} C {:>6}  {}", l, t.trim_end());
                    }
                    for (l, t) in &r {
                        let mark = if first { m } else { ' ' };
                        first = false;
                        let _ = writeln!(out, "{mark} R {:>6}  {}", l, t.trim_end());
                    }
                }
            }
        }
        for n in &row.notes {
            let tag = n.severity.marker();
            let _ = writeln!(out, "{:>8}^ {tag} {}", "", n.message);
        }
    }
    if skipped {
        let _ = writeln!(out, "  ...");
    }
}

fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(w.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn pad(s: &str, w: usize) -> String {
    let t = fit(s, w);
    let n = t.chars().count();
    format!("{t}{}", " ".repeat(w - n.min(w)))
}
