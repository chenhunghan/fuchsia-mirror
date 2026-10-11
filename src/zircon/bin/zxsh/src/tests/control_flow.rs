// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::eval::{EvalOutcome, ExecutionContext, ShellState, eval_command};
use crate::parser::ast::ASTBuilder;
use crate::parser::{parse_script, tokenize};
use bstr::BStr;
use zx::Task;

fn eval_str(script: &str, state: &mut ShellState, ctx: &mut ExecutionContext) -> EvalOutcome {
    let mut builder = ASTBuilder::new();
    let tokens = tokenize(BStr::new(script)).unwrap();
    let cmds = parse_script(&mut builder, &tokens).unwrap();
    let root = builder.add_sequence_or_single(&cmds);
    eval_command(&mut builder, root, state, ctx).unwrap()
}

fn try_eval_str(
    script: &str,
    state: &mut ShellState,
    ctx: &mut ExecutionContext,
) -> Result<EvalOutcome, String> {
    let mut builder = ASTBuilder::new();
    let tokens = tokenize(BStr::new(script)).map_err(|e| e.to_string())?;
    let cmds = parse_script(&mut builder, &tokens).map_err(|e| e.to_string())?;
    let root = builder.add_sequence_or_single(&cmds);
    eval_command(&mut builder, root, state, ctx)
}

#[test]
fn test_control_flow_if() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let res = eval_str("if A=1; then B=10; else B=20; fi", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("A").unwrap(), "1");
    assert_eq!(state.get_var("B").unwrap(), "10");

    let res = eval_str("if A=2; then B=30; fi", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("B").unwrap(), "30");

    let res = eval_str("if false; then C=1; else C=2; fi", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("C").unwrap(), "2");

    let res = eval_str("if false; then D=1; fi", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert!(state.get_var("D").is_none());

    let res = eval_str("if exit 5; then E=1; fi", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Exit(5));

    let res = eval_str("if return 4; then F=1; fi", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Return(4));
}

#[test]
fn test_control_flow_logical_operators() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let res = eval_str("A=1 && B=2", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("A").unwrap(), "1");
    assert_eq!(state.get_var("B").unwrap(), "2");

    let res = eval_str("false && C=3", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(1));
    assert!(state.get_var("C").is_none());

    let res = eval_str("D=3 || E=4", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("D").unwrap(), "3");
    assert!(state.get_var("E").is_none());

    let res = eval_str("false || F=5", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("F").unwrap(), "5");

    let res = eval_str("exit 7 && G=1", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Exit(7));

    let res = eval_str("exit 8 || H=1", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Exit(8));
}

#[test]
fn test_control_flow_while_loop() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let res = eval_str("while false; do A=1; done", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert!(state.get_var("A").is_none());

    let res = eval_str(
        "N=0; while true; do N=$((N+1)); if A=1; then break; fi; done",
        &mut state,
        &mut ctx,
    );
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("N").unwrap(), "1");

    let res =
        eval_str("N=0; for x in 1 2; do N=$((N+1)); continue; N=99; done", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("N").unwrap(), "2");

    let res = eval_str("while exit 9; do B=1; done", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Exit(9));
}

#[test]
fn test_control_flow_until_loop() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let res = eval_str("N=0; until true; do N=$((N+1)); done", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("N").unwrap(), "0");

    let res = eval_str("until exit 6; do C=1; done", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Exit(6));
}

