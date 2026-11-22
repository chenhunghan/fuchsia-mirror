// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::builtins::is_special_builtin;
use crate::eval::testing::{
    ResolvedAlias, apply_assignments, is_assignment_flat, parse_simple_command_args,
    resolve_alias_loop, split_assignment_flat,
};
use crate::eval::{
    EXIT_NOT_FOUND, EvalOutcome, ExecutionContext, ShellState, eval_simple, eval_string,
};
use crate::fd::Fd;
use crate::parser::ast::{ASTBuilder, ResolvedWordPart};
use crate::process::{make_pipe, read_fd_to_end};
use bstr::{BStr, BString, ByteSlice};

#[test]
fn test_is_assignment_flat() {
    let mut builder = ASTBuilder::new();

    // Empty parts
    assert!(!is_assignment_flat(&[], &builder));

    // Non-literal tag
    let parts_non_literal = vec![ResolvedWordPart::Var(BString::from("VAR"))];
    let w_non_lit = builder.add_resolved_word(&parts_non_literal);
    let slice_non_lit = builder.get_slice(w_non_lit);
    assert!(!is_assignment_flat(slice_non_lit, &builder));

    // Literal with no '='
    let parts_no_eq = vec![ResolvedWordPart::Literal(BString::from("FOO"))];
    let w_no_eq = builder.add_resolved_word(&parts_no_eq);
    assert!(!is_assignment_flat(builder.get_slice(w_no_eq), &builder));

    // Empty variable name before '='
    let parts_empty_name = vec![ResolvedWordPart::Literal(BString::from("=VAL"))];
    let w_empty_name = builder.add_resolved_word(&parts_empty_name);
    assert!(!is_assignment_flat(builder.get_slice(w_empty_name), &builder));

    // Invalid start character (digit)
    let parts_digit_start = vec![ResolvedWordPart::Literal(BString::from("1VAR=VAL"))];
    let w_digit_start = builder.add_resolved_word(&parts_digit_start);
    assert!(!is_assignment_flat(builder.get_slice(w_digit_start), &builder));

    // Invalid inner character (hyphen)
    let parts_hyphen = vec![ResolvedWordPart::Literal(BString::from("VAR-NAME=VAL"))];
    let w_hyphen = builder.add_resolved_word(&parts_hyphen);
    assert!(!is_assignment_flat(builder.get_slice(w_hyphen), &builder));

    // Valid assignments
    let parts_valid1 = vec![ResolvedWordPart::Literal(BString::from("VAR=VAL"))];
    let w_valid1 = builder.add_resolved_word(&parts_valid1);
    assert!(is_assignment_flat(builder.get_slice(w_valid1), &builder));

    let parts_valid2 = vec![ResolvedWordPart::Literal(BString::from("_VAR123=VAL"))];
    let w_valid2 = builder.add_resolved_word(&parts_valid2);
    assert!(is_assignment_flat(builder.get_slice(w_valid2), &builder));
}

#[test]
fn test_split_assignment_flat() {
    let mut builder = ASTBuilder::new();
    let parts = vec![
        ResolvedWordPart::Literal(BString::from("MY_VAR=hello")),
        ResolvedWordPart::Var(BString::from("SUFFIX")),
    ];
    let w_slice = builder.add_resolved_word(&parts);
    let (name, val_start, remaining) = split_assignment_flat(builder.get_slice(w_slice), &builder);

    assert_eq!(name, "MY_VAR");
    assert_eq!(val_start, "hello");
    assert_eq!(remaining.len(), 1);
}

#[test]
fn test_parse_simple_command_args() {
    let mut builder = ASTBuilder::new();

    let arg_assign1 = builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from("A=1"))]);
    let arg_assign2 = builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from("B=2"))]);
    let arg_empty = builder.add_resolved_word(&[]);
    let arg_cmd = builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from("echo"))]);
    let arg_assign3 = builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from("C=3"))]);

    let cmd_ptr =
        builder.add_simple_command(&[arg_assign1, arg_assign2, arg_empty, arg_cmd, arg_assign3]);

    let (assignments, cmd_args) = parse_simple_command_args(&builder, cmd_ptr);
    assert_eq!(assignments.len(), 2);
    assert_eq!(cmd_args.len(), 3);
}

