// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::eval::testing::{SpawnedProcess, spawn_command_with_redirection};
use crate::eval::{
    EXIT_FAILURE, EXIT_NOT_FOUND, EvalOutcome, ExecutionContext, ShellState, eval_pipeline,
    eval_string,
};
use crate::fd::Fd;
use crate::parser::ast::{ASTBuilder, CommandTag, RedirectTag, RedirectTemplate, ResolvedWordPart};
use crate::process::{make_pipe, read_fd_to_end};
use bstr::{BStr, BString, ByteSlice};

#[test]
fn test_spawn_command_empty_args_error() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();

    let mut builder = ASTBuilder::new();
    let cmd_ptr = builder.add_simple_command(&[]);

    let res =
        spawn_command_with_redirection(&mut builder, cmd_ptr, &mut state, &ctx, None, None, None);
    assert!(res.is_err());
    assert_eq!(res.unwrap_err(), "No command specified");
}

#[test]
fn test_spawn_command_with_redirect_nesting() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();

    let mut builder = ASTBuilder::new();
    let simple_cmd = builder.add_simple_command(&[]);

    let dev_null =
        builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from("/dev/null"))]);
    let template = RedirectTemplate {
        tag: RedirectTag::TO_FILE,
        append: 0,
        clobber: 1,
        expand: 0,
        src_fd: Fd(1),
        dest_fd: Fd(0),
        filename: Some(dev_null),
        body: None,
    };
    let redirects_slice = builder.add_redirects_from_templates(&[template]);
    let redirect_cmd = builder.add_redirect_command(simple_cmd, redirects_slice);

    let res = spawn_command_with_redirection(
        &mut builder,
        redirect_cmd,
        &mut state,
        &ctx,
        None,
        None,
        None,
    );
    assert!(res.is_err());
    assert_eq!(res.unwrap_err(), "No command specified");
}

#[test]
fn test_eval_pipeline_simple() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let mut builder = ASTBuilder::new();
    let cmd1 = builder.add_simple_command(&[]);
    let cmd2 = builder.add_simple_command(&[]);
    let (pipe_cmd_mut, pipe_cmd_ptr) = builder.add_command_uninit(CommandTag::PIPELINE);
    pipe_cmd_mut.left = cmd1;
    pipe_cmd_mut.right = cmd2;

    let res = eval_pipeline(&mut builder, pipe_cmd_ptr, &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
}

#[test]
fn test_spawn_command_not_found_returns_failed_exit_code() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();

    let mut builder = ASTBuilder::new();
    let cmd_word = builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from(
        "/nonexistent_zxsh_test_cmd",
    ))]);
    let cmd_ptr = builder.add_simple_command(&[cmd_word]);

    let (err_read, err_write) = make_pipe().unwrap();
    let res = spawn_command_with_redirection(
        &mut builder,
        cmd_ptr,
        &mut state,
        &ctx,
        None,
        None,
        Some(&err_write),
    )
    .unwrap();
    drop(err_write);

    match res {
        SpawnedProcess::Failed(code) => assert_eq!(code, EXIT_NOT_FOUND),
        SpawnedProcess::Running(_) => panic!("Expected SpawnedProcess::Failed(127)"),
    }

    let err_out = String::from_utf8(read_fd_to_end(err_read).unwrap()).unwrap();
    assert!(
        err_out.contains("zxsh: /nonexistent_zxsh_test_cmd:"),
        "expected diagnostic on stderr, got: {}",
        err_out
    );
}

