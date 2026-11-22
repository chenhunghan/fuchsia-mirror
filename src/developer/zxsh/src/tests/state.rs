// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::args::parse_args;
use crate::collections::FlatMap;
use crate::eval::testing::{Frame, StateBackupGuard};
use crate::eval::{ExecutionContext, ShellState};
use crate::fd::Fd;
use bstr::{BStr, BString, ByteSlice};

#[test]
fn test_env_basic_variables() {
    let mut state = ShellState::new();
    assert_eq!(state.get_var(BStr::new("VAR")), None);

    state.set_var(BStr::new("VAR"), BStr::new("value"));
    assert_eq!(state.get_var(BStr::new("VAR")), Some(BString::from("value")));

    state.unset_var(BStr::new("VAR"));
    assert_eq!(state.get_var(BStr::new("VAR")), None);
}

#[test]
fn test_env_readonly_variables() {
    let mut state = ShellState::new();
    state.set_var(BStr::new("VAR"), BStr::new("value"));
    assert!(!state.is_readonly(BStr::new("VAR")));

    state.make_readonly(BStr::new("VAR"));
    assert!(state.is_readonly(BStr::new("VAR")));

    // Try to overwrite readonly variable (should be ignored)
    state.set_var(BStr::new("VAR"), BStr::new("new_value"));
    assert_eq!(state.get_var(BStr::new("VAR")), Some(BString::from("value")));

    // Try to unset readonly variable (should be ignored)
    state.unset_var(BStr::new("VAR"));
    assert_eq!(state.get_var(BStr::new("VAR")), Some(BString::from("value")));
}

#[test]
fn test_env_functions() {
    let mut state = ShellState::new();
    assert_eq!(state.get_function(BStr::new("my_func")), None);

    let body = vec![1, 2, 3]; // Dummy serialized body
    state.add_function(BString::from("my_func"), body.clone());
    assert_eq!(state.get_function(BStr::new("my_func")), Some(&body));

    let removed = state.remove_function(BStr::new("my_func"));
    assert_eq!(removed, Some(body));
    assert_eq!(state.get_function(BStr::new("my_func")), None);
}

#[test]
fn test_env_aliases() {
    let mut state = ShellState::new();
    assert!(!state.aliases.contains_key(BStr::new("ll")));

    state.aliases.insert(BString::from("ll"), BString::from("ls -l"));
    assert_eq!(state.aliases.get(BStr::new("ll")), Some(&BString::from("ls -l")));
}

#[test]
fn test_internal_fd_leak() {
    let _env = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();
    use std::os::fd::AsRawFd;
    let stdout_phys = ctx.stdout().unwrap().as_raw_fd();
    let res = ctx.dup_fd(Fd(stdout_phys));
    assert!(res.is_err());
}

#[test]
fn test_state_non_utf8_handling() {
    let mut state = ShellState::new();

    // 1. Non-UTF8 global variable name and value
    let var_name = BStr::new(b"VAR_\xFF\xFE");
    let var_val = BStr::new(b"VAL_\x80\x81");
    state.set_var(var_name, var_val);
    assert_eq!(state.get_var(var_name), Some(BString::from(var_val)));

    // 2. Non-UTF8 alias
    let alias_name = BString::from(b"cmd_\xFF");
    let alias_val = BString::from(b"echo \xFE\xFD");
    state.aliases.insert(alias_name.clone(), alias_val.clone());
    assert_eq!(state.aliases.get(alias_name.as_bstr()), Some(&alias_val));

    // 3. Non-UTF8 function name and binary body
    let func_name = BString::from(b"func_\xDE\xAD");
    let func_body = vec![0xFF, 0x00, 0xFE, 0x01];
    state.add_function(func_name.clone(), func_body.clone());
    assert_eq!(state.get_function(func_name.as_bstr()), Some(&func_body));

    // 4. Non-UTF8 positional arguments
    let args = vec![BString::from(b"arg1_\x80"), BString::from(b"arg2_\x90")];
    state.set_args(args.clone());
    assert_eq!(state.get_args(), args);

    // 5. Non-UTF8 local variables within a function frame
    state.frames.push(Frame { local_vars: FlatMap::new(), args: vec![] });
    let local_name = BStr::new(b"LOCAL_\xAA\xBB");
    let local_val = BStr::new(b"LOCAL_VAL_\xCC\xDD");
    state.declare_local(local_name, Some(local_val));
    assert_eq!(state.get_var(local_name), Some(BString::from(local_val)));
}

