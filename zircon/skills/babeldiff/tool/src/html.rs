//! A self-contained HTML report for people.
//!
//! The page puts what needs attention first: a sidebar lists every function
//! pair with its issue and note counts, each pair opens with the checks that
//! differ, and the aligned code dims what matches so that the rows with
//! differences stand out. Hovering an identifier highlights it, under its
//! other spelling, on both sides. Everything (styles, script) is inline, so
//! the file can be copied anywhere and opened in a browser.

use crate::analyze::{CppOrigin, Link, PairReport, Report};
use crate::check::{Marker, Row, Severity};
use crate::model::{Function, Lang, Unit};
use crate::normalize;
use std::fmt::Write;

#[derive(Clone, Debug, Default)]
pub struct HtmlOptions {
    /// What was compared, such as a commit or a patch's subject.
    pub title: String,
}

pub fn render_html(report: &Report, opts: &HtmlOptions) -> String {
    let mut out = String::with_capacity(64 * 1024);
    let title = if opts.title.is_empty() {
        "babeldiff report".to_string()
    } else {
        format!("babeldiff: {}", opts.title)
    };
    let _ = writeln!(
        out,
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n<style>{CSS}</style>\n</head>\n<body>",
        esc(&title)
    );

    // Top bar: what this is, the totals, and the view controls.
    let issues = report.issues();
    let _ = writeln!(
        out,
        "<header class=\"top\">\n<div class=\"brand\">babeldiff</div>\
         <div class=\"what\">{}</div>\n<div class=\"totals\">{}{}{}{}</div>\n\
         <div class=\"controls\">\
         <label title=\"Collapse runs of equivalent lines and expected Rust additions, \
         leaving the rows that differ\"><input type=\"checkbox\" id=\"opt-fold\"> Hide equivalent rows</label>\
         <label>Show <select id=\"opt-show\">\
         <option value=\"all\">all functions</option>\
         <option value=\"findings\">functions with issues or notes</option>\
         <option value=\"issues\">functions with issues</option></select></label>\
         <button type=\"button\" id=\"opt-comments\" class=\"wide\" \
         title=\"Your line comments, ready to copy\">Comments <span id=\"comment-count\">0</span></button>\
         <button type=\"button\" id=\"opt-keys\" title=\"Keyboard shortcuts\">?</button>\
         </div>\n</header>",
        esc(&opts.title),
        chip(
            if issues == 0 { "good" } else { "bad" },
            &count(issues, "issue")
        ),
        chip("warn", &count(report.notes(), "note")),
        chip("plain", &count(report.pairs.len(), "function pair")),
        if report.lints.is_empty() {
            String::new()
        } else {
            format!(
                "<a href=\"#lints\">{}</a>",
                chip(
                    if report.lint_issues() == 0 {
                        "warn"
                    } else {
                        "bad"
                    },
                    &count(report.lints.len(), "rubric lint")
                )
            )
        },
    );
    out.push_str(KEYS_HELP);
    out.push_str(COMMENTS_PANEL);

    out.push_str("<div class=\"layout\">\n");
    render_nav(&mut out, report);
    out.push_str("<main id=\"main\">\n");
    if report.pairs.is_empty() {
        out.push_str("<p class=\"empty\">No C++ and Rust functions were paired.</p>\n");
    }
    for (i, p) in report.pairs.iter().enumerate() {
        render_pair(&mut out, i, p);
    }
    render_leftovers(&mut out, report);
    out.push_str("</main>\n</div>\n");
    let _ = writeln!(out, "<script>{JS}</script>\n</body>\n</html>");
    out
}

fn count(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

fn chip(class: &str, text: &str) -> String {
    format!("<span class=\"chip {class}\">{}</span>", esc(text))
}

fn status_class(p: &PairReport) -> &'static str {
    if p.issues() > 0 {
        "bad"
    } else if p.notes() > 0 {
        "warn"
    } else {
        "good"
    }
}

fn render_nav(out: &mut String, report: &Report) {
    out.push_str(
        "<nav class=\"side\" aria-label=\"Functions\">\n\
         <div class=\"progress\"><span id=\"reviewed-count\">0</span> of ",
    );
    let _ = writeln!(
        out,
        "{} reviewed<div class=\"bar\"><div id=\"reviewed-bar\"></div></div></div>\n<ol>",
        report.pairs.len()
    );
    for (i, p) in report.pairs.iter().enumerate() {
        let _ = writeln!(
            out,
            "<li class=\"{}\" data-issues=\"{}\"><a href=\"#p{i}\" data-pair=\"{i}\">\
             <span class=\"dot\" aria-hidden=\"true\"></span>\
             <span class=\"names\"><span class=\"cn\">{}</span><span class=\"rn\">{}</span></span>\
             <span class=\"counts\">{}</span></a></li>",
            status_class(p),
            p.issues(),
            wrap_name(&p.cpp.name),
            wrap_name(&p.rust.name),
            nav_counts(p),
        );
    }
    out.push_str("</ol>\n");
    if !report.lints.is_empty() {
        let _ = writeln!(
            out,
            "<a class=\"extra\" href=\"#lints\">Rubric lints ({})</a>",
            report.lints.len()
        );
    }
    if crate::render::placement_notable(report) {
        out.push_str("<a class=\"extra\" href=\"#placement\">File placement</a>\n");
    }
    if !report.cpp_changes.is_empty() {
        let _ = writeln!(
            out,
            "<a class=\"extra\" href=\"#cpp-changes\">C++ changed outside the port ({})</a>",
            report.cpp_changes.len()
        );
    }
    let unpaired = report.unmatched_cpp.len() + report.unmatched_rust.len();
    if unpaired > 0 {
        let _ = writeln!(
            out,
            "<a class=\"extra\" href=\"#unpaired\">Unpaired functions ({unpaired})</a>"
        );
    }
    if !report.removed_cpp_shims.is_empty() {
        let _ = writeln!(
            out,
            "<a class=\"extra\" href=\"#cpp-shims\">C++ FFI helpers for Rust ({})</a>",
            report.removed_cpp_shims.len()
        );
    }
    if !report.rust_facades.is_empty() {
        let _ = writeln!(
            out,
            "<a class=\"extra\" href=\"#facades\">Rust that calls C++ ({})</a>",
            report.rust_facades.len()
        );
    }
    if !report.shims.is_empty() {
        let _ = writeln!(
            out,
            "<a class=\"extra\" href=\"#shims\">FFI shims ({})</a>",
            report.shims.len()
        );
    }
    out.push_str("</nav>\n");
}

/// A qualified name that may break after each `::`.
fn wrap_name(name: &str) -> String {
    esc(name).replace("::", "::<wbr>")
}

fn nav_counts(p: &PairReport) -> String {
    let mut s = String::new();
    if p.issues() > 0 {
        let _ = write!(s, "<b class=\"n-bad\">{}</b>", p.issues());
    }
    if p.notes() > 0 {
        let _ = write!(s, "<b class=\"n-warn\">{}</b>", p.notes());
    }
    if s.is_empty() {
        s.push_str("<b class=\"n-good\">✓</b>");
    }
    s
}

fn render_pair(out: &mut String, i: usize, p: &PairReport) {
    let _ = writeln!(
        out,
        "<section class=\"pair {}\" id=\"p{i}\" data-pair=\"{i}\" data-issues=\"{}\" \
         data-key=\"{}\">\n<div class=\"phead\">\n<h2><span class=\"cn\">{}</span>\
         <span class=\"arrow\" aria-label=\"corresponds to\">⇄</span><span class=\"rn\">{}</span></h2>\n\
         <label class=\"rev\"><input type=\"checkbox\" class=\"reviewed\"> Reviewed</label>\n</div>",
        status_class(p),
        p.issues(),
        esc(&format!("{}|{}", p.cpp.name, p.rust.name)),
        esc(&p.cpp.name),
        esc(&p.rust.name),
    );

    // Where the two sides come from and how they were paired.
    out.push_str("<div class=\"meta\">");
    let origin = match p.origin {
        CppOrigin::Changed => String::new(),
        CppOrigin::Unchanged => " <span class=\"tag\">not changed by this diff</span>".into(),
    };
    let _ = write!(
        out,
        "<div><span class=\"lang c\">C++</span> <code>{}</code>{origin}</div>\
         <div><span class=\"lang r\">Rust</span> <code>{}</code></div>",
        esc(&p.cpp.location()),
        esc(&p.rust.location()),
    );
    let via = match &p.link {
        Link::Ffi {
            shim,
            shim_location,
        } => Some(format!(
            "Paired because C++ now calls the FFI shim <code>{}</code> <span class=\"loc\">{}</span>",
            esc(shim),
            esc(shim_location)
        )),
        Link::FfiName {
            shim,
            shim_location,
        } => Some(format!(
            "Paired by the name of the FFI shim <code>{}</code> <span class=\"loc\">{}</span>",
            esc(shim),
            esc(shim_location)
        )),
        Link::Forced => Some("Paired with <code>--pair</code>".into()),
        Link::Similarity if !p.rationale.is_empty() => Some(format!("Paired: {}", esc(&p.rationale))),
        Link::Similarity => Some("Paired by name and body similarity".into()),
    };
    if let Some(v) = via {
        let _ = write!(out, "<div class=\"via\">{v}</div>");
    }
    if let Some(f) = &p.forwarder {
        let _ = write!(
            out,
            "<div class=\"via\">Rust <code>{}</code> forwards here <span class=\"loc\">{}</span></div>",
            esc(&f.name),
            esc(&f.location())
        );
    }
    for o in &p.overrides {
        let _ = write!(
            out,
            "<div class=\"via\">Also folds in the C++ override <code>{}</code> <span class=\"loc\">{}</span>, \
             from a related class</div>",
            esc(&o.cpp.name),
            esc(&o.cpp.location())
        );
    }
    out.push_str("</div>\n");

    render_checks(out, p);
    render_findings(out, i, p);
    render_code(out, &format!("p{i}"), &p.cpp, &p.rust, &p.rows);
    for (n, o) in p.overrides.iter().enumerate() {
        let _ = writeln!(
            out,
            "<h3 class=\"override\">Override <code>{}</code> ⇄ <code>{}</code></h3>",
            esc(&o.cpp.name),
            esc(&p.rust.name)
        );
        render_code(out, &format!("p{i}o{n}"), &o.cpp, &p.rust, &o.rows);
    }
    out.push_str("</section>\n");
}