#[test]
fn test_apply_assignments_readonly_error() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();
    state.make_readonly(BStr::new("RO_VAR"));

    let mut builder = ASTBuilder::new();
    let arg_assign =
        builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from("RO_VAR=123"))]);

    let res = apply_assignments(&builder, &[arg_assign], &mut state, &ctx);
    assert!(res.is_err());
}

#[test]
fn test_apply_assignments_and_backup_restoration() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();
    state.set_var(BStr::new("EXISTING"), BStr::new("old_val"));
    assert!(!state.exported().contains(BStr::new("EXISTING")));

    let mut builder = ASTBuilder::new();
    let arg_assign =
        builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from("EXISTING=new_val"))]);
    let arg_unset =
        builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from("NEW_VAR=temp_val"))]);

    {
        let guard =
            apply_assignments(&builder, &[arg_assign, arg_unset], &mut state, &ctx).unwrap();
        assert_eq!(guard.state.get_var(BStr::new("EXISTING")).unwrap(), "new_val");
        assert!(guard.state.exported().contains(BStr::new("EXISTING")));
        assert!(guard.state.vars().iter().any(|(k, v)| k == "EXISTING" && v == "new_val"));

        assert_eq!(guard.state.get_var(BStr::new("NEW_VAR")).unwrap(), "temp_val");
        assert!(guard.state.exported().contains(BStr::new("NEW_VAR")));
        assert!(guard.state.vars().iter().any(|(k, v)| k == "NEW_VAR" && v == "temp_val"));
    }
    assert_eq!(state.get_var(BStr::new("EXISTING")).unwrap(), "old_val");
    assert!(!state.exported().contains(BStr::new("EXISTING")));
    assert!(!state.vars().iter().any(|(k, _)| k == "EXISTING"));

    assert_eq!(state.get_var(BStr::new("NEW_VAR")), None);
    assert!(!state.exported().contains(BStr::new("NEW_VAR")));
    assert!(!state.vars().iter().any(|(k, _)| k == "NEW_VAR"));
}

#[test]
fn test_apply_assignments_previously_exported_restoration() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();
    state.set_and_export_var(BStr::new("FOO"), BStr::new("old"));

    let mut builder = ASTBuilder::new();
    let arg_assign =
        builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from("FOO=new"))]);

    {
        let guard = apply_assignments(&builder, &[arg_assign], &mut state, &ctx).unwrap();
        assert_eq!(guard.state.get_var(BStr::new("FOO")).unwrap(), "new");
        assert!(guard.state.exported().contains(BStr::new("FOO")));
        assert!(guard.state.vars().iter().any(|(k, v)| k == "FOO" && v == "new"));
    }

    assert_eq!(state.get_var(BStr::new("FOO")).unwrap(), "old");
    assert!(state.exported().contains(BStr::new("FOO")));
    assert!(state.vars().iter().any(|(k, v)| k == "FOO" && v == "old"));
}

#[test]
fn test_apply_assignments_duplicate_var_in_same_command() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();
    state.set_var(BStr::new("FOO"), BStr::new("initial"));

    let mut builder = ASTBuilder::new();
    let assign1 =
        builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from("FOO=first"))]);
    let assign2 =
        builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from("FOO=second"))]);

    {
        let guard = apply_assignments(&builder, &[assign1, assign2], &mut state, &ctx).unwrap();
        assert_eq!(guard.state.get_var(BStr::new("FOO")).unwrap(), "second");
        assert!(guard.state.exported().contains(BStr::new("FOO")));
    }

    assert_eq!(state.get_var(BStr::new("FOO")).unwrap(), "initial");
    assert!(!state.exported().contains(BStr::new("FOO")));
}

#[test]
fn test_eval_simple_assignments_only() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let mut builder = ASTBuilder::new();
    let arg_assign1 =
        builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from("X=100"))]);
    let arg_assign2 =
        builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from("Y=200"))]);
    let cmd_ptr = builder.add_simple_command(&[arg_assign1, arg_assign2]);

    let res = eval_simple(&mut builder, cmd_ptr, &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var(BStr::new("X")).unwrap(), "100");
    assert_eq!(state.get_var(BStr::new("Y")).unwrap(), "200");
    assert!(!state.exported().contains(BStr::new("X")));
    assert!(!state.exported().contains(BStr::new("Y")));
}