#[test]
fn test_shell_env() {
    let mut state = ShellState::new();
    state.set_and_export_var(BStr::new("PATH"), BStr::new("/custom/bin:/another/bin"));
    state.set_and_export_var(BStr::new("FOO"), BStr::new("bar"));

    let env = state.vars();
    assert_eq!(env.path().entries().count(), 2);

    let cstrings = env.to_spawn_env().expect("to_spawn_env failed");
    let cstr_strs: Vec<_> = cstrings.iter().map(|s| s.to_str().unwrap()).collect();
    assert!(cstr_strs.contains(&"FOO=bar"));
    assert!(cstr_strs.contains(&"PATH=/custom/bin:/another/bin"));
}

#[test]
fn test_vars_resolves_local_variables_in_frames() {
    let mut state = ShellState::new();
    state.set_and_export_var(BStr::new("SHARED"), BStr::new("global_val"));

    // Push outer function frame: `local FOO=local_val; export FOO` and `local SHARED=outer_val`
    state.frames.push(Frame { local_vars: FlatMap::new(), args: vec![] });
    state.declare_local(BStr::new("FOO"), Some(BStr::new("local_val")));
    state.export_var(BStr::new("FOO"));
    state.declare_local(BStr::new("SHARED"), Some(BStr::new("outer_val")));

    let env = state.vars();
    assert!(env.iter().any(|(k, v)| k == "FOO" && v == "local_val"));
    assert!(env.iter().any(|(k, v)| k == "SHARED" && v == "outer_val"));

    // Push inner function frame shadowing `FOO` and testing `opt_allexport` on `declare_local` & `set_var`
    state.frames.push(Frame { local_vars: FlatMap::new(), args: vec![] });
    state.declare_local(BStr::new("FOO"), Some(BStr::new("inner_val")));
    state.opt_allexport = true;
    state.declare_local(BStr::new("AUTO_LOCAL"), Some(BStr::new("auto_val")));
    state.declare_local(BStr::new("LATER_LOCAL"), None);
    state.set_var(BStr::new("LATER_LOCAL"), BStr::new("later_val"));
    state.opt_allexport = false;

    let inner_env = state.vars();
    assert!(inner_env.iter().any(|(k, v)| k == "FOO" && v == "inner_val"));
    assert!(inner_env.iter().any(|(k, v)| k == "SHARED" && v == "outer_val"));
    assert!(inner_env.iter().any(|(k, v)| k == "AUTO_LOCAL" && v == "auto_val"));
    assert!(inner_env.iter().any(|(k, v)| k == "LATER_LOCAL" && v == "later_val"));

    // Pop inner frame
    state.frames.pop();
    let outer_env = state.vars();
    assert!(outer_env.iter().any(|(k, v)| k == "FOO" && v == "local_val"));

    // Pop outer frame: `FOO` has no global value so it is not in `vars()`, `SHARED` falls back to global
    state.frames.pop();
    let global_env = state.vars();
    assert!(!global_env.iter().any(|(k, _)| k == "FOO"));
    assert!(global_env.iter().any(|(k, v)| k == "SHARED" && v == "global_val"));
}

