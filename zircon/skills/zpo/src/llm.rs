// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What zpo asks the coding agent (see [`crate::agent`]). Three uses, all
//! optional and all labelled as suggestions in the UI:
//!
//! * explain: narrative of the verdict + concrete next steps with source
//!   pointers (the agent may search the Fuchsia checkout)
//! * resolve_roles: which role name each thread requests (reads driver source)
//! * discover_task: turn a fuzzy description into a task spec using trace facts

use crate::agent::Agent;
use crate::extensions::{LESSON_TOPICS, Lessons};
use serde_json::Value;

/// What the prompt says about the recorded traces (see
/// [`crate::trace_context`]).
pub fn traces_section(traces: Option<&Value>, allow_queries: bool) -> String {
    let Some(t) = traces else { return String::new() };
    let caps = t.get("captures").and_then(Value::as_array).cloned().unwrap_or_default();
    if caps.is_empty() {
        return String::new();
    }
    let facts = t.get("facts_md").and_then(Value::as_str).unwrap_or("");
    let mut s = format!(
        "\nRecorded traces\n---------------\nThe report was computed from these captures. Per capture: the \
         critical-path threads, their `utid` in that capture (utids differ between captures) and their time \
         per thread state over the whole capture.\n{facts}"
    );
    let warm = t.get("warm").and_then(Value::as_bool).unwrap_or(false);
    if allow_queries && warm {
        let tp = t.get("tp").and_then(Value::as_str).unwrap_or("trace_processor_shell");
        s += "\nYou can query the captures yourself. Each one is loaded in a warm trace processor session:\n";
        for c in &caps {
            s += &format!(
                "- `{}` ({}): `{tp} query --remote {} --agent \"SELECT ...\"`\n",
                c.get("tag").and_then(Value::as_str).unwrap_or(""),
                c.get("condition").and_then(Value::as_str).unwrap_or(""),
                c.get("session").and_then(Value::as_str).unwrap_or("")
            );
        }
        s += &format!(
            "Rules:
- Put the SQL in one double-quoted argument and use single quotes for SQL strings. Never put `$`,
  backticks or redirections in the command: they make the agent ask the user, and nobody is watching.
- Join on `utid` (per capture, see the tables above), never on `tid`. `ts` and `dur` are in ns.
- `thread_state.state` is 'Running', 'R' (preempted, still runnable), 'W' (woken, waiting for a CPU)
  or 'S' (blocked); `waker_utid` is the thread that woke it.
- Examples:
  `SELECT state, sum(dur)/1000 AS us FROM thread_state WHERE utid = U AND ts BETWEEN A AND B GROUP BY state`
  `SELECT ts, dur, waker_utid FROM thread_state WHERE utid = U AND state IN ('R','W') ORDER BY dur DESC LIMIT 10`
Use queries to check your hypotheses, and quote the numbers you measured. If a command does not run,
continue with the report and the tables above.
"
        );
    } else {
        s += "\nYou cannot run commands against the traces in this conversation; work from the report and the \
              tables above.\n";
    }
    s
}

/// The CPU profile's function table, top 10 per condition (see
/// [`crate::cpu_profile`]).
pub fn run_functions_section(run_functions: Option<&Value>) -> String {
    let Some(rf) = run_functions.and_then(Value::as_object).filter(|o| !o.is_empty()) else {
        return String::new();
    };
    let mut s = String::from(
        "\nWhere the run time goes (CPU profile)\n-------------------------------------\nA separate capture per \
         condition sampled the chain's processes every 100 us. Only samples inside the run segments of the \
         slowest 10% of jobs count. 'self' is the share of those samples whose leaf is this function; 'in \
         kernel: zx_…' rows are syscalls (the thread was in the kernel), with the code that made the call. \
         'context' is the nearest frame that is not generic library code.\n",
    );
    let mut conds: Vec<&String> = rf.keys().collect();
    conds.sort();
    for cond in conds {
        let t = &rf[cond];
        let n = t.get("samples").and_then(Value::as_i64).unwrap_or(0);
        if n == 0 || t.get("error").is_some_and(|e| !e.is_null()) {
            continue;
        }
        let name = match cond.as_str() {
            "idle" => "idle system",
            "stress" => "every core busy",
            other => other,
        };
        s += &format!(
            "\n#### {name}: {n} samples over {} jobs\n\n| self | total | function | binary | thread | context / caller |\n|---:|---:|---|---|---|---|\n",
            t.get("jobs").and_then(Value::as_i64).unwrap_or(0)
        );
        for f in t.get("functions").and_then(Value::as_array).into_iter().flatten().take(10) {
            let str_of = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let ctx = [str_of("caller"), str_of("context")]
                .into_iter()
                .find(|x| !x.is_empty())
                .unwrap_or_default();
            let pct = |k: &str| 100.0 * f.get(k).and_then(Value::as_f64).unwrap_or(0.0) / n as f64;
            s += &format!(
                "| {:.0}% | {:.0}% | `{}` | {} | {} | {ctx} |\n",
                pct("self"),
                pct("total"),
                str_of("name"),
                str_of("binary"),
                str_of("thread")
            );
        }
    }
    s
}