#[test]
fn test_eval_pipeline_last_stage_exit_status() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    // Huge variable value causes spawn_command to fail with OUT_OF_RANGE/INVALID_ARGS -> EXIT_FAILURE (1).
    let huge = "x".repeat(100_000);
    state.set_var(BStr::new("HUGE_ARG"), BStr::new(&huge));

    // Stage 1 fails with 1, Stage 2 fails with 127 -> Pipeline exit status is 127 (last stage).
    let res = eval_string(
        b"/pkg/bin/zxsh \"$HUGE_ARG\" | /nonexistent_zxsh_stage2".as_bstr(),
        &mut state,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(res, EvalOutcome::Code(EXIT_NOT_FOUND));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("127")));

    // Stage 1 fails with 127, Stage 2 fails with 1 -> Pipeline exit status is 1 (last stage).
    let res = eval_string(
        b"/nonexistent_zxsh_stage1 | /pkg/bin/zxsh \"$HUGE_ARG\"".as_bstr(),
        &mut state,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(res, EvalOutcome::Code(EXIT_FAILURE));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("1")));

    // With `set -e` (`opt_errexit = true`) and `ignore_err_depth == 0`:
    // - `: | /nonexistent_zxsh_cmd` returns `Ok(EvalOutcome::Exit(127))` and sets `$?` to `127`.
    // - `/nonexistent_zxsh_cmd | true` returns `Ok(EvalOutcome::Code(0))` and sets `$?` to `0`.
    state.opt_errexit = true;
    let res = eval_string(b": | /nonexistent_zxsh_cmd".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Exit(EXIT_NOT_FOUND));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("127")));

    let res = eval_string(b"/nonexistent_zxsh_cmd | true".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("0")));
}

#[test]
fn test_pipeline_subshell_isolation_and_dynamic_builtins() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let run_capture =
        |script: &[u8], state: &mut ShellState, ctx: &mut ExecutionContext| -> String {
            let (read_fd, write_fd) = make_pipe().unwrap();
            ctx.set_fd(Fd::STDOUT, write_fd);
            let outcome = eval_string(script.as_bstr(), state, ctx).unwrap();
            ctx.close_fd(Fd::STDOUT);
            assert_eq!(outcome, EvalOutcome::Code(0));
            String::from_utf8(read_fd_to_end(read_fd).unwrap()).unwrap()
        };

    // 1. Builtin with prefix assignment in pipeline: `FOO=1 echo hello | read line; echo $line`
    let out = run_capture(
        b"FOO=1 echo hello | { read line; printf '%s' \"$line\"; }",
        &mut state,
        &mut ctx,
    );
    assert_eq!(out, "hello");
    assert_eq!(state.get_var(BStr::new("FOO")), None);

    // 2. Multi-part quoted/unquoted builtin name in pipeline: `"ec""ho" hello | ...`
    let out = run_capture(
        b"\"ec\"\"ho\" composite | { read line; printf '%s' \"$line\"; }",
        &mut state,
        &mut ctx,
    );
    assert_eq!(out, "composite");

    // 3. Dynamic variable command name expanding to builtin or function in pipeline
    let out = run_capture(
        b"cmd=echo; $cmd dyn_builtin | { read line; printf '%s' \"$line\"; }",
        &mut state,
        &mut ctx,
    );
    assert_eq!(out, "dyn_builtin");

    let out = run_capture(
        b"my_fn() { printf 'from_fn:%s' \"$1\"; }; fn_var=my_fn; $fn_var arg1 | { read line; printf '%s' \"$line\"; }",
        &mut state,
        &mut ctx,
    );
    assert_eq!(out, "from_fn:arg1");

    // 4. Side-effect isolation in pipelines: arithmetic assignment, := parameter assignment,
    // bare assignment, and bare redirection in pipeline stages do not leak into parent shell.
    let res =
        eval_string(b"/pkg/bin/zxsh -c ':' $((x = 42)) | true".as_bstr(), &mut state, &mut ctx)
            .unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var(BStr::new("x")), None);

    let res = eval_string(b"/pkg/bin/zxsh -c ':' ${y:=99} | true".as_bstr(), &mut state, &mut ctx)
        .unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var(BStr::new("y")), None);

    let res = eval_string(b"z=1 | true".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var(BStr::new("z")), None);

    let res = eval_string(b">/dev/null | true".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
}