#[test]
fn test_state_backup_guard_and_unexport_var() {
    let mut state = ShellState::new();
    // 1. Exported but unset variable (`export UNSET_EXP`)
    state.export_var(BStr::new("UNSET_EXP"));
    // 2. Unexported set variable
    state.set_var(BStr::new("UNEXP_SET"), BStr::new("orig"));

    {
        let mut guard = StateBackupGuard::new(&mut state);
        guard.backup_var(BStr::new("UNSET_EXP"));
        guard.backup_var(BStr::new("UNSET_EXP")); // duplicate backup ignored
        guard.state.set_and_export_var(BStr::new("UNSET_EXP"), BStr::new("temp"));

        guard.backup_var(BStr::new("UNEXP_SET"));
        guard.state.set_and_export_var(BStr::new("UNEXP_SET"), BStr::new("temp2"));

        let debug_str = format!("{:?}", guard);
        assert!(debug_str.contains("StateBackupGuard"));
        assert_eq!(guard.backups.len(), 2);
    }

    // UNSET_EXP should be unset again, but still marked exported!
    assert_eq!(state.get_var(BStr::new("UNSET_EXP")), None);
    assert!(state.exported().contains(BStr::new("UNSET_EXP")));

    // UNEXP_SET should be restored to "orig" and unexported!
    assert_eq!(state.get_var(BStr::new("UNEXP_SET")), Some(BString::from("orig")));
    assert!(!state.exported().contains(BStr::new("UNEXP_SET")));
}

#[test]
#[should_panic(expected = "name cannot contain '='")]
fn test_unexport_var_with_equals_panics() {
    let mut state = ShellState::new();
    state.unexport_var(BStr::new("A=B"));
}

#[test]
fn test_variable_values_with_equals() {
    let mut state = ShellState::new();
    state.set_var(BStr::new("FOO"), BStr::new("bar=baz=qux"));
    assert_eq!(state.get_var(BStr::new("FOO")), Some(BString::from("bar=baz=qux")));

    state.export_var(BStr::new("FOO"));
    let env = state.vars();
    let cstrings = env.to_spawn_env().expect("to_spawn_env failed");
    let cstr_strs: Vec<_> = cstrings.iter().map(|s| s.to_str().unwrap()).collect();
    assert!(cstr_strs.contains(&"FOO=bar=baz=qux"));

    // Test local variable values with equals
    state.frames.push(Frame { local_vars: FlatMap::new(), args: vec![] });
    state.declare_local(BStr::new("LOCAL_VAR"), Some(BStr::new("a=b=c=d")));
    assert_eq!(state.get_var(BStr::new("LOCAL_VAR")), Some(BString::from("a=b=c=d")));
}

#[test]
#[should_panic(expected = "name cannot contain '='")]
fn test_set_var_with_equals_panics() {
    let mut state = ShellState::new();
    state.set_var(BStr::new("A=B"), BStr::new("val"));
}

#[test]
#[should_panic(expected = "name cannot contain '='")]
fn test_export_var_with_equals_panics() {
    let mut state = ShellState::new();
    state.export_var(BStr::new("A=B"));
}

#[test]
#[should_panic(expected = "name cannot contain '='")]
fn test_unset_var_with_equals_panics() {
    let mut state = ShellState::new();
    state.unset_var(BStr::new("A=B"));
}

#[test]
#[should_panic(expected = "name cannot contain '='")]
fn test_readonly_var_with_equals_panics() {
    let mut state = ShellState::new();
    state.make_readonly(BStr::new("A=B"));
}

#[test]
#[should_panic(expected = "name cannot contain '='")]
fn test_declare_local_with_equals_panics() {
    let mut state = ShellState::new();
    state.frames.push(Frame { local_vars: FlatMap::new(), args: vec![] });
    state.declare_local(BStr::new("A=B"), Some(BStr::new("val")));
}

#[test]
#[should_panic(expected = "name cannot contain '='")]
fn test_add_function_with_equals_panics() {
    let mut state = ShellState::new();
    state.add_function(BString::from("A=B"), vec![]);
}

#[test]
#[should_panic(expected = "name cannot contain '='")]
fn test_shell_env_new_with_equals_panics() {
    use crate::eval::ShellEnv;
    let _ = ShellEnv::new(vec![(BString::from("A=B"), BString::from("val"))]);
}