/// The ground rule that asks the agent to link the lessons, if an extension
/// declares them.
fn lessons_rule(lessons: &Lessons) -> String {
    if lessons.url.is_empty() {
        return String::new();
    }
    let anchors: Vec<String> = LESSON_TOPICS
        .iter()
        .filter_map(|(t, what)| lessons.anchors.get(*t).map(|a| format!("{a} {what}")))
        .collect();
    format!(
        "- Link the relevant subsection of the scheduler lesson series when you explain *why*:\n  {} (anchors: {}).\n",
        lessons.url,
        anchors.join(", ")
    )
}

/// The prompt that asks the agent to explain a report.
pub fn explain_prompt(
    report_md: &str,
    fuchsia_dir: &str,
    task: &str,
    traces: Option<&Value>,
    allow_queries: bool,
    run_functions: Option<&Value>,
    lessons: &Lessons,
) -> String {
    format!(
        "You are helping an engineer optimize a task on a device running Fuchsia (Zircon scheduler).
A measurement tool produced the report below. The report decomposes the task's latency into buckets and
reaches a verdict about whether this is a code/dependency/hardware problem or a scheduling problem.

Ground rules (these are the tool's principles, keep them):
- Prefer code/protocol/dependency fixes over scheduler tuning. A deadline profile only helps time spent
  runnable-but-not-running; it cannot make instructions faster or shorten hardware waits.
- Zircon today supports deadline profiles with capacity / period / deadline / affinity, and fair priorities.
  It does NOT yet have criticality tiers or weighted surplus sharing: do not recommend those.
- When a recommendation touches hardware behaviour (e.g. chip-select, DMA, interrupt coalescing) say
  explicitly that it needs hardware-context review and list features to retest.
{}
The Fuchsia checkout is at `{fuchsia_dir}` (read-only for you). The task was specified as `{task}`.
You may look up the drivers/components named in the report (process names like `driver-host-#input`
map to drivers; FIDL method names map to protocol definitions and their server implementations) to make
your suggestions concrete, with file paths. Spend at most ~10 minutes.

Write, in this order:
1. **In one paragraph**: what the task does per job (the chain of threads and FIDL hops), and the verdict in plain words.
2. **What to do first** (3-6 bullets, most valuable first), each with: the evidence from the report, the expected
   effect, and concrete pointers (files/functions/FIDL methods){}.
3. **What not to do yet**, if the report proposes deadline parameters that should wait.
4. **How to verify**: what to measure after the change (the tool re-measures and gates automatically; say what the gate should show).

Report:
```
{report_md}
```
{}{}",
        lessons_rule(lessons),
        if lessons.url.is_empty() { "" } else { ", and the lesson link that explains why" },
        traces_section(traces, allow_queries),
        run_functions_section(run_functions)
    )
}

/// Asks the agent to explain a report, optionally with access to the traces.
#[allow(clippy::too_many_arguments)]
pub fn explain(
    agent: &Agent,
    report_md: &str,
    fuchsia_dir: &str,
    task: &str,
    model: &str,
    traces: Option<&Value>,
    allow_queries: bool,
    run_functions: Option<&Value>,
    lessons: &Lessons,
) -> anyhow::Result<String> {
    let prompt =
        explain_prompt(report_md, fuchsia_dir, task, traces, allow_queries, run_functions, lessons);
    // Querying the traces takes the agent longer (measured: ~9 min, 17 queries).
    let warm = traces.and_then(|t| t.get("warm")).and_then(Value::as_bool).unwrap_or(false);
    let timeout = if allow_queries && warm { 1200 } else { 600 };
    agent.ask(&prompt, "zpo: explain verdict", timeout, model)
}

/// Asks which role each thread requests: threads are [{label, candidates}];
/// returns label -> role.
pub fn resolve_roles(
    agent: &Agent,
    threads: &Value,
    fuchsia_dir: &str,
    profiles_path: &str,
    model: &str,
) -> anyhow::Result<indexmap::IndexMap<String, String>> {
    let prompt = format!(
        "In the Fuchsia checkout at `{fuchsia_dir}` (read-only), determine which scheduler *role name* each of
these threads requests via `fuchsia.scheduler.RoleManager/SetRole` (look for the role string in the driver or
component source: C++ `SetRole`, Rust `set_role_for_this_thread`, fdf dispatcher `scheduler_role` options,
component manifests, or `.profiles` files). The product's profiles file is `{profiles_path}`.
Threads are given as `process/thread[tid]` with the deadline parameters currently in effect and candidate
roles ranked by a heuristic; confirm or correct each. Answer ONLY with a JSON object mapping each thread
label to a role name (or null if you cannot determine it), followed by one short line of justification per
thread.

{}",
        serde_json::to_string_pretty(threads)?
    );
    let text = agent.ask(&prompt, "zpo: resolve roles", 900, model)?;
    Ok(parse_role_mapping(&text))
}

/// The first JSON object in an answer with string values, keeping those.
/// Each object is read with a JSON parser from its opening brace, so braces
/// inside strings, braces in the prose before it (`{role}`) and any text after
/// it do not matter.
pub fn parse_role_mapping(text: &str) -> indexmap::IndexMap<String, String> {
    for (start, _) in text.match_indices('{') {
        let mut iter = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
        if let Some(Ok(Value::Object(m))) = iter.next() {
            let out: indexmap::IndexMap<String, String> =
                m.into_iter().filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string()))).collect();
            if !out.is_empty() {
                return out;
            }
        }
    }
    Default::default()
}