/// The summary checks as a row of cards, differences first.
fn render_checks(out: &mut String, p: &PairReport) {
    let s = &p.summary;
    let list = |v: &[String]| {
        if v.is_empty() {
            "none".to_string()
        } else {
            v.iter()
                .map(|e| {
                    if e == "?" {
                        "? (propagated)"
                    } else {
                        e.as_str()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    let mut cards: Vec<(bool, String)> = Vec::new();
    let seq = |name: &str, a: &[String], b: &[String]| -> (bool, String) {
        let same = a == b;
        let body = if same && a.is_empty() {
            "<div class=\"v\">none on either side</div>".to_string()
        } else if same {
            format!("<div class=\"v\">{}</div>", esc(&list(a)))
        } else {
            format!(
                "<div class=\"v\"><span class=\"lang c\">C++</span> {}</div>\
                 <div class=\"v\"><span class=\"lang r\">Rust</span> {}</div>",
                esc(&list(a)),
                esc(&list(b))
            )
        };
        (
            same,
            format!(
                "<div class=\"card {}\"><div class=\"k\">{name} {}</div>{body}</div>",
                if same { "ok" } else { "diff" },
                if same { "match" } else { "differ" }
            ),
        )
    };
    cards.push(seq("Errors, in order,", &s.cpp_errors, &s.rust_errors));
    cards.push(seq("Locks", &s.cpp_locks, &s.rust_locks));
    let flow_same = s.flow.iter().all(|(_, a, b)| a == b);
    let flow: Vec<String> = s
        .flow
        .iter()
        .map(|(k, a, b)| {
            if a == b {
                format!("{k} {a}")
            } else {
                format!("<b>{k} {a} vs {b}</b>")
            }
        })
        .collect();
    cards.push((
        flow_same,
        format!(
            "<div class=\"card {}\"><div class=\"k\">Control flow {}</div><div class=\"v\">{}</div></div>",
            if flow_same { "ok" } else { "diff" },
            if flow_same { "matches" } else { "differs" },
            if flow.is_empty() {
                "straight-line".to_string()
            } else {
                flow.join(", ")
            }
        ),
    ));
    let missing = s.comments_total - s.comments_same - s.comments_changed;
    let comments_ok = missing == 0 && s.comments_changed == 0;
    cards.push((
        comments_ok,
        format!(
            "<div class=\"card {}\"><div class=\"k\">Comments</div><div class=\"v\">{}</div></div>",
            if missing > 0 {
                "diff"
            } else if s.comments_changed > 0 {
                "warn"
            } else {
                "ok"
            },
            if s.comments_total == 0 {
                "no C++ comments".to_string()
            } else {
                format!(
                    "{} of {} identical{}{}",
                    s.comments_same,
                    s.comments_total,
                    if s.comments_changed > 0 {
                        format!(", {} reworded", s.comments_changed)
                    } else {
                        String::new()
                    },
                    if missing > 0 {
                        format!(", <b>{missing} missing in Rust</b>")
                    } else {
                        String::new()
                    }
                )
            }
        ),
    ));
    cards.sort_by_key(|(ok, _)| *ok);
    out.push_str("<div class=\"checks\">");
    for (_, c) in cards {
        out.push_str(&c);
    }
    let _ = write!(
        out,
        "<div class=\"card sim\"><div class=\"k\">Similarity</div><div class=\"v\">{:.0}%</div></div>",
        p.score * 100.0
    );
    out.push_str("</div>\n");
}

/// Findings, issues first, each linking to its row.
fn render_findings(out: &mut String, i: usize, p: &PairReport) {
    // (severity, block, row, message), where block 0 is the primary C++
    // and block n + 1 is override n.
    let mut items: Vec<(Severity, usize, usize, String)> = Vec::new();
    let blocks =
        std::iter::once((&p.cpp, &p.rows)).chain(p.overrides.iter().map(|o| (&o.cpp, &o.rows)));
    for (b, (_, rows)) in blocks.clone().enumerate() {
        for (k, row) in rows.iter().enumerate() {
            for n in &row.notes {
                items.push((n.severity, b, k, n.message.clone()));
            }
        }
    }
    let blocks: Vec<(&Function, &Vec<Row>)> = blocks.collect();
    if items.is_empty() {
        out.push_str(
            "<p class=\"clean\">No differences found. Check the aligned code below.</p>\n",
        );
        return;
    }
    items.sort_by_key(|(sev, b, k, _)| (*sev, *b, *k));
    out.push_str("<ul class=\"findings\">\n");
    for (sev, b, k, msg) in items {
        let (class, icon) = match sev {
            Severity::Issue => ("bad", "!"),
            Severity::Note => ("warn", "~"),
        };
        let (cpp, rows) = blocks[b];
        let row = &rows[k];
        let anchor = if b == 0 {
            format!("p{i}r{k}")
        } else {
            format!("p{i}o{}r{k}", b - 1)
        };
        let at = |u: Option<usize>, f: &Function| {
            u.map(|j| {
                let unit = &f.units[j];
                match &unit.file {
                    Some(file) => format!("{}:{}", short(file), unit.start_line),
                    None => format!("{}:{}", short(&f.path), unit.start_line),
                }
            })
        };
        let loc: Vec<String> = [at(row.cpp, cpp), at(row.rust, &p.rust)]
            .into_iter()
            .flatten()
            .collect();
        let _ = writeln!(
            out,
            "<li class=\"{class}\"><a href=\"#{anchor}\"><span class=\"icon\">{icon}</span>\
             <span class=\"msg\">{}</span><span class=\"loc\">{}</span></a></li>",
            esc(&msg),
            esc(&loc.join(" · "))
        );
    }
    out.push_str("</ul>\n");
}

/// One side of a row: numbered lines, with lines the alignment skipped
/// (closing braces and the like) shown dimmed.
struct Side<'a> {
    f: &'a Function,
    indent: usize,
    last: usize,
    in_comment: bool,
}

impl<'a> Side<'a> {
    fn new(f: &'a Function) -> Self {
        let indent = f
            .units
            .iter()
            .find(|u| u.file.is_none())
            .map(|u| leading(f.line(u.start_line)))
            .filter(|&n| n != usize::MAX)
            .unwrap_or(0);
        Side {
            f,
            indent,
            last: 0,
            in_comment: false,
        }
    }

    fn line_html(&mut self, no: &str, text: &str, class: &str, title: &str) -> String {
        self.line_in(None, no, text, class, title)
    }

    /// A line; `file` is set when it comes from another file than the
    /// function's, so that a comment on it names the right place.
    fn line_in(
        &mut self,
        file: Option<&str>,
        no: &str,
        text: &str,
        class: &str,
        title: &str,
    ) -> String {
        let code = highlight(text, self.f.lang, &mut self.in_comment);
        let title = if title.is_empty() {
            String::new()
        } else {
            format!(" title=\"{}\"", esc(title))
        };
        let file = file
            .map(|f| format!(" data-p=\"{}\"", esc(f)))
            .unwrap_or_default();
        format!("<div class=\"ln{class}\"{title}{file}><span class=\"no\">{no}</span><span class=\"tx\">{code}</span></div>")
    }

    fn unit(&mut self, u: &Unit) -> String {
        let mut s = String::new();
        if let Some(file) = &u.file {
            let ind = u.ext_lines.iter().map(|l| leading(l)).min().unwrap_or(0);
            let mut in_comment = false;
            std::mem::swap(&mut in_comment, &mut self.in_comment);
            for (k, l) in u.ext_lines.iter().enumerate() {
                let t = cut(l, ind);
                s.push_str(&self.line_in(
                    Some(file),
                    &format!("{}*", u.start_line + k),
                    &t,
                    " ext",
                    &format!("From the declaration in {file}"),
                ));
            }
            std::mem::swap(&mut in_comment, &mut self.in_comment);
            return s;
        }
        let from = u.start_line.max(self.last + 1);
        if from > u.end_line {
            return s;
        }
        if self.last > 0 && from > self.last + 1 {
            s.push_str(&self.gap(self.last + 1, from - 1));
        }
        for l in from..=u.end_line {
            let t = cut(self.f.line(l), self.indent);
            s.push_str(&self.line_html(&l.to_string(), &t, "", ""));
        }
        self.last = u.end_line;
        s
    }

    /// Lines between units: braces, blank lines, and anything the
    /// extraction did not model.
    fn gap(&mut self, from: usize, to: usize) -> String {
        let mut s = String::new();
        for l in from..=to {
            let raw = self.f.line(l);
            if raw.trim().is_empty() {
                continue;
            }
            let t = cut(raw, self.indent);
            s.push_str(&self.line_html(&l.to_string(), &t, " gap", ""));
        }
        s
    }

    /// The rest of the function after the last unit.
    fn tail(&mut self) -> String {
        if self.last == 0 || self.last >= self.f.end_line {
            return String::new();
        }
        let s = self.gap(self.last + 1, self.f.end_line);
        self.last = self.f.end_line;
        s
    }
}

fn row_class(row: &Row, inherited: Option<Severity>) -> &'static str {
    let worst = row.notes.iter().map(|n| n.severity).min().or(inherited);
    match (row.marker, worst) {
        (Marker::Same, _) => "same",
        (Marker::CppOnly, Some(Severity::Issue)) => "conly bad",
        (Marker::CppOnly, _) => "conly warn",
        (Marker::RustOnly, Some(Severity::Issue)) => "ronly bad",
        (Marker::RustOnly, _) => "ronly warn",
        (Marker::Issue, _) => "diff bad",
        (Marker::Note, _) => "diff warn",
    }
}

fn marker_html(m: Marker) -> &'static str {
    match m {
        Marker::Same => "<span class=\"mk\" title=\"Equivalent\">=</span>",
        Marker::Note => "<span class=\"mk\" title=\"Aligned, with a note\">~</span>",
        Marker::Issue => "<span class=\"mk\" title=\"Aligned, with an issue\">!</span>",
        Marker::CppOnly => "<span class=\"mk\" title=\"Only in C++\">◀</span>",
        Marker::RustOnly => "<span class=\"mk\" title=\"Only in Rust\">▶</span>",
    }
}

fn render_code(out: &mut String, id: &str, cpp: &Function, rust: &Function, rows: &[Row]) {
    let mut c = Side::new(cpp);
    let mut r = Side::new(rust);
    let _ = write!(
        out,
        "<div class=\"code\" data-cp=\"{}\" data-rp=\"{}\" data-cf=\"{}\" data-rf=\"{}\">",
        esc(&cpp.path),
        esc(&rust.path),
        esc(&cpp.name),
        esc(&rust.name)
    );
    out.push_str(
        "\n<div class=\"colhead\"><div><span class=\"lang c\">C++</span></div>\
         <div></div><div><span class=\"lang r\">Rust</span></div></div>\n",
    );
    // Consecutive rows with nothing to review, equivalent lines and
    // expected Rust additions, are grouped so that they can be folded away.
    // Each row is kept with whether it is an expected addition.
    let mut run: Vec<(String, bool)> = Vec::new();
    let flush = |out: &mut String, run: &mut Vec<(String, bool)>| {
        if run.len() >= 2 {
            let what = if run.iter().any(|(_, expected)| *expected) {
                "equivalent or expected rows"
            } else {
                "equivalent rows"
            };
            let _ = write!(
                out,
                "<div class=\"run\"><button type=\"button\" class=\"fold\" \
                 title=\"Show these rows\">{} {what}</button>",
                run.len()
            );
            for (r, _) in run.drain(..) {
                out.push_str(&r);
            }
            out.push_str("</div>\n");
        } else {
            for (r, _) in run.drain(..) {
                out.push_str(&r);
            }
        }
    };
    // A run of one-sided rows carries its note on the first row only.
    let mut run_sev: Option<(Marker, Severity)> = None;
    for (k, row) in rows.iter().enumerate() {
        // A safety comment is an expected addition: shown, but not flagged.
        let expected_title = match row.rust.map(|j| &rust.units[j].features) {
            _ if row.cpp.is_some() || !row.notes.is_empty() => None,
            Some(f) if f.safety => Some("Safety comment, expected in Rust"),
            Some(f) if f.lock_plumbing => Some("ksync lock bookkeeping, expected in Rust"),
            _ => None,
        };
        let expected = expected_title.is_some();
        let expected_title = expected_title.unwrap_or_default();
        let inherited = match (run_sev, row.notes.is_empty()) {
            _ if expected => None,
            (Some((m, sev)), true) if m == row.marker => Some(sev),
            _ => None,
        };
        run_sev = if expected {
            run_sev
        } else {
            match row.marker {
                Marker::CppOnly | Marker::RustOnly => row
                    .notes
                    .iter()
                    .map(|n| n.severity)
                    .min()
                    .or(inherited)
                    .map(|sev| (row.marker, sev)),
                _ => None,
            }
        };
        let left = row.cpp.map(|j| c.unit(&cpp.units[j])).unwrap_or_default();
        let right = row.rust.map(|j| r.unit(&rust.units[j])).unwrap_or_default();
        if left.is_empty() && right.is_empty() && row.notes.is_empty() {
            continue;
        }
        let expected_mk = format!("<span class=\"mk\" title=\"{expected_title}\">+</span>");
        let mut html = String::new();
        let _ = write!(
            html,
            "<div class=\"row {}\" id=\"{id}r{k}\">",
            if expected {
                "ronly expected"
            } else {
                row_class(row, inherited)
            }
        );
        let _ = write!(
            html,
            "<div class=\"cell c{}\">{left}</div>{}<div class=\"cell r{}\">{right}</div>",
            if row.cpp.is_none() { " none" } else { "" },
            if expected {
                &expected_mk
            } else {
                marker_html(row.marker)
            },
            if row.rust.is_none() { " none" } else { "" },
        );
        if !row.notes.is_empty() {
            html.push_str("<div class=\"notes\">");
            for n in &row.notes {
                let msg = &n.message;
                let (class, icon) = match n.severity {
                    Severity::Issue => ("bad", "!"),
                    Severity::Note => ("warn", "~"),
                };
                let _ = write!(
                    html,
                    "<div class=\"note {class}\"><span class=\"icon\">{icon}</span>{}</div>",
                    esc(msg)
                );
            }
            html.push_str("</div>");
        }
        html.push_str("</div>\n");
        if expected || (row.marker == Marker::Same && row.notes.is_empty()) {
            run.push((html, expected));
        } else {
            flush(out, &mut run);
            out.push_str(&html);
        }
    }
    flush(out, &mut run);
    let (lt, rt) = (c.tail(), r.tail());
    if !lt.is_empty() || !rt.is_empty() {
        let _ = writeln!(
            out,
            "<div class=\"row same end\"><div class=\"cell c\">{lt}</div><span class=\"mk\"></span>\
             <div class=\"cell r\">{rt}</div></div>"
        );
    }
    out.push_str("</div>\n");
}

fn render_leftovers(out: &mut String, report: &Report) {
    if !report.lints.is_empty() {
        out.push_str(
            "<section class=\"leftover\" id=\"lints\"><h2>Rubric lints</h2>\n\
             <p>Checks of the changed files against the porting rubric that need no pairing: \
             FFI declarations that disagree across the languages, <code>unsafe</code> without \
             a safety comment, FFI shims that do more than forward, and code that landed in a \
             Rust file not named after its C++ file.</p><ul>\n",
        );
        for l in &report.lints {
            let (class, mark) = match l.severity {
                crate::check::Severity::Issue => ("n-bad", "!"),
                crate::check::Severity::Note => ("n-warn", "~"),
            };
            let related = l.related.as_ref().map_or(String::new(), |(p, n)| {
                format!(
                    " <span class=\"loc\">C++ at {}</span>",
                    esc(&format!("{p}:{n}"))
                )
            });
            let _ = writeln!(
                out,
                "<li><b class=\"{class}\">{mark}</b> <span class=\"tag\">{}</span> {} \
                 <span class=\"loc\">{}</span>{related}</li>",
                esc(l.kind.name()),
                esc(&l.message),
                esc(&format!("{}:{}", l.path, l.line)),
            );
        }
        out.push_str("</ul></section>\n");
    }
    if crate::render::placement_notable(report) {
        out.push_str(
            "<section class=\"leftover\" id=\"placement\"><h2>File placement</h2>\n\
             <p>The Rust files each C++ file's functions went to. Each C++ file should become \
             one Rust file named after it; unexpected destinations are marked.</p><ul>\n",
        );
        for pl in &report.placement {
            let parts: Vec<String> = pl
                .targets
                .iter()
                .map(|t| {
                    let names: Vec<&str> = t.functions.iter().map(|(n, _)| n.as_str()).collect();
                    format!(
                        "<span class=\"{}\" title=\"{}\">{} ({})</span>",
                        if t.expected { "loc" } else { "n-bad" },
                        esc(&names.join(", ")),
                        esc(&t.rust_path),
                        t.functions.len()
                    )
                })
                .collect();
            let _ = writeln!(
                out,
                "<li><span class=\"loc\">{}</span> &rarr; {}</li>",
                esc(&pl.cpp_path),
                parts.join(", ")
            );
        }
        out.push_str("</ul></section>\n");
    }
    if !report.cpp_changes.is_empty() {
        out.push_str(
            "<section class=\"leftover\" id=\"cpp-changes\"><h2>C++ changed outside the port</h2>\n\
             <p>The change edits this C++, which stays C++. Nothing on the Rust side corresponds \
             to it, so check each by hand.</p><ul>\n",
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
                format!(" in <code>{}</code>", esc(&c.functions.join(", ")))
            };
            let _ = writeln!(
                out,
                "<li><code>{}</code>{within} <span class=\"loc\">{}</span></li>",
                esc(&c.text),
                esc(&span)
            );
        }
        out.push_str("</ul></section>\n");
    }
    if !report.unmatched_cpp.is_empty() || !report.unmatched_rust.is_empty() {
        out.push_str(
            "<section class=\"leftover\" id=\"unpaired\"><h2>Unpaired functions</h2>\n\
             <p>These changed functions have no counterpart. Check that C++ removed here \
             was really dropped, and that new Rust has no C++ it should match.</p><ul>\n",
        );
        for f in &report.unmatched_cpp {
            let _ = writeln!(
                out,
                "<li><span class=\"lang c\">C++</span> <code>{}</code> <span class=\"loc\">{}</span></li>",
                esc(&f.name),
                esc(&f.location())
            );
        }
        for f in &report.unmatched_rust {
            let callers = report.callers(f);
            let helper = if callers.is_empty() {
                String::new()
            } else {
                format!(
                    " <span class=\"tag\">helper for {}</span>",
                    esc(&callers.join(", "))
                )
            };
            let _ = writeln!(
                out,
                "<li><span class=\"lang r\">Rust</span> <code>{}</code> <span class=\"loc\">{}</span>{helper}</li>",
                esc(&f.name),
                esc(&f.location())
            );
        }
        out.push_str("</ul></section>\n");
    }
    if !report.rust_tests.is_empty() {
        let _ = writeln!(
            out,
            "<section class=\"leftover\" id=\"rust-tests\"><h2>Rust tests with no C++ counterpart ({})</h2><ul>",
            report.rust_tests.len()
        );
        for f in &report.rust_tests {
            let _ = writeln!(
                out,
                "<li><span class=\"lang r\">Rust</span> <code>{}</code> <span class=\"loc\">{}</span></li>",
                esc(&f.name),
                esc(&f.location())
            );
        }
        out.push_str("</ul></section>\n");
    }
    if !report.removed_cpp_shims.is_empty() {
        out.push_str(
            "<section class=\"leftover\" id=\"cpp-shims\"><h2>C++ FFI helpers for Rust</h2>\n\
             <p>These C++ helpers exist for Rust to call through FFI, and the change edits or \
             removes them. They have no Rust counterpart to compare with.</p><ul>\n",
        );
        for f in &report.removed_cpp_shims {
            let _ = writeln!(
                out,
                "<li><code>{}</code> <span class=\"loc\">{}</span></li>",
                esc(&f.name),
                esc(&f.location())
            );
        }
        out.push_str("</ul></section>\n");
    }
    if !report.rust_facades.is_empty() {
        out.push_str(
            "<section class=\"leftover\" id=\"facades\"><h2>Rust that calls C++</h2>\n\
             <p>These Rust functions only forward to C++ through an FFI helper, so there is \
             nothing to compare line by line. Check that each calls the right helper.</p><ul>\n",
        );
        for (f, callee) in &report.rust_facades {
            let _ = writeln!(
                out,
                "<li><code>{}</code> → <code>{}</code> <span class=\"loc\">{}</span></li>",
                esc(&f.name),
                esc(callee),
                esc(&f.location())
            );
        }
        out.push_str("</ul></section>\n");
    }
    if !report.shims.is_empty() {
        out.push_str(
            "<section class=\"leftover\" id=\"shims\"><h2>FFI shims</h2>\n\
             <p>Each shim forwards a C++ call into Rust.</p><ul>\n",
        );
        for s in &report.shims {
            let target = match &s.target {
                Some(t) => format!(" → <code>{}</code>", esc(t)),
                None => " <span class=\"tag\">target not found</span>".into(),
            };
            let _ = writeln!(
                out,
                "<li><code>{}</code>{target} <span class=\"loc\">{}</span></li>",
                esc(&s.shim.name),
                esc(&s.shim.location())
            );
        }
        out.push_str("</ul></section>\n");
    }
}

fn short(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
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

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(ch),
        }
    }
    o
}