#[test]
fn test_eval_simple_bare_and_null_command_assignments_export_semantics() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    // 1. Bare assignment persists without exporting when set -a is off.
    let res = eval_string(b"FOO=bar".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var(BStr::new("FOO")), Some(BString::from("bar")));
    assert!(!state.exported().contains(BStr::new("FOO")));
    assert!(!state.vars().iter().any(|(k, _)| k == "FOO"));

    // 2. Null-command assignment (where command word expands to nothing) persists without exporting.
    let res = eval_string(b"BAR=baz $UNSET_VAR".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var(BStr::new("BAR")), Some(BString::from("baz")));
    assert!(!state.exported().contains(BStr::new("BAR")));
    assert!(!state.vars().iter().any(|(k, _)| k == "BAR"));

    // 3. Previously exported variable modified via bare or null-command assignment remains exported.
    state.export_var(BStr::new("FOO"));
    let res = eval_string(b"FOO=updated $UNSET_VAR".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var(BStr::new("FOO")), Some(BString::from("updated")));
    assert!(state.exported().contains(BStr::new("FOO")));
    assert!(state.vars().iter().any(|(k, v)| k == "FOO" && v == "updated"));

    // 4. With set -a (allexport), both bare and null-command assignments export the variable.
    state.opt_allexport = true;
    let res = eval_string(b"ALLEXP1=v1".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
    assert!(state.exported().contains(BStr::new("ALLEXP1")));
    assert!(state.vars().iter().any(|(k, v)| k == "ALLEXP1" && v == "v1"));

    let res = eval_string(b"ALLEXP2=v2 $UNSET_VAR".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
    assert!(state.exported().contains(BStr::new("ALLEXP2")));
    assert!(state.vars().iter().any(|(k, v)| k == "ALLEXP2" && v == "v2"));
}

#[test]
fn test_eval_simple_prefix_assignment_exported_during_command_and_restored() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    // 1. Shell function: `FOO=bar check_fn` exports FOO during `check_fn` and reverts FOO on return.
    eval_string(b"check_fn() { SEEN_FOO=$FOO; export -p; }".as_bstr(), &mut state, &mut ctx)
        .unwrap();

    let (read_fd, write_fd) = make_pipe().unwrap();
    ctx.set_fd(Fd::STDOUT, write_fd);
    eval_string(b"FOO=bar check_fn".as_bstr(), &mut state, &mut ctx).unwrap();
    ctx.close_fd(Fd::STDOUT);
    let out = String::from_utf8(read_fd_to_end(read_fd).unwrap()).unwrap();
    assert!(out.contains("export FOO='bar'"), "expected FOO='bar' in export -p output: {}", out);
    assert_eq!(state.get_var(BStr::new("SEEN_FOO")), Some(BString::from("bar")));
    assert_eq!(state.get_var(BStr::new("FOO")), None);
    assert!(!state.exported().contains(BStr::new("FOO")));
    assert!(!state.vars().iter().any(|(k, _)| k == "FOO"));

    // If a function modifies the prefix-assigned variable (`FOO=2`), `FOO` is still restored on return.
    eval_string(b"mut_fn() { FOO=2; }".as_bstr(), &mut state, &mut ctx).unwrap();
    eval_string(b"FOO=1 mut_fn".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(state.get_var(BStr::new("FOO")), None);
    assert!(!state.exported().contains(BStr::new("FOO")));

    // 2. Previously exported variable with shell function (`export FOO=old; FOO=new check_fn`):
    let (read_fd, write_fd) = make_pipe().unwrap();
    ctx.set_fd(Fd::STDOUT, write_fd);
    eval_string(b"export FOO=old; FOO=new check_fn".as_bstr(), &mut state, &mut ctx).unwrap();
    ctx.close_fd(Fd::STDOUT);
    let out = String::from_utf8(read_fd_to_end(read_fd).unwrap()).unwrap();
    assert!(out.contains("export FOO='new'"), "expected FOO='new' in export -p output: {}", out);
    assert_eq!(state.get_var(BStr::new("SEEN_FOO")), Some(BString::from("new")));
    assert_eq!(state.get_var(BStr::new("FOO")), Some(BString::from("old")));
    assert!(state.exported().contains(BStr::new("FOO")));
    assert!(state.vars().iter().any(|(k, v)| k == "FOO" && v == "old"));
    state.unset_var(BStr::new("FOO"));

    // 3. Regular builtins (`command`, `echo`, `true`) also export during execution and revert after completion.
    eval_string(b"FOO=bar command eval 'SEEN_CMD_FOO=$FOO'".as_bstr(), &mut state, &mut ctx)
        .unwrap();
    assert_eq!(state.get_var(BStr::new("SEEN_CMD_FOO")), Some(BString::from("bar")));
    assert_eq!(state.get_var(BStr::new("FOO")), None);
    assert!(!state.exported().contains(BStr::new("FOO")));

    eval_string(b"FOO=1 echo hello >/dev/null".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(state.get_var(BStr::new("FOO")), None);
    assert!(!state.exported().contains(BStr::new("FOO")));

    eval_string(b"FOO=1 true".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(state.get_var(BStr::new("FOO")), None);
    assert!(!state.exported().contains(BStr::new("FOO")));

    // 4. Prefix assignment on a compound alias.
    state.aliases.insert(
        BString::from("comp_alias"),
        BString::from("SEEN_FROM_ALIAS=$TEMP_ALIAS_VAR; true"),
    );
    eval_string(b"TEMP_ALIAS_VAR=alias_val comp_alias".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(state.get_var(BStr::new("SEEN_FROM_ALIAS")), Some(BString::from("alias_val")));
    assert_eq!(state.get_var(BStr::new("TEMP_ALIAS_VAR")), None);
    assert!(!state.exported().contains(BStr::new("TEMP_ALIAS_VAR")));
}

#[test]
fn test_is_special_builtin() {
    let special = [
        ":", ".", "break", "continue", "eval", "exec", "exit", "export", "readonly", "return",
        "set", "shift", "times", "trap", "unset",
    ];
    for name in special {
        assert!(is_special_builtin(name), "expected {} to be special builtin", name);
    }

    let regular = [
        "echo", "printf", "test", "[", "cd", "pwd", "read", "command", "type", "alias", "unalias",
        "getopts", "jobs", "fg", "bg", "wait", "umask", "ulimit", "local", "true", "false",
    ];
    for name in regular {
        assert!(!is_special_builtin(name), "expected {} to be regular builtin", name);
    }
}

#[test]
fn test_eval_simple_prefix_assignment_persists_for_special_builtins() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    // 1. `FOO=1 :` persists FOO=1 (unexported unless -a).
    eval_string(b"FOO=1 :".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(state.get_var(BStr::new("FOO")), Some(BString::from("1")));
    assert!(!state.exported().contains(BStr::new("FOO")));

    // 2. `EXP_VAR=1 export EXP_VAR` persists EXP_VAR=1 AND leaves EXP_VAR exported.
    eval_string(b"EXP_VAR=1 export EXP_VAR".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(state.get_var(BStr::new("EXP_VAR")), Some(BString::from("1")));
    assert!(state.exported().contains(BStr::new("EXP_VAR")));
    assert!(state.vars().iter().any(|(k, v)| k == "EXP_VAR" && v == "1"));

    // 3. `RO_PRE=1 readonly RO_PRE` persists RO_PRE=1 and marks RO_PRE readonly.
    eval_string(b"RO_PRE=1 readonly RO_PRE".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(state.get_var(BStr::new("RO_PRE")), Some(BString::from("1")));
    assert!(state.is_readonly(BStr::new("RO_PRE")));
    assert!(!state.exported().contains(BStr::new("RO_PRE")));

    // 4. `EVAL_FOO=1 eval 'EVAL_BAR=$EVAL_FOO'` persists both EVAL_FOO=1 and EVAL_BAR=1 (unexported),
    // and `EVAL_EXP=1 eval 'export EVAL_EXP'` persists EVAL_EXP=1 AND leaves EVAL_EXP exported.
    eval_string(b"EVAL_FOO=1 eval 'EVAL_BAR=$EVAL_FOO'".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(state.get_var(BStr::new("EVAL_FOO")), Some(BString::from("1")));
    assert_eq!(state.get_var(BStr::new("EVAL_BAR")), Some(BString::from("1")));
    assert!(!state.exported().contains(BStr::new("EVAL_FOO")));

    eval_string(b"EVAL_EXP=1 eval 'export EVAL_EXP'".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(state.get_var(BStr::new("EVAL_EXP")), Some(BString::from("1")));
    assert!(state.exported().contains(BStr::new("EVAL_EXP")));
    assert!(state.vars().iter().any(|(k, v)| k == "EVAL_EXP" && v == "1"));

    // 5. Bare `EXEC_BARE=1 exec` and `EXEC_CMD=1 exec /nonexistent_bin` persist prefix assignments.
    eval_string(b"EXEC_BARE=1 exec".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(state.get_var(BStr::new("EXEC_BARE")), Some(BString::from("1")));
    assert!(!state.exported().contains(BStr::new("EXEC_BARE")));

    let exec_res =
        eval_string(b"EXEC_CMD=1 exec /nonexistent_zxsh_test_bin".as_bstr(), &mut state, &mut ctx)
            .unwrap();
    assert_eq!(exec_res, EvalOutcome::Exit(EXIT_NOT_FOUND));
    assert_eq!(state.get_var(BStr::new("EXEC_CMD")), Some(BString::from("1")));
    assert!(!state.exported().contains(BStr::new("EXEC_CMD")));
}

#[test]
fn test_eval_simple_readonly_error() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();
    state.make_readonly(BStr::new("RO_VAR"));

    let mut builder = ASTBuilder::new();
    let arg_assign =
        builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from("RO_VAR=test"))]);
    let cmd_ptr = builder.add_simple_command(&[arg_assign]);

    let res = eval_simple(&mut builder, cmd_ptr, &mut state, &mut ctx);
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("is read only"));
}

#[test]
fn test_eval_simple_function_execution() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let mut func_builder = ASTBuilder::new();
    let body_cmd = func_builder.add_empty_simple_command();
    let serialized = func_builder.get_ref(body_cmd).serialize(&func_builder);
    state.add_function(BString::from("my_func"), serialized);

    let mut builder = ASTBuilder::new();
    let arg_fn = builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from("my_func"))]);
    let arg_param = builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from("arg1"))]);
    let cmd_ptr = builder.add_simple_command(&[arg_fn, arg_param]);

    let res = eval_simple(&mut builder, cmd_ptr, &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
}