#[test]
fn test_default_shell_variables_align_with_dash() {
    let state = ShellState::new();
    assert_eq!(state.get_var(BStr::new("PATH")), Some(BString::from("/bin:/boot/bin:/boot-bin")));
    assert!(!state.exported().contains(BStr::new("PATH")));

    assert!(state.get_var(BStr::new("PWD")).is_some());
    assert!(state.exported().contains(BStr::new("PWD")));

    assert_eq!(state.get_var(BStr::new("IFS")), Some(BString::from(" \t\n")));
    assert!(!state.exported().contains(BStr::new("IFS")));

    assert_eq!(state.get_var(BStr::new("OPTIND")), Some(BString::from("1")));
    assert!(!state.exported().contains(BStr::new("OPTIND")));

    assert!(state.get_var(BStr::new("PPID")).is_some());
    assert!(state.is_readonly(BStr::new("PPID")));
    assert!(!state.exported().contains(BStr::new("PPID")));

    let expected_ps1 = if unsafe { libc::geteuid() } == 0 { "# " } else { "$ " };
    assert_eq!(state.get_var(BStr::new("PS1")), Some(BString::from(expected_ps1)));
    assert_eq!(state.get_var(BStr::new("PS2")), Some(BString::from("> ")));
    assert_eq!(state.get_var(BStr::new("PS4")), Some(BString::from("+ ")));
}

#[test]
fn test_ifs_join_sep_and_star_at_get_var() {
    let mut state = ShellState::new();
    state.set_args(vec![BString::from("a"), BString::from("b"), BString::from("c")]);

    // Default IFS (" \t\n") -> first byte is ' '
    assert_eq!(state.ifs_join_sep(), Some(b' '));
    assert_eq!(state.get_var(BStr::new("*")), Some(BString::from("a b c")));
    assert_eq!(state.get_var(BStr::new("@")), Some(BString::from("a b c")));

    // Custom IFS (":;") -> first byte is ':' for *, while @ always joins with ' '
    state.set_var(BStr::new("IFS"), BStr::new(":;"));
    assert_eq!(state.ifs_join_sep(), Some(b':'));
    assert_eq!(state.get_var(BStr::new("*")), Some(BString::from("a:b:c")));
    assert_eq!(state.get_var(BStr::new("@")), Some(BString::from("a b c")));

    // Empty IFS ("") -> None for * (concatenated with no separator), while @ joins with ' '
    state.set_var(BStr::new("IFS"), BStr::new(""));
    assert_eq!(state.ifs_join_sep(), None);
    assert_eq!(state.get_var(BStr::new("*")), Some(BString::from("abc")));
    assert_eq!(state.get_var(BStr::new("@")), Some(BString::from("a b c")));

    // Unset IFS -> falls back to ' '
    state.unset_var(BStr::new("IFS"));
    assert_eq!(state.ifs_join_sep(), Some(b' '));
    assert_eq!(state.get_var(BStr::new("*")), Some(BString::from("a b c")));
    assert_eq!(state.get_var(BStr::new("@")), Some(BString::from("a b c")));
}