const CPP_KEYWORDS: &[&str] = &[
    "alignas",
    "alignof",
    "auto",
    "bool",
    "break",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "constexpr",
    "consteval",
    "constinit",
    "continue",
    "decltype",
    "default",
    "delete",
    "do",
    "double",
    "else",
    "enum",
    "explicit",
    "extern",
    "false",
    "final",
    "float",
    "for",
    "friend",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "mutable",
    "namespace",
    "new",
    "noexcept",
    "nullptr",
    "operator",
    "override",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "static_assert",
    "static_cast",
    "reinterpret_cast",
    "const_cast",
    "dynamic_cast",
    "struct",
    "switch",
    "template",
    "this",
    "throw",
    "true",
    "try",
    "typedef",
    "typename",
    "union",
    "unsigned",
    "using",
    "virtual",
    "void",
    "volatile",
    "while",
];

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while",
];

/// Key under which an identifier matches its spelling in the other
/// language: `kMaxCount`, `MAX_COUNT` and `max_count` share one.
fn ident_key(id: &str) -> String {
    let id = id.strip_prefix("r#").unwrap_or(id);
    let b = id.as_bytes();
    let id = if b.len() > 1 && b[0] == b'k' && b[1].is_ascii_uppercase() {
        &id[1..]
    } else {
        id
    };
    normalize::ident(id)
}