#[test]
fn test_resolve_alias_loop_words_and_command() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    state.aliases.insert(BString::from("myalias"), BString::from("echo hello"));

    let mut builder = ASTBuilder::new();
    let arg_alias =
        builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from("myalias"))]);

    let resolved = resolve_alias_loop(&mut builder, vec![arg_alias], &mut state, &mut ctx).unwrap();
    match resolved {
        ResolvedAlias::Words(words) => {
            assert!(!words.is_empty());
        }
        _ => panic!("Expected words"),
    }
}

#[test]
fn test_eval_simple_nonexistent_external_command() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    // 1. Nonexistent external command sets $? = 127 and returns EvalOutcome::Code(127) without Err.
    // Prefix assignment on failed external command is reverted.
    let (err_read, err_write) = make_pipe().unwrap();
    ctx.set_fd(Fd::STDERR, err_write);
    let res =
        eval_string(b"TEMP_EXT=val /nonexistent_zxsh_cmd".as_bstr(), &mut state, &mut ctx).unwrap();
    ctx.close_fd(Fd::STDERR);
    assert_eq!(res, EvalOutcome::Code(EXIT_NOT_FOUND));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("127")));
    assert_eq!(state.get_var(BStr::new("TEMP_EXT")), None);
    let err_out = String::from_utf8(read_fd_to_end(err_read).unwrap()).unwrap();
    assert!(
        err_out.contains("zxsh: /nonexistent_zxsh_cmd:"),
        "expected diagnostic on stderr, got: {}",
        err_out
    );

    // 2. With set -e, nonexistent external command returns EvalOutcome::Exit(127) and sets $? = 127.
    state.set_last_status(0);
    state.opt_errexit = true;
    let res = eval_string(b"/nonexistent_zxsh_cmd".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Exit(EXIT_NOT_FOUND));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("127")));
}