/// Asks for a task spec from a description and trace facts.
pub fn discover_task(
    agent: &Agent,
    description: &str,
    facts: &str,
    model: &str,
) -> anyhow::Result<String> {
    let prompt = format!(
        "An engineer wants to analyze a task on a device running Fuchsia and described it as:
\"{description}\"

Below are facts extracted from a trace of the device while the task was exercised: the most frequent FIDL
methods (kernel:ipc), interrupts, and the busiest threads. Propose the best *task specification* for the
analysis tool, choosing one of:
  events:<start>-><end>   where an event is irq:<n>, ipc:<Method> (write side), ipc-in:<Method> (read side), slice:<Name>
  fidl:<lib/Protocol.Method>
  thread:<process>/<thread>
Prefer an `events:` spec that spans the user-visible latency (e.g. from the device interrupt to the event
reaching the consumer). Give the spec on the first line inside backticks, then 3-5 lines of reasoning, and
one alternative spec.

Trace facts:
{facts}"
    );
    agent.ask(&prompt, "zpo: discover task", 600, model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prompt_invites_queries_only_when_allowed_and_warm() {
        let traces = json!({
            "tp": "/fake/tp", "warm": true, "facts_md": "| t |",
            "captures": [{"tag": "stress-1", "condition": "every core busy", "session": "zpo-abc"}],
        });
        let on = traces_section(Some(&traces), true);
        assert!(on.contains("/fake/tp query --remote zpo-abc --agent"), "{on}");
        // Every command the prompt names matches the allowlist rule.
        assert!(!on.contains("help agent"), "{on}");
        let off = traces_section(Some(&traces), false);
        assert!(!off.contains("--remote"));
        assert!(off.contains("| t |"));
        let mut cold = traces.clone();
        cold["warm"] = json!(false);
        assert!(!traces_section(Some(&cold), true).contains("--remote"));
    }

    #[test]
    fn prompt_lists_top_functions() {
        let rf = json!({"stress": {"samples": 10, "jobs": 3, "functions": [{
            "name": "in kernel: zx_port_wait", "binary": "libzircon.so", "self": 4, "total": 4,
            "thread": "p/t[1]", "caller": "zx::port::Port::wait (libvfs_rust.so)", "context": null,
        }]}});
        let s = run_functions_section(Some(&rf));
        assert!(s.contains("every core busy: 10 samples over 3 jobs"), "{s}");
        assert!(s.contains("| 40% | 40% | `in kernel: zx_port_wait` |"), "{s}");
        assert_eq!(run_functions_section(None), "");
    }

    #[test]
    fn role_mapping_from_answer() {
        let m =
            parse_role_mapping("Here:\n{\"a/b[1]\": \"fuchsia.x\", \"c/d[2]\": null}\nbecause...");
        assert_eq!(m.len(), 1);
        assert_eq!(m["a/b[1]"], "fuchsia.x");
        assert!(parse_role_mapping("no json").is_empty());
        let m = parse_role_mapping("Use {role} names:\n```json\n{\"a/b[1]\": \"fuchsia.x\"}\n```");
        assert_eq!(m["a/b[1]"], "fuchsia.x");
    }
}