/// Syntax-highlights one line. `in_comment` carries a block comment across
/// lines. Error codes get a key naming the error, so `ZX_ERR_NO_MEMORY` and
/// `Status::NO_MEMORY` highlight together; other identifiers get their
/// normalized name.
pub fn highlight(line: &str, lang: Lang, in_comment: &mut bool) -> String {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut o = String::new();
    let mut i = 0;
    let text = |a: usize, b: usize| -> String { chars[a..b].iter().collect() };
    let keywords = match lang {
        Lang::Cpp => CPP_KEYWORDS,
        Lang::Rust => RUST_KEYWORDS,
    };
    while i < n {
        if *in_comment {
            let mut j = i;
            while j < n && !(chars[j] == '*' && j + 1 < n && chars[j + 1] == '/') {
                j += 1;
            }
            let end = if j < n {
                *in_comment = false;
                j + 2
            } else {
                n
            };
            let _ = write!(o, "<span class=\"cm\">{}</span>", esc(&text(i, end)));
            i = end;
            continue;
        }
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            let _ = write!(o, "<span class=\"cm\">{}</span>", esc(&text(i, n)));
            break;
        }
        if c == '/' && next == Some('*') {
            *in_comment = true;
            let _ = write!(o, "<span class=\"cm\">{}</span>", esc(&text(i, i + 2)));
            i += 2;
            continue;
        }
        if c == '"' {
            let mut j = i + 1;
            while j < n && chars[j] != '"' {
                if chars[j] == '\\' {
                    j += 1;
                }
                j += 1;
            }
            let end = (j + 1).min(n);
            let _ = write!(o, "<span class=\"st\">{}</span>", esc(&text(i, end)));
            i = end;
            continue;
        }
        if c == '\'' {
            // A character literal, or in Rust possibly a lifetime.
            let close = if next == Some('\\') {
                (i + 3..n.min(i + 12)).find(|&j| chars[j] == '\'')
            } else if chars.get(i + 2) == Some(&'\'') {
                Some(i + 2)
            } else if lang == Lang::Cpp {
                (i + 1..n).find(|&j| chars[j] == '\'')
            } else {
                None
            };
            match close {
                Some(j) => {
                    let _ = write!(o, "<span class=\"st\">{}</span>", esc(&text(i, j + 1)));
                    i = j + 1;
                }
                None => {
                    let mut j = i + 1;
                    while j < n && (chars[j].is_alphanumeric() || chars[j] == '_') {
                        j += 1;
                    }
                    let _ = write!(o, "<span class=\"kw\">{}</span>", esc(&text(i, j)));
                    i = j;
                }
            }
            continue;
        }
        if c.is_ascii_digit() {
            let mut j = i;
            while j < n && (chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '.') {
                j += 1;
            }
            let _ = write!(o, "<span class=\"nu\">{}</span>", esc(&text(i, j)));
            i = j;
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let mut j = i;
            while j < n && (chars[j].is_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            // `r#ident`
            if lang == Lang::Rust && j == i + 1 && c == 'r' && chars.get(j) == Some(&'#') {
                j += 1;
                while j < n && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
            }
            let word = text(i, j);
            // `Status::NO_MEMORY` is one token for error matching.
            if lang == Lang::Rust && word == "Status" && text(j, (j + 2).min(n)) == "::" {
                let mut k = j + 2;
                while k < n && (chars[k].is_alphanumeric() || chars[k] == '_') {
                    k += 1;
                }
                let code = text(j + 2, k);
                if !code.is_empty() && code.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
                    let _ = write!(
                        o,
                        "<span class=\"er\" data-k=\"e:{}\">{}</span>",
                        esc(&code),
                        esc(&text(i, k))
                    );
                    i = k;
                    continue;
                }
            }
            if let Some(code) = word.strip_prefix("ZX_ERR_") {
                let _ = write!(
                    o,
                    "<span class=\"er\" data-k=\"e:{}\">{}</span>",
                    esc(code),
                    esc(&word)
                );
            } else if word == "ZX_OK" || (lang == Lang::Rust && word == "Ok") {
                let _ = write!(
                    o,
                    "<span class=\"ok\" data-k=\"e:OK\">{}</span>",
                    esc(&word)
                );
            } else if keywords.contains(&word.as_str()) {
                let _ = write!(o, "<span class=\"kw\">{}</span>", esc(&word));
            } else if lang == Lang::Rust && chars.get(j) == Some(&'!') {
                let _ = write!(
                    o,
                    "<span class=\"mc id\" data-k=\"{}\">{}!</span>",
                    esc(&ident_key(&word)),
                    esc(&word)
                );
                j += 1;
            } else {
                let _ = write!(
                    o,
                    "<span class=\"id\" data-k=\"{}\">{}</span>",
                    esc(&ident_key(&word)),
                    esc(&word)
                );
            }
            i = j;
            continue;
        }
        o.push_str(&esc(&c.to_string()));
        i += 1;
    }
    o
}