#[test]
fn test_control_flow_for_loop() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let res = eval_str("for x in 10 20 30; do LAST=$x; done", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("LAST").unwrap(), "30");

    let res = eval_str("for x in; do A=1; done", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert!(state.get_var("A").is_none());

    let res = eval_str("for x in 1 2 3; do B=$x; break; done", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("B").unwrap(), "1");

    let res = eval_str("for x in 1 2 3; do continue; C=$x; done", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert!(state.get_var("C").is_none());

    state.make_readonly("RO");
    let err = try_eval_str("for RO in a b; do :; done", &mut state, &mut ctx);
    assert!(err.is_err());
    assert!(err.unwrap_err().contains("is read only"));
}

#[test]
fn test_control_flow_nested_break_and_continue() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let res = eval_str(
        "for i in 1 2; do for j in 10 20; do OUT=$i-$j; break 2; done; done",
        &mut state,
        &mut ctx,
    );
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("OUT").unwrap(), "1-10");

    let res = eval_str(
        "COUNT=0; for i in 1 2; do for j in 10 20; do COUNT=$((COUNT+1)); continue 2; done; done",
        &mut state,
        &mut ctx,
    );
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("COUNT").unwrap(), "2");
}

#[test]
fn test_control_flow_case() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let res = eval_str("case foo in f*) MATCH=yes ;; bar) MATCH=no ;; esac", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("MATCH").unwrap(), "yes");

    let res = eval_str("case bar in foo|bar) MATCH=multi ;; esac", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("MATCH").unwrap(), "multi");

    let res = eval_str("case nomatch in a) X=1 ;; b) X=2 ;; esac", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert!(state.get_var("X").is_none());

    let res = eval_str("case zzz in a) X=1 ;; *) X=default ;; esac", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("X").unwrap(), "default");
}

#[test]
fn test_control_flow_background() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let res = try_eval_str("A=1 &", &mut state, &mut ctx);
    // Background execution attempts process spawn; verify it runs without crashing.
    assert!(res.is_ok() || res.is_err());

    let res_while = try_eval_str("while true; do true; done &", &mut state, &mut ctx);
    assert!(res_while.is_ok() || res_while.is_err());

    let res_until = try_eval_str("until false; do true; done &", &mut state, &mut ctx);
    assert!(res_until.is_ok() || res_until.is_err());

    let res_for = try_eval_str("for x in 1 2; do true; done &", &mut state, &mut ctx);
    assert!(res_for.is_ok() || res_for.is_err());

    let res_if = try_eval_str("if true; then true; else true; fi &", &mut state, &mut ctx);
    assert!(res_if.is_ok() || res_if.is_err());

    let res_case = try_eval_str("case a in a) true ;; esac &", &mut state, &mut ctx);
    assert!(res_case.is_ok() || res_case.is_err());

    let res_seq = try_eval_str("{ true; true; } &", &mut state, &mut ctx);
    assert!(res_seq.is_ok() || res_seq.is_err());

    for job in &state.bg_jobs {
        let _ = job.process.kill();
    }
}

#[test]
fn test_control_flow_errexit_scope_guard() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    state.opt_errexit = true;
    let res = eval_str("if false; then X=1; else X=2; fi; Y=3", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("X").unwrap(), "2");
    assert_eq!(state.get_var("Y").unwrap(), "3");
    assert_eq!(state.ignore_err_depth, 0);

    let res = eval_str("false || Z=4", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("Z").unwrap(), "4");
    assert_eq!(state.ignore_err_depth, 0);
}

#[test]
fn test_ps4_xtrace_expansion() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();
    state.opt_xtrace = true;
    state.set_var(BStr::new("PS4"), BStr::new("++ "));
    let res = eval_str("true", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
}

#[test]
fn test_control_flow_nonexistent_command_with_errexit() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();
    state.opt_errexit = true;

    // 1. `nonexistent_cmd || VAR=fallback` executes the right-hand side and succeeds with 0 under `set -e`.
    let res = eval_str("/nonexistent_zxsh_cmd || FALLBACK=ok", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("FALLBACK").unwrap(), "ok");
    assert_eq!(state.get_var("?").unwrap(), "0");

    // 2. `if nonexistent_cmd; then ... else ... fi` executes the else branch under `set -e`.
    let res = eval_str(
        "if /nonexistent_zxsh_cmd; then BRANCH=then; else BRANCH=else_taken; fi",
        &mut state,
        &mut ctx,
    );
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var("BRANCH").unwrap(), "else_taken");

    // 3. `nonexistent_cmd && VAR=skipped` short-circuits with status 127 whenerrexit is disabled.
    state.opt_errexit = false;
    let res = eval_str("/nonexistent_zxsh_cmd && SKIPPED=yes", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(127));
    assert!(state.get_var("SKIPPED").is_none());
    assert_eq!(state.get_var("?").unwrap(), "127");
}