#[test]
fn test_state_serialization_preserves_frames() {
    use crate::serialization::{Deserialize, Serialize};

    let mut state = ShellState::new();
    state.set_var(BStr::new("SHARED"), BStr::new("global_val"));
    state.set_var(BStr::new("UNSET_IN_INNER"), BStr::new("global_u"));
    state.set_args(vec![BString::from("top1"), BString::from("top2")]);

    let mut outer_locals = FlatMap::new();
    outer_locals.insert(BString::from("SHARED"), Some(BString::from("outer_val")));
    outer_locals.insert(BString::from("OUTER_ONLY"), Some(BString::from("o1")));
    outer_locals.insert(BString::from("UNSET_IN_INNER"), Some(BString::from("outer_u")));
    state.frames.push(Frame {
        local_vars: outer_locals,
        args: vec![BString::from("outer_arg1"), BString::from("outer_arg2")],
    });

    let mut inner_locals = FlatMap::new();
    inner_locals.insert(BString::from("SHARED"), Some(BString::from("inner_val")));
    inner_locals.insert(BString::from("INNER_ONLY"), Some(BString::from("i1")));
    inner_locals.insert(BString::from("UNSET_IN_INNER"), None);
    state.frames.push(Frame { local_vars: inner_locals, args: vec![BString::from("inner_arg1")] });

    let mut buf = Vec::new();
    state.serialize_into(&mut buf);

    let mut offset = 0;
    let restored = ShellState::deserialize(&buf, &mut offset).unwrap();
    assert_eq!(offset, buf.len());
    assert_eq!(restored.frames, state.frames);
    assert_eq!(restored.get_var(BStr::new("SHARED")), Some(BString::from("inner_val")));
    assert_eq!(restored.get_var(BStr::new("OUTER_ONLY")), Some(BString::from("o1")));
    assert_eq!(restored.get_var(BStr::new("INNER_ONLY")), Some(BString::from("i1")));
    assert_eq!(restored.get_var(BStr::new("UNSET_IN_INNER")), None);
    assert_eq!(restored.get_args(), vec![BString::from("inner_arg1")]);
    assert_eq!(restored.get_var(BStr::new("1")), Some(BString::from("inner_arg1")));
    assert_eq!(restored.get_var(BStr::new("#")), Some(BString::from("1")));
    assert_eq!(restored.get_var(BStr::new("$")), state.get_var(BStr::new("$")));
}