const KEYS_HELP: &str = "<div id=\"keys\" hidden><div class=\"box\"><h3>Keyboard shortcuts</h3>\
<dl><dt>j / k</dt><dd>Next / previous difference</dd>\
<dt>n / p</dt><dd>Next / previous function</dd>\
<dt>x</dt><dd>Mark the current function reviewed</dd>\
<dt>f</dt><dd>Hide or show equivalent rows</dd>\
<dt>i</dt><dd>Show all functions, those with issues or notes, or those with issues</dd>\
<dt>c</dt><dd>Show your line comments, ready to copy</dd>\
<dt>?</dt><dd>Show or hide this help</dd></dl>\
<p>Click a line number to comment on that line.</p></div></div>\n";

/// The panel that gathers line comments for copying into a code review or
/// a message to an agent.
const COMMENTS_PANEL: &str = "<div id=\"comments\" hidden><div class=\"box\">\
<h3>Line comments</h3>\
<p class=\"hint\">Click a line number on either side to comment on it. Each comment \
is listed with its file and line, ready to paste into a review or a message to an agent.</p>\
<textarea id=\"comments-text\" readonly spellcheck=\"false\"></textarea>\
<div class=\"buttons\"><button type=\"button\" id=\"comments-copy\" class=\"primary\">Copy all</button>\
<button type=\"button\" id=\"comments-clear\">Delete all</button>\
<button type=\"button\" id=\"comments-close\">Close</button></div></div></div>\n";

const CSS: &str = r#"
:root {
  --bg: #fbfbfa; --panel: #ffffff; --ink: #1f2328; --muted: #6a737d; --faint: #9aa1a9;
  --line: #e4e6e9; --hover: #f3f4f6;
  --bad: #c62828; --bad-bg: #fdecec; --bad-edge: #f3b5b5;
  --warn: #9a6700; --warn-bg: #fff6dc; --warn-edge: #f0d58a; --warn-soft: #fffbef;
  --good: #1a7f37; --good-bg: #e8f5ec;
  --cpp: #3559a8; --rust: #a2482a;
  --hatch: rgba(120,120,120,.08);
  --kw: #8250df; --st: #0a6b3d; --cm: #6e7781; --nu: #0550ae; --er: #b3261e; --mc: #953800;
  --hl: #ffe58f;
  --mono: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace;
  --sans: system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #0f1115; --panel: #161a20; --ink: #e6e8eb; --muted: #9aa4af; --faint: #6b7580;
    --line: #2a3038; --hover: #1d232b;
    --bad: #ff8a80; --bad-bg: #3a1d1f; --bad-edge: #6b2c2f;
    --warn: #f2c14e; --warn-bg: #332a14; --warn-edge: #5e4b1c; --warn-soft: #221e14;
    --good: #6fdd8b; --good-bg: #16301f;
    --cpp: #8fb0ff; --rust: #ffab8a;
    --hatch: rgba(255,255,255,.04);
    --kw: #c9a3ff; --st: #7ee2a8; --cm: #8b949e; --nu: #79c0ff; --er: #ff8a80; --mc: #ffb77a;
    --hl: #6b5a12;
  }
}
* { box-sizing: border-box; }
html { scroll-padding-top: calc(var(--top, 49px) + 34px); }
body { margin: 0; background: var(--bg); color: var(--ink); font: 14px/1.45 var(--sans); }
code, .loc { font-family: var(--mono); font-size: 12.5px; }
.loc { color: var(--muted); }
.top { position: sticky; top: 0; z-index: 5; display: flex; flex-wrap: wrap; align-items: center;
  gap: 8px 16px; padding: 10px 16px; background: var(--panel); border-bottom: 1px solid var(--line); }