#[test]
fn test_function_frames_preserved_in_subshells_and_cmd_sub() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    state.set_var(BStr::new("SHARED"), BStr::new("global"));
    state.set_args(vec![BString::from("global_arg1"), BString::from("global_arg2")]);

    let script = br#"
        test_fn() {
            local SHARED=from_local
            local ONLY_LOCAL=local_secret
            shift
            SUB_OUT=$( (printf '%s|%s|%s|%s|%s' "$SHARED" "$ONLY_LOCAL" "$#" "$1" "$*") )
            ( return 42 )
            SUB_RET=$?
            set -- new1 new2
            PIPE_OUT=$(printf '%s|%s|%s' "$SHARED" "$#" "$*" | { read line; printf '%s' "$line"; })
        }
        test_fn fn_arg1 fn_arg2 fn_arg3
    "#;

    let res = eval_string(script.as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(
        state.get_var(BStr::new("SUB_OUT")),
        Some(BString::from("from_local|local_secret|2|fn_arg2|fn_arg2 fn_arg3"))
    );
    assert_eq!(state.get_var(BStr::new("SUB_RET")), Some(BString::from("42")));
    assert_eq!(state.get_var(BStr::new("PIPE_OUT")), Some(BString::from("from_local|2|new1 new2")));
    // Verify global state was not overwritten after function returned
    assert_eq!(state.get_var(BStr::new("SHARED")), Some(BString::from("global")));
    assert_eq!(state.get_var(BStr::new("ONLY_LOCAL")), None);
    assert_eq!(state.get_args(), vec![BString::from("global_arg1"), BString::from("global_arg2")]);
}

#[test]
fn test_root_pid_preserved_across_subshells() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let expected_pid = fuchsia_runtime::process_self().koid().unwrap().raw_koid().to_string();
    assert_eq!(state.get_var(BStr::new("$")), Some(BString::from(expected_pid.as_str())));

    let (read_fd, write_fd) = make_pipe().unwrap();
    ctx.set_fd(Fd::STDOUT, write_fd);

    let script = br#"
        printf 'parent=%s\n' "$$"
        ( printf 'subshell=%s\n' "$$" )
        printf 'cmd_sub=%s\n' "$(printf '%s' "$$")"
        printf 'backtick=%s\n' "`printf '%s' "$$"`"
        printf '%s\n' "$$" | { read p; printf 'pipe=%s|%s\n' "$p" "$$"; }
        ( printf 'nested=%s\n' "$( ( printf '%s' "$$" ) )" )
        { printf 'bg=%s\n' "$$"; } &
        wait
    "#;
    let res = eval_string(script.as_bstr(), &mut state, &mut ctx).unwrap();
    ctx.close_fd(Fd::STDOUT);
    assert_eq!(res, EvalOutcome::Code(0));

    let out = String::from_utf8(read_fd_to_end(read_fd).unwrap()).unwrap();
    let expected_out = format!(
        "parent={p}\n\
         subshell={p}\n\
         cmd_sub={p}\n\
         backtick={p}\n\
         pipe={p}|{p}\n\
         nested={p}\n\
         bg={p}\n",
        p = expected_pid
    );
    assert_eq!(out, expected_out);

    // Separate zxsh invocation (`zxsh -c 'printf %s $$'`) is a new shell, so its $$ is its own process koid.
    let res = eval_string(
        b"CHILD_SH_PID=$(/pkg/bin/zxsh -c 'printf \"%s\" \"$$\"')".as_bstr(),
        &mut state,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
    let child_pid = state.get_var(BStr::new("CHILD_SH_PID")).unwrap();
    let child_pid_str = child_pid.to_str().unwrap();
    assert!(!child_pid_str.is_empty());
    assert!(child_pid_str.parse::<u64>().is_ok(), "expected numeric koid, got {}", child_pid_str);
    assert_ne!(child_pid_str, expected_pid);
}