#[test]
fn test_local_unset_and_reassignment_scoping() {
    use crate::eval::{EvalOutcome, eval_string};
    use crate::process::{make_pipe, read_fd_to_end};

    let mut state = ShellState::new();
    state.set_and_export_var(BStr::new("X"), BStr::new("global_val"));

    // 1. Direct state manipulation across nested frames
    state.frames.push(Frame { local_vars: FlatMap::new(), args: vec![] });
    state.declare_local(BStr::new("X"), Some(BStr::new("outer_val")));
    assert_eq!(state.get_var(BStr::new("X")), Some(BString::from("outer_val")));

    // Unsetting local X shadows global X (does not reveal global_val) and omits X from vars()
    state.unset_var(BStr::new("X"));
    assert_eq!(state.get_var(BStr::new("X")), None);
    assert!(!state.vars().iter().any(|(k, _)| k == "X"));
    assert_eq!(state.all_vars().get(BStr::new("X")), Some(&BString::from("global_val")));

    // Reassigning X after unset keeps X local to the outer frame
    state.set_var(BStr::new("X"), BStr::new("outer_reassigned"));
    assert_eq!(state.get_var(BStr::new("X")), Some(BString::from("outer_reassigned")));
    assert_eq!(state.all_vars().get(BStr::new("X")), Some(&BString::from("global_val")));

    // Inner frame declares local X, unsets it, and reassigns it without affecting outer or global
    state.frames.push(Frame { local_vars: FlatMap::new(), args: vec![] });
    state.declare_local(BStr::new("X"), Some(BStr::new("inner_val")));
    assert_eq!(state.get_var(BStr::new("X")), Some(BString::from("inner_val")));

    state.unset_var(BStr::new("X"));
    assert_eq!(state.get_var(BStr::new("X")), None);

    state.set_var(BStr::new("X"), BStr::new("inner_reassigned"));
    assert_eq!(state.get_var(BStr::new("X")), Some(BString::from("inner_reassigned")));

    state.frames.pop();
    assert_eq!(state.get_var(BStr::new("X")), Some(BString::from("outer_reassigned")));

    state.frames.pop();
    assert_eq!(state.get_var(BStr::new("X")), Some(BString::from("global_val")));
    assert!(state.vars().iter().any(|(k, v)| k == "X" && v == "global_val"));

    // 2. End-to-end shell evaluation including nested functions and subshell inside function
    let mut ctx = ExecutionContext::initial().unwrap();
    let (out_read, out_write) = make_pipe().unwrap();
    ctx.set_fd(Fd::STDOUT, out_write);

    let script = b"
        x=global
        inner() {
            local x=inner_init
            unset x
            printf 'inner_unset=%s\n' \"${x-UNSET}\"
            x=inner_reassigned
            printf 'inner_reassigned=%s\n' \"$x\"
        }
        outer() {
            local x=outer_init
            unset x
            printf 'outer_unset=%s\n' \"${x-UNSET}\"
            ( printf 'subshell_unset=%s\n' \"${x-UNSET}\" )
            x=outer_reassigned
            inner
            printf 'outer_after_inner=%s\n' \"$x\"
        }
        outer
        printf 'global_after=%s\n' \"$x\"
    ";
    let res = eval_string(script.as_bstr(), &mut state, &mut ctx).unwrap();
    ctx.close_fd(Fd::STDOUT);
    assert_eq!(res, EvalOutcome::Code(0));

    let out = String::from_utf8(read_fd_to_end(out_read).unwrap()).unwrap();
    assert_eq!(
        out,
        "outer_unset=UNSET\n\
         subshell_unset=UNSET\n\
         inner_unset=UNSET\n\
         inner_reassigned=inner_reassigned\n\
         outer_after_inner=outer_reassigned\n\
         global_after=global\n"
    );
}

#[test]
fn test_special_var_dash_and_arg0_and_login_option() {
    let mut state = ShellState::new();
    assert_eq!(state.get_var(BStr::new("-")), Some(BString::from("")));

    // Enabling -I / ignoreeof reflects 'I' in $-
    state.set_option_by_flag(b'I', true).unwrap();
    assert_eq!(state.get_var(BStr::new("-")), Some(BString::from("I")));

    state.set_option_by_name(BStr::new("ignoreeof"), false).unwrap();
    assert_eq!(state.get_var(BStr::new("-")), Some(BString::from("")));

    state.set_option_by_name(BStr::new("ignoreeof"), true).unwrap();
    state.set_option_by_flag(b'e', true).unwrap();
    assert_eq!(state.get_var(BStr::new("-")), Some(BString::from("eI")));

    // set -l and set -o login are accepted as valid runtime options
    assert!(state.set_option_by_flag(b'l', true).is_ok());
    assert!(state.set_option_by_flag(b'l', false).is_ok());
    assert!(state.set_option_by_name(BStr::new("login"), true).is_ok());
    assert!(state.set_option_by_name(BStr::new("login"), false).is_ok());

    // $0 defaults to argv[0] when invoked with -c without command_name operand
    let parsed_c =
        parse_args(&[BString::from("/boot/bin/sh"), BString::from("-c"), BString::from("echo $0")])
            .unwrap();
    let state_c = ShellState::with_args(parsed_c, FlatMap::new()).unwrap();
    assert_eq!(state_c.get_var(BStr::new("0")), Some(BString::from("/boot/bin/sh")));

    // $0 is overridden by command_name operand when provided after -c command_string
    let parsed_c_named = parse_args(&[
        BString::from("/boot/bin/sh"),
        BString::from("-c"),
        BString::from("echo $0"),
        BString::from("custom_cmd_name"),
        BString::from("arg1"),
    ])
    .unwrap();
    let state_c_named = ShellState::with_args(parsed_c_named, FlatMap::new()).unwrap();
    assert_eq!(state_c_named.get_var(BStr::new("0")), Some(BString::from("custom_cmd_name")));
    assert_eq!(state_c_named.get_var(BStr::new("1")), Some(BString::from("arg1")));

    // $0 defaults to argv[0] when invoked with -s
    let parsed_s =
        parse_args(&[BString::from("/boot/bin/zxsh"), BString::from("-s"), BString::from("pos1")])
            .unwrap();
    let state_s = ShellState::with_args(parsed_s, FlatMap::new()).unwrap();
    assert_eq!(state_s.get_var(BStr::new("0")), Some(BString::from("/boot/bin/zxsh")));
    assert_eq!(state_s.get_var(BStr::new("1")), Some(BString::from("pos1")));
}