.brand { font-weight: 700; letter-spacing: .02em; }
.what { color: var(--muted); font-family: var(--mono); font-size: 12.5px; flex: 1 1 200px;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.totals { display: flex; gap: 6px; }
.chip { display: inline-block; padding: 1px 9px; border-radius: 999px; font-size: 12.5px; font-weight: 600;
  border: 1px solid var(--line); }
.chip.bad { background: var(--bad-bg); color: var(--bad); border-color: var(--bad-edge); }
.chip.good { background: var(--good-bg); color: var(--good); }
.chip.warn { background: var(--warn-bg); color: var(--warn); border-color: var(--warn-edge); }
.chip.plain { color: var(--muted); }
.controls { display: flex; flex-wrap: wrap; gap: 6px 14px; align-items: center; color: var(--muted); font-size: 13px; }
.controls label { cursor: pointer; white-space: nowrap; }
.controls button { width: 26px; height: 26px; border-radius: 50%; border: 1px solid var(--line);
  background: var(--panel); color: var(--muted); cursor: pointer; font-weight: 700; }
.layout { display: grid; grid-template-columns: 300px minmax(0, 1fr); }
.side { position: sticky; top: var(--top, 49px); align-self: start; height: calc(100vh - var(--top, 49px)); overflow: auto;
  border-right: 1px solid var(--line); padding: 12px 8px 24px; background: var(--panel); }
.progress { font-size: 12.5px; color: var(--muted); padding: 0 8px 10px; }
.progress .bar { height: 4px; background: var(--line); border-radius: 2px; margin-top: 5px; overflow: hidden; }
#reviewed-bar { height: 100%; width: 0; background: var(--good); transition: width .2s; }
.side ol { list-style: none; margin: 0; padding: 0; }
.side li a { display: grid; grid-template-columns: 10px minmax(0,1fr) auto; gap: 8px; align-items: center;
  padding: 5px 8px; border-radius: 6px; color: inherit; text-decoration: none; }
.side li a:hover { background: var(--hover); }
.side li.current a { background: var(--hover); box-shadow: inset 3px 0 0 var(--ink); }
.side li.done .names { opacity: .5; }
.side li.done .dot { background: var(--faint) !important; }
.dot { width: 9px; height: 9px; border-radius: 50%; background: var(--good); }
li.bad .dot { background: var(--bad); } li.warn .dot { background: var(--warn); }
.names { display: flex; flex-direction: column; min-width: 0; font-family: var(--mono); font-size: 12px; }
.names span { overflow-wrap: anywhere; }
.names .rn { color: var(--muted); }
.counts { display: flex; gap: 4px; font-size: 11.5px; }
.counts b { min-width: 18px; text-align: center; border-radius: 9px; padding: 0 5px; }
.n-bad { background: var(--bad-bg); color: var(--bad); }
.n-warn { background: var(--warn-bg); color: var(--warn); }
.n-good { color: var(--good); }
.side .extra { display: block; margin: 12px 8px 0; color: var(--muted); font-size: 13px; }
main { padding: 16px 20px 80px; min-width: 0; }
.pair { background: var(--panel); border: 1px solid var(--line); border-radius: 10px; margin: 0 0 22px;
  overflow: clip; }
.pair.bad { border-top: 3px solid var(--bad); } .pair.warn { border-top: 3px solid var(--warn); }
.pair.good { border-top: 3px solid var(--good); }
.pair.done { opacity: .72; }
.phead { display: flex; align-items: flex-start; gap: 12px; padding: 12px 16px 4px; }
.phead h2 { margin: 0; flex: 1; font: 600 15px/1.4 var(--mono); display: flex; flex-wrap: wrap; gap: 4px 10px;
  align-items: baseline; min-width: 0; word-break: break-all; }
.phead .cn { color: var(--cpp); } .phead .rn { color: var(--rust); }
.arrow { color: var(--faint); font-weight: 400; }
.rev { white-space: nowrap; color: var(--muted); font-size: 13px; cursor: pointer; padding-top: 2px; }
.meta { padding: 0 16px 8px; color: var(--muted); font-size: 12.5px; display: grid; gap: 1px; }
.meta code { color: var(--ink); }
h3.override { margin: 12px 16px 4px; font-size: 13px; font-weight: 600; color: var(--muted); }
h3.override code { color: var(--ink); }
.lang { display: inline-block; min-width: 34px; font: 700 10.5px/1.6 var(--sans); text-transform: uppercase;
  letter-spacing: .04em; text-align: center; border-radius: 4px; padding: 0 4px; }
.lang.c { color: var(--cpp); background: color-mix(in srgb, var(--cpp) 12%, transparent); }
.lang.r { color: var(--rust); background: color-mix(in srgb, var(--rust) 12%, transparent); }
.tag { font-size: 11.5px; border: 1px solid var(--line); border-radius: 4px; padding: 0 5px; color: var(--muted); }
.checks { display: flex; flex-wrap: wrap; gap: 8px; padding: 4px 16px 12px; }
.card { border: 1px solid var(--line); border-radius: 8px; padding: 6px 10px; min-width: 130px; max-width: 100%; }
.card .k { font-size: 11.5px; font-weight: 600; color: var(--muted); text-transform: uppercase; letter-spacing: .03em; }
.card .v { font-family: var(--mono); font-size: 12.5px; overflow-wrap: anywhere; }
.card.ok .k::before { content: "✓ "; color: var(--good); }
.card.diff { background: var(--bad-bg); border-color: var(--bad-edge); }
.card.diff .k { color: var(--bad); } .card.diff .k::before { content: "✗ "; }
.card.warn { background: var(--warn-bg); border-color: var(--warn-edge); }
.card.warn .k { color: var(--warn); }
.card.sim { margin-left: auto; text-align: right; }
.clean { margin: 0 16px 12px; color: var(--good); font-size: 13px; }
.findings { list-style: none; margin: 0 16px 12px; padding: 0; border: 1px solid var(--line); border-radius: 8px; overflow: hidden; }
.findings li + li { border-top: 1px solid var(--line); }
.findings a { display: grid; grid-template-columns: 22px minmax(0,1fr) auto; gap: 8px; padding: 5px 10px;
  color: inherit; text-decoration: none; align-items: baseline; }
.findings a:hover { background: var(--hover); }
.findings .msg { overflow-wrap: anywhere; }
.icon { display: inline-grid; place-items: center; width: 18px; height: 18px; border-radius: 50%;
  font: 700 12px/1 var(--mono); flex: none; }
.bad > a .icon, .note.bad .icon { background: var(--bad); color: var(--panel); }
.warn > a .icon, .note.warn .icon { background: var(--warn-edge); color: var(--ink); }
.code { border-top: 1px solid var(--line); font: 12.5px/1.5 var(--mono); }
.colhead, .row { display: grid; grid-template-columns: minmax(0,1fr) 26px minmax(0,1fr); }
.colhead { position: sticky; top: var(--top, 49px); z-index: 2; background: var(--panel); border-bottom: 1px solid var(--line);
  padding: 3px 0; }
.colhead > div { padding: 0 10px; }
.row { border-bottom: 1px solid color-mix(in srgb, var(--line) 55%, transparent); }
.row:target, .row.cur { outline: 2px solid var(--ink); outline-offset: -2px; }
.cell { min-width: 0; padding: 1px 0; }
.cell.none { background-image: repeating-linear-gradient(135deg, var(--hatch) 0 6px, transparent 6px 12px); }
.mk { display: grid; place-items: start center; padding-top: 1px; color: var(--faint); font-weight: 700;
  border-left: 1px solid var(--line); border-right: 1px solid var(--line); }
.ln { display: grid; grid-template-columns: 44px minmax(0,1fr); }
.no { color: var(--faint); text-align: right; padding-right: 10px; user-select: none; }
.tx { white-space: pre-wrap; overflow-wrap: anywhere; padding-right: 10px; padding-left: 2ch; text-indent: -2ch; }
.ln.gap { opacity: .45; }
.ln.ext .no { color: var(--muted); font-style: italic; }
.row.expected .mk { color: var(--good); font-weight: 400; }
.row.expected .cell.c { background-image: none; }
.row.same .mk { color: color-mix(in srgb, var(--faint) 60%, transparent); }
.row.bad .mk { color: var(--bad); } .row.warn .mk { color: var(--warn); }
.row.diff.bad .cell, .row.conly.bad .cell.c, .row.ronly.bad .cell.r { background-color: var(--bad-bg); }
.row.diff.warn .cell, .row.conly.warn .cell.c, .row.ronly.warn .cell.r { background-color: var(--warn-soft); }
.row.bad { box-shadow: inset 3px 0 0 var(--bad); } .row.warn { box-shadow: inset 3px 0 0 var(--warn-edge); }
.notes { grid-column: 1 / -1; padding: 3px 10px 5px 54px; font: 13px/1.45 var(--sans);
  display: grid; gap: 2px; }
.row.bad .notes { background: var(--bad-bg); } .row.warn .notes { background: var(--warn-soft); }
.note.warn { color: var(--muted); font-size: 12.5px; }
.note { display: flex; gap: 7px; align-items: baseline; overflow-wrap: anywhere; }
.note.bad { color: var(--bad); font-weight: 600; }
.run > .fold { display: none; }
body.fold .run:not(.open) > .row { display: none; }
body.fold .run:not(.open) > .fold { display: block; width: 100%; border: 0; border-bottom: 1px solid var(--line);
  background: var(--hover); color: var(--faint); font: 11px/1 var(--sans); padding: 2px 4px 2px 54px;
  text-align: left; cursor: pointer; }
body.fold .run:not(.open) > .fold::before { content: "⋯ "; }
body.fold .run:not(.open) > .fold:hover { color: var(--muted); }
body.show-issues .pair:not(.bad), body.show-issues .side li:not(.bad),
body.show-findings .pair.good, body.show-findings .side li.good { display: none; }
.controls select { font: inherit; color: var(--ink); background: var(--panel); border: 1px solid var(--line);
  border-radius: 6px; padding: 1px 4px; }
.kw { color: var(--kw); } .st { color: var(--st); } .nu { color: var(--nu); } .mc { color: var(--mc); }
.cm { color: var(--cm); font-style: italic; }
.er, .ok { color: var(--er); font-weight: 600; } .ok { color: var(--good); }
.hl { background: var(--hl); border-radius: 2px; }
.leftover { background: var(--panel); border: 1px solid var(--line); border-radius: 10px; padding: 12px 16px; margin-bottom: 22px; }
.leftover h2 { margin: 0 0 4px; font-size: 15px; }
.leftover p { margin: 0 0 8px; color: var(--muted); }
.leftover ul { margin: 0; padding-left: 18px; }
.empty { color: var(--muted); }
#keys { position: fixed; inset: 0; z-index: 10; background: rgba(0,0,0,.35); display: grid; place-items: center; }
#keys[hidden] { display: none; }
#keys .box { background: var(--panel); border-radius: 10px; padding: 16px 20px; min-width: 280px; }
#keys h3 { margin: 0 0 8px; }
#keys dl { display: grid; grid-template-columns: auto 1fr; gap: 4px 14px; margin: 0; }
#keys dt { font-family: var(--mono); font-weight: 700; }
#keys dd { margin: 0; }
.controls button.wide { width: auto; border-radius: 13px; padding: 0 10px; font-weight: 600; }
.controls button.has { color: var(--nu); border-color: var(--nu); }
.code .no { cursor: pointer; }
.code .no:hover { color: var(--nu); }
.code .no:hover::before { content: "+"; float: left; padding-left: 6px; font-weight: 700; }
.ln.commented .no { color: var(--nu); font-weight: 700; }
.lc { margin: 3px 10px 5px 44px; padding: 6px 8px; border: 1px solid var(--line); border-left: 3px solid var(--nu);
  border-radius: 6px; background: var(--panel); font: 13px/1.45 var(--sans); }
.lc .text { white-space: pre-wrap; overflow-wrap: anywhere; }
.lc textarea { display: block; width: 100%; min-height: 4.5em; resize: vertical; font: inherit; color: var(--ink);
  background: var(--bg); border: 1px solid var(--line); border-radius: 4px; padding: 4px 6px; }
.lc .buttons, #comments .buttons { display: flex; gap: 6px; margin-top: 6px; }
.lc button, #comments button { font: 12.5px var(--sans); padding: 2px 10px; border-radius: 6px; cursor: pointer;
  border: 1px solid var(--line); background: var(--panel); color: var(--ink); }