#[test]
fn test_command_substitution_exit_status() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    // 1. Bare assignment with failing command substitution sets $? and returns non-zero code.
    let res = eval_string(b"x=$(false)".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(1));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("1")));
    assert_eq!(state.get_var(BStr::new("x")), Some(BString::from("")));

    // 2. Plain assignment without command substitution resets $? to 0.
    let res = eval_string(b"x=plain".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("0")));
    assert_eq!(state.get_var(BStr::new("x")), Some(BString::from("plain")));

    // 3. Multiple assignments on one line: last command substitution status wins, even if followed by plain assignment.
    let res =
        eval_string(b"a=$(printf val; exit 42) b=plain".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(42));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("42")));
    assert_eq!(state.get_var(BStr::new("a")), Some(BString::from("val")));
    assert_eq!(state.get_var(BStr::new("b")), Some(BString::from("plain")));

    // 4. Multiple command substitutions in assignments: last command substitution status wins.
    let res = eval_string(b"a=$(exit 42) b=$(true)".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("0")));

    // 5. Backtick and double-quoted command substitutions in assignments propagate exit status.
    let res = eval_string(b"x=`exit 13`".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(13));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("13")));

    let res = eval_string(b"x=\"$(exit 17)\"".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(17));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("17")));

    // 6. Null-command expansion with command substitution sets $? to the command substitution's status.
    let res = eval_string(b"$(exit 7)".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(7));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("7")));

    // 7. Null-command expansion without command substitution resets $? to 0.
    state.unset_var(BStr::new("UNSET_CMD_VAR"));
    let res = eval_string(b"$UNSET_CMD_VAR".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("0")));

    // 8. Command substitution in arguments to a real command is overwritten by the command's exit status
    // and does not leak into a subsequent bare assignment.
    let (out_read, out_write) = make_pipe().unwrap();
    ctx.set_fd(Fd::STDOUT, out_write);
    let res = eval_string(b"echo $(false); after_echo=1".as_bstr(), &mut state, &mut ctx).unwrap();
    ctx.close_fd(Fd::STDOUT);
    drop(out_read);
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("0")));
    assert_eq!(state.get_var(BStr::new("after_echo")), Some(BString::from("1")));

    // 9. Bare heredoc redirection with command substitution propagates exit status.
    let res = eval_string(b"<<EOF\n$(exit 23)\nEOF\n".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(23));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("23")));

    // 10. Command substitutions in for/case headers do not leak into bare assignments in their bodies.
    let res = eval_string(
        b"for v in $(printf item; exit 55); do body_var=1; done".as_bstr(),
        &mut state,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("0")));

    let res = eval_string(
        b"case $(printf m; exit 66) in $(printf m; exit 77)) case_var=2 ;; esac".as_bstr(),
        &mut state,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("0")));

    // 11. With set -e, failing command substitution in bare assignment or null command exits,
    // whereas failing command substitution in argument to succeeding command does not.
    state.opt_errexit = true;
    let (out_read, out_write) = make_pipe().unwrap();
    ctx.set_fd(Fd::STDOUT, out_write);
    let res = eval_string(b"echo $(false)".as_bstr(), &mut state, &mut ctx).unwrap();
    ctx.close_fd(Fd::STDOUT);
    drop(out_read);
    assert_eq!(res, EvalOutcome::Code(0));

    let res = eval_string(b"x=$(exit 5)".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Exit(5));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("5")));

    let res = eval_string(b"$(exit 9)".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Exit(9));
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("9")));
}

#[test]
fn test_eval_simple_external_command_expands_once_and_dynamic_name() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    // Side-effecting argument to an external command must be expanded exactly once.
    state.set_var(BStr::new("x"), BStr::new("0"));
    let res =
        eval_string(b"/pkg/bin/zxsh -c ':' $((x += 1))".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
    assert_eq!(state.get_var(BStr::new("x")), Some(BString::from("1")));

    // Dynamic variable expanding to an external binary executes directly without infinite recursion.
    state.set_var(BStr::new("ext_cmd"), BStr::new("/pkg/bin/zxsh"));
    let res = eval_string(b"$ext_cmd -c ':'".as_bstr(), &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
}