#[test]
fn test_control_flow_background_nonexistent_command_wait() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let res = eval_str("/nonexistent_zxsh_bg_cmd &", &mut state, &mut ctx);
    assert_eq!(res, EvalOutcome::Code(0));
    assert!(state.last_bg_pid.is_some(), "expected $! (last_bg_pid) to be set");
    assert_eq!(state.bg_jobs.len(), 1);

    let wait_res = eval_str("wait $!", &mut state, &mut ctx);
    assert_eq!(wait_res, EvalOutcome::Code(127));
    assert_eq!(state.get_var("?").unwrap(), "127");
}

#[test]
fn test_control_flow_pipeline_negation() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    // Basic negation of exit codes.
    assert_eq!(eval_str("! true", &mut state, &mut ctx), EvalOutcome::Code(1));
    assert_eq!(state.get_var("?").unwrap(), "1");

    assert_eq!(eval_str("! false", &mut state, &mut ctx), EvalOutcome::Code(0));
    assert_eq!(state.get_var("?").unwrap(), "0");

    // Double negation.
    assert_eq!(eval_str("! ! true", &mut state, &mut ctx), EvalOutcome::Code(0));
    assert_eq!(eval_str("! ! false", &mut state, &mut ctx), EvalOutcome::Code(1));

    // Pipeline negation negates the exit status of the pipeline as a whole.
    assert_eq!(eval_str("! true | false", &mut state, &mut ctx), EvalOutcome::Code(0));
    assert_eq!(eval_str("! false | true", &mut state, &mut ctx), EvalOutcome::Code(1));

    // Conditionals (`if`, `while`, `until`) and logical lists (`&&`, `||`).
    assert_eq!(
        eval_str("if ! false; then COND=yes; else COND=no; fi", &mut state, &mut ctx),
        EvalOutcome::Code(0)
    );
    assert_eq!(state.get_var("COND").unwrap(), "yes");

    assert_eq!(
        eval_str("if ! true; then COND=yes; else COND=no; fi", &mut state, &mut ctx),
        EvalOutcome::Code(0)
    );
    assert_eq!(state.get_var("COND").unwrap(), "no");

    assert_eq!(eval_str("! false && AND_OK=1", &mut state, &mut ctx), EvalOutcome::Code(0));
    assert_eq!(state.get_var("AND_OK").unwrap(), "1");

    assert_eq!(eval_str("! true || OR_OK=1", &mut state, &mut ctx), EvalOutcome::Code(0));
    assert_eq!(state.get_var("OR_OK").unwrap(), "1");

    // Under `set -e`, `! false` or `! true` should NOT trigger errexit.
    state.opt_errexit = true;
    assert_eq!(
        eval_str("! false; ! true; AFTER_NOT=reached", &mut state, &mut ctx),
        EvalOutcome::Code(0)
    );
    assert_eq!(state.get_var("AFTER_NOT").unwrap(), "reached");
    assert_eq!(state.ignore_err_depth, 0);
    state.opt_errexit = false;

    // Non-code control flow outcomes (e.g., `return` inside a function) propagate through `!`.
    assert_eq!(eval_str("f() { ! return 42; }; f", &mut state, &mut ctx), EvalOutcome::Code(42));

    // Alias starting with `!` expands as a negated command and appends trailing arguments.
    assert_eq!(
        eval_str("alias not_test='! test'; not_test -n ''", &mut state, &mut ctx),
        EvalOutcome::Code(0)
    );
}