.lc button.primary, #comments button.primary { background: var(--nu); border-color: var(--nu); color: var(--panel); }
#comments { position: fixed; inset: 0; z-index: 10; background: rgba(0,0,0,.35); display: grid; place-items: center; }
#comments[hidden] { display: none; }
#comments .box { background: var(--panel); border-radius: 10px; padding: 16px 20px; width: min(760px, calc(100vw - 32px)); }
#comments h3 { margin: 0 0 6px; }
#comments .hint, #keys p { color: var(--muted); margin: 8px 0; font-size: 13px; }
#comments textarea { display: block; width: 100%; height: 50vh; resize: vertical; font: 12.5px/1.45 var(--mono);
  color: var(--ink); background: var(--bg); border: 1px solid var(--line); border-radius: 6px; padding: 8px; }
@media (max-width: 900px) {
  .layout { grid-template-columns: 1fr; }
  .side { position: static; height: auto; max-height: 40vh; border-right: 0; border-bottom: 1px solid var(--line); }
  main { padding: 12px 8px 60px; }
  .ln { grid-template-columns: 34px minmax(0,1fr); }
  .notes { padding-left: 12px; }
}
@media (max-width: 700px) {
  .colhead { display: none; }
  .row { grid-template-columns: minmax(0,1fr); }
  .row > .mk { display: none; }
  .cell.none { display: none; }
  .cell::before { display: block; font: 700 10px/1.8 var(--sans); letter-spacing: .04em; padding-left: 8px; }
  .cell.c::before { content: "C++"; color: var(--cpp); }
  .cell.r::before { content: "RUST"; color: var(--rust); }
  .cell.r { border-top: 1px dashed var(--line); }
  .row.same .cell::before { display: none; }
  .row.same .cell.r { border-top: 0; }
  .cell.c { box-shadow: inset 3px 0 0 color-mix(in srgb, var(--cpp) 45%, transparent); }
  .cell.r { box-shadow: inset 3px 0 0 color-mix(in srgb, var(--rust) 45%, transparent); }
  .top { position: static; }
  .colhead { top: 0; }
}
@media print {
  .top, .side { position: static; } .layout { grid-template-columns: 1fr; } .side, .controls { display: none; }
  .colhead { position: static; }
}
"#;

const JS: &str = r#"
(function () {
  "use strict";
  var store = {
    get: function (k) { try { return localStorage.getItem(k); } catch (e) { return null; } },
    set: function (k, v) { try { localStorage.setItem(k, v); } catch (e) {} }
  };
  var body = document.body;
  function measure() {
    document.documentElement.style.setProperty("--top", document.querySelector(".top").offsetHeight + "px");
  }
  measure();
  window.addEventListener("resize", measure);
  var pairs = Array.prototype.slice.call(document.querySelectorAll("section.pair"));
  var navItems = Array.prototype.slice.call(document.querySelectorAll(".side li"));
  var prefix = "babeldiff:" + document.title + ":";

  // Reviewed state, remembered per function pair in this browser.
  function updateProgress() {
    var done = 0;
    pairs.forEach(function (p, i) {
      var on = p.querySelector("input.reviewed").checked;
      p.classList.toggle("done", on);
      if (navItems[i]) navItems[i].classList.toggle("done", on);
      if (on) done++;
    });
    document.getElementById("reviewed-count").textContent = done;
    document.getElementById("reviewed-bar").style.width = pairs.length ? (100 * done / pairs.length) + "%" : "0";
  }
  pairs.forEach(function (p) {
    var box = p.querySelector("input.reviewed");
    box.checked = store.get(prefix + p.dataset.key) === "1";
    box.addEventListener("change", function () {
      store.set(prefix + p.dataset.key, box.checked ? "1" : "0");
      updateProgress();
    });
  });
  updateProgress();

  // View options.
  function option(id, cls) {
    var box = document.getElementById(id);
    box.checked = store.get("babeldiff:" + id) === "1";
    body.classList.toggle(cls, box.checked);
    box.addEventListener("change", function () {
      body.classList.toggle(cls, box.checked);
      store.set("babeldiff:" + id, box.checked ? "1" : "0");
    });
    return box;
  }
  var fold = option("opt-fold", "fold");
  // Which functions to show: all, those with issues or notes, or those
  // with issues.
  var show = document.getElementById("opt-show");
  function setShow(v) {
    if (!show.querySelector('option[value="' + v + '"]')) v = "all";
    show.value = v;
    body.classList.toggle("show-findings", v === "findings");
    body.classList.toggle("show-issues", v === "issues");
    store.set("babeldiff:opt-show", v);
  }
  setShow(store.get("babeldiff:opt-show") || (store.get("babeldiff:opt-issues") === "1" ? "issues" : "all"));
  show.addEventListener("change", function () {
    setShow(show.value);
    // Leave the keys to the page, not the menu.
    show.blur();
  });
  document.addEventListener("click", function (e) {
    var b = e.target.closest ? e.target.closest("button.fold") : null;
    if (b) b.parentNode.classList.add("open");
  });

  // Line comments on either side, remembered for this report in this
  // browser and gathered with their file and line for pasting into a code
  // review or a message to an agent.
  var commentKey = prefix + "comments";
  var comments = [];
  try { comments = JSON.parse(store.get(commentKey) || "[]") || []; } catch (e) { comments = []; }
  var nextId = comments.reduce(function (m, c) { return Math.max(m, c.id || 0); }, 0) + 1;
  var panel = document.getElementById("comments");
  var panelText = document.getElementById("comments-text");
  var countEl = document.getElementById("comment-count");
  var commentsButton = document.getElementById("opt-comments");
  function lineInfo(ln) {
    var code = ln.closest(".code"), pair = ln.closest("section.pair");
    var rust = ln.closest(".cell").classList.contains("r");
    return {
      pair: pair ? pair.dataset.key : "",
      side: rust ? "Rust" : "C++",
      path: ln.dataset.p || (rust ? code.dataset.rp : code.dataset.cp),
      fn: rust ? code.dataset.rf : code.dataset.cf,
      line: parseInt(ln.querySelector(".no").textContent, 10),
      code: ln.querySelector(".tx").textContent
    };
  }
  function findLine(c) {
    for (var i = 0; i < pairs.length; i++) {
      if (pairs[i].dataset.key !== c.pair) continue;
      var lines = pairs[i].querySelectorAll(".cell." + (c.side === "Rust" ? "r" : "c") + " .ln");
      for (var j = 0; j < lines.length; j++) {
        var info = lineInfo(lines[j]);
        if (info.path === c.path && info.line === c.line) return lines[j];
      }
    }
    return null;
  }
  function exportText() {
    // Page order; comments whose line is no longer in the report go last.
    var order = Array.prototype.map.call(document.querySelectorAll(".lc[data-id]"), function (b) { return +b.dataset.id; });
    var pos = function (c) { var i = order.indexOf(c.id); return i < 0 ? order.length + c.id : i; };
    return comments.slice().sort(function (a, b) { return pos(a) - pos(b); }).map(function (c) {
      return c.path + ":" + c.line + " (" + c.side + (c.fn ? ", in " + c.fn : "") + ")\n> " +
        c.code.trim() + "\n" + c.text;
    }).join("\n\n");
  }
  function refreshComments() {
    countEl.textContent = comments.length;
    commentsButton.classList.toggle("has", comments.length > 0);
    if (!panel.hidden) panelText.value = exportText();
  }
  function saveComments() {
    store.set(commentKey, JSON.stringify(comments));
    refreshComments();
  }
  function button(label, cls, f) {
    var b = document.createElement("button");
    b.type = "button";
    b.textContent = label;
    if (cls) b.className = cls;
    b.addEventListener("click", f);
    return b;
  }
  function afterComments(ln) {
    var at = ln;
    while (at.nextElementSibling && at.nextElementSibling.classList.contains("lc")) at = at.nextElementSibling;
    return at;
  }
  function marked(ln) {
    var n = ln.nextElementSibling;
    ln.classList.toggle("commented", !!(n && n.classList.contains("lc")));
  }
  function view(ln, c) {
    var box = document.createElement("div");
    box.className = "lc";
    box.dataset.id = c.id;
    var text = document.createElement("div");
    text.className = "text";
    text.textContent = c.text;
    var buttons = document.createElement("div");
    buttons.className = "buttons";
    buttons.appendChild(button("Edit", "", function () { editor(ln, c, box); }));
    buttons.appendChild(button("Delete", "", function () {
      comments = comments.filter(function (x) { return x !== c; });
      box.remove();
      marked(ln);
      saveComments();
    }));
    box.appendChild(text);
    box.appendChild(buttons);
    return box;
  }
  function editor(ln, c, old) {
    var info = c || lineInfo(ln);
    var box = document.createElement("div");
    box.className = "lc edit";
    var ta = document.createElement("textarea");
    ta.value = c ? c.text : "";
    ta.placeholder = "Comment on " + info.path + ":" + info.line + " (Ctrl+Enter to save)";
    function close() {
      if (old) box.replaceWith(old); else box.remove();
      marked(ln);
    }
    function save() {
      var t = ta.value.trim();
      if (!t) { close(); return; }
      if (c) {
        c.text = t;
      } else {
        c = info;
        c.id = nextId++;
        c.text = t;
        comments.push(c);
      }
      box.replaceWith(view(ln, c));
      marked(ln);
      saveComments();
    }
    ta.addEventListener("keydown", function (e) {
      if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) { e.preventDefault(); save(); }
      if (e.key === "Escape") { e.preventDefault(); close(); }
    });
    var buttons = document.createElement("div");
    buttons.className = "buttons";
    buttons.appendChild(button("Save", "primary", save));
    buttons.appendChild(button("Cancel", "", close));
    box.appendChild(ta);
    box.appendChild(buttons);
    if (old) old.replaceWith(box); else afterComments(ln).after(box);
    ln.classList.add("commented");
    ta.focus();
  }
  comments.forEach(function (c) {
    var ln = findLine(c);
    if (!ln) return;
    afterComments(ln).after(view(ln, c));
    ln.classList.add("commented");
    var run = ln.closest(".run");
    if (run) run.classList.add("open");
  });
  refreshComments();
  document.addEventListener("click", function (e) {
    var no = e.target.closest ? e.target.closest(".code .ln > .no") : null;
    if (!no) return;
    var ln = no.parentNode, open = afterComments(ln);
    if (open !== ln && open.classList.contains("edit")) open.querySelector("textarea").focus();
    else editor(ln, null, null);
  });
  function togglePanel(show) {
    panel.hidden = !show;
    if (show) { panelText.value = exportText(); panelText.focus(); panelText.select(); }
  }
  commentsButton.addEventListener("click", function () { togglePanel(panel.hidden); });
  panel.addEventListener("click", function (e) { if (e.target === panel) togglePanel(false); });
  panel.addEventListener("keydown", function (e) { if (e.key === "Escape") togglePanel(false); });
  document.getElementById("comments-close").addEventListener("click", function () { togglePanel(false); });
  var copyButton = document.getElementById("comments-copy");
  copyButton.addEventListener("click", function () {
    var done = function () {
      copyButton.textContent = "Copied";
      setTimeout(function () { copyButton.textContent = "Copy all"; }, 1500);
    };
    var fallback = function () { panelText.select(); try { document.execCommand("copy"); done(); } catch (e) {} };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(panelText.value).then(done, fallback);
    } else {
      fallback();
    }
  });
  document.getElementById("comments-clear").addEventListener("click", function () {
    if (!comments.length || !window.confirm("Delete all " + comments.length + " line comments?")) return;
    comments = [];
    document.querySelectorAll(".lc").forEach(function (b) { b.remove(); });
    document.querySelectorAll(".ln.commented").forEach(function (l) { l.classList.remove("commented"); });
    saveComments();
  });

  // Hovering an identifier highlights every spelling of it in the pair.
  var lit = [];
  document.addEventListener("mouseover", function (e) {
    var t = e.target;
    if (!t.dataset || !t.dataset.k) return;
    lit.forEach(function (x) { x.classList.remove("hl"); });
    var scope = t.closest("section.pair") || document;
    var sel = '[data-k="' + t.dataset.k.replace(/["\\]/g, "\\$&") + '"]';
    lit = Array.prototype.slice.call(scope.querySelectorAll(sel));
    lit.forEach(function (x) { x.classList.add("hl"); });
  });
  document.addEventListener("mouseout", function (e) {
    if (e.target.dataset && e.target.dataset.k) {
      lit.forEach(function (x) { x.classList.remove("hl"); });
      lit = [];
    }
  });

  // Track the function in view.
  var current = 0;
  function setCurrent(i) {
    current = i;
    navItems.forEach(function (li, j) { li.classList.toggle("current", j === i); });
  }
  if ("IntersectionObserver" in window) {
    var obs = new IntersectionObserver(function (entries) {
      entries.forEach(function (en) {
        if (en.isIntersecting) setCurrent(+en.target.dataset.pair);
      });
    }, { rootMargin: "-50% 0px -50% 0px" });
    pairs.forEach(function (p) { obs.observe(p); });
  }

  // j/k and n/p step from the last place they went while it is still on
  // screen, so a row that cannot be scrolled to the middle is never picked
  // again. Otherwise they start from the top of the visible code.
  var last = null;
  function visible(el) { return el.offsetParent !== null; }
  function onScreen(el) {
    var r = el.getBoundingClientRect();
    return visible(el) && r.bottom > 0 && r.top < window.innerHeight;
  }
  function readingLine() {
    return parseFloat(getComputedStyle(document.documentElement).scrollPaddingTop) || 0;
  }
  function go(el) {
    if (!el) return;
    last = el;
    document.querySelectorAll(".row.cur").forEach(function (r) { r.classList.remove("cur"); });
    if (el.classList.contains("row")) {
      var run = el.closest(".run");
      if (run) run.classList.add("open");
      el.classList.add("cur");
    }
    var pair = el.closest("section.pair");
    if (pair) setCurrent(+pair.dataset.pair);
    el.scrollIntoView({ block: el.classList.contains("row") ? "center" : "start" });
  }
  // Following a link to a row or function makes it the place to step from.
  window.addEventListener("hashchange", function () {
    var t = location.hash && document.getElementById(location.hash.slice(1));
    if (t && (t.classList.contains("row") || t.classList.contains("pair"))) last = t;
  });
  function step(list, dir) {
    list = list.filter(visible);
    var from = last && onScreen(last) ? last : null;
    if (from && list.indexOf(from) < 0) {
      // Stepping between functions from a row: start from its function.
      from = from.closest("section.pair");
      if (from && list.indexOf(from) < 0) from = null;
    }
    if (from) { go(list[list.indexOf(from) + dir]); return; }
    var line = readingLine() + 1;
    if (dir > 0) {
      for (var i = 0; i < list.length; i++) {
        if (list[i].getBoundingClientRect().top >= line) { go(list[i]); return; }
      }
    } else {
      for (var j = list.length - 1; j >= 0; j--) {
        if (list[j].getBoundingClientRect().top < line - 2) { go(list[j]); return; }
      }
    }
  }
  var keys = document.getElementById("keys");
  document.getElementById("opt-keys").addEventListener("click", function () { keys.hidden = !keys.hidden; });
  keys.addEventListener("click", function () { keys.hidden = true; });
  document.addEventListener("keydown", function (e) {
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    if (/INPUT|TEXTAREA|SELECT/.test(e.target.tagName) && e.target.type !== "checkbox") return;
    var diffs = Array.prototype.slice.call(document.querySelectorAll(".row.bad, .row.warn"));
    switch (e.key) {
      case "j": step(diffs, 1); break;
      case "k": step(diffs, -1); break;
      case "n": step(pairs, 1); break;
      case "p": step(pairs, -1); break;
      case "x":
        var box = pairs[current] && pairs[current].querySelector("input.reviewed");
        if (box) { box.checked = !box.checked; box.dispatchEvent(new Event("change")); }
        break;
      case "f": fold.click(); break;
      case "i":
        var opts = ["all", "findings", "issues"];
        setShow(opts[(opts.indexOf(show.value) + 1) % opts.length]);
        break;
      case "c": togglePanel(panel.hidden); break;
      case "?": keys.hidden = !keys.hidden; break;
      case "Escape": keys.hidden = true; togglePanel(false); break;
      default: return;
    }
    e.preventDefault();
  });
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_errors_under_one_key() {
        let mut c = false;
        let cpp = highlight("return ZX_ERR_NO_MEMORY;", Lang::Cpp, &mut c);
        let rust = highlight("return Err(Status::NO_MEMORY);", Lang::Rust, &mut c);
        assert!(cpp.contains("data-k=\"e:NO_MEMORY\""));
        assert!(rust.contains("data-k=\"e:NO_MEMORY\""));
    }

    #[test]
    fn identifiers_share_keys_across_spellings() {
        assert_eq!(ident_key("kMaxSubscribers"), "max_subscribers");
        assert_eq!(ident_key("MAX_SUBSCRIBERS"), "max_subscribers");
        assert_eq!(ident_key("subscriber_count_"), "subscriber_count");
        assert_eq!(ident_key("SubscriberCount"), "subscriber_count");
    }

    #[test]
    fn block_comments_span_lines() {
        let mut c = false;
        let a = highlight("x = 1; /* start", Lang::Cpp, &mut c);
        assert!(c);
        let b = highlight("end */ y", Lang::Cpp, &mut c);
        assert!(!c);
        assert!(a.contains("<span class=\"cm\">/*</span>"));
        assert!(b.starts_with("<span class=\"cm\">end */</span>"));
    }

    #[test]
    fn escapes_markup() {
        let mut c = false;
        let h = highlight("if (a < b && c > d) {}", Lang::Cpp, &mut c);
        assert!(h.contains("&lt;") && h.contains("&amp;&amp;") && h.contains("&gt;"));
        assert!(!h.contains(" < "));
    }
}
