// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::execution_context::ExecutionContext;
use super::expand::{
    ExpandedCommand, expand_alias, expand_argument, expand_assignment_value,
    get_literal_command_name,
};
use super::spawn::{SpawnedProcess, spawn_external_command, wait_for_process_to_exit};
use super::state::{Frame, ShellState, StateBackupGuard};
use super::{EvalOutcome, eval_command};
use crate::builtins::{is_builtin, is_special_builtin};
use crate::collections::{FlatMap, FlatSet};
use crate::parser::ast::{ASTBuilder, Command, WordPart, WordPartTag};
use crate::relative;
use bstr::{BStr, BString, ByteSlice};

pub fn split_simple_command_args(
    cmd: &Command,
    buf: &relative::Buffer,
) -> (Vec<relative::Slice<WordPart>>, Vec<relative::Slice<WordPart>>) {
    let mut assignments_refs = Vec::new();
    let mut cmd_args_refs = Vec::new();
    let mut parsing_assignments = true;
    for &arg_slice in cmd.simple_args.as_slice(buf) {
        let parts = arg_slice.as_slice(buf);
        if parts.is_empty() {
            cmd_args_refs.push(relative::Slice::empty());
            parsing_assignments = false;
        } else if parsing_assignments && is_assignment_flat(parts, buf) {
            assignments_refs.push(arg_slice);
        } else {
            parsing_assignments = false;
            cmd_args_refs.push(arg_slice);
        }
    }
    (assignments_refs, cmd_args_refs)
}

pub fn parse_simple_command_args<'a>(
    builder: &'a ASTBuilder,
    cmd_ptr: relative::Ptr<Command>,
) -> (Vec<relative::Slice<WordPart>>, Vec<relative::Slice<WordPart>>) {
    split_simple_command_args(builder.get_ref(cmd_ptr), builder)
}

pub fn apply_assignments<'a>(
    builder: &ASTBuilder,
    assignments_refs: &[relative::Slice<WordPart>],
    state: &'a mut ShellState,
    ctx: &ExecutionContext,
) -> Result<StateBackupGuard<'a>, String> {
    let mut guard = StateBackupGuard::new(state);
    for &arg_slice in assignments_refs {
        let assoc = builder.get_slice(arg_slice);
        let (name, val_start, remaining) = split_assignment_flat(assoc, builder);
        let expanded_val =
            expand_assignment_value(val_start, remaining, &mut *guard.state, ctx, builder)?;
        if guard.state.is_readonly(name) {
            return Err(format!("{}: is read only", name));
        }
        guard.backup_var(name);
        guard.state.set_and_export_var(name, &expanded_val);
    }
    Ok(guard)
}

pub enum ResolvedAlias {
    Words(Vec<relative::Slice<WordPart>>),
    Command(relative::Ptr<Command>),
}

pub fn resolve_alias_loop(
    builder: &mut ASTBuilder,
    initial_args: Vec<relative::Slice<WordPart>>,
    state: &mut ShellState,
    ctx: &mut ExecutionContext,
) -> Result<ResolvedAlias, String> {
    let mut cmd_args = initial_args;
    if let Some(expanded) = expand_alias(builder, &cmd_args, state, ctx, &mut FlatSet::new())? {
        match expanded {
            ExpandedCommand::Words(new_args) => {
                cmd_args = new_args;
            }
            ExpandedCommand::Command(new_cmd_ptr) => {
                return Ok(ResolvedAlias::Command(new_cmd_ptr));
            }
        }
    }
    Ok(ResolvedAlias::Words(cmd_args))
}

pub fn expand_command_and_env<'a>(
    builder: &ASTBuilder,
    assignments_refs: &[relative::Slice<WordPart>],
    cmd_args: &[relative::Slice<WordPart>],
    state: &'a mut ShellState,
    ctx: &ExecutionContext,
) -> Result<(StateBackupGuard<'a>, Vec<BString>), String> {
    let guard = apply_assignments(builder, assignments_refs, state, ctx)?;
    let mut expanded_args = Vec::new();
    for &arg_slice in cmd_args {
        let arg = builder.get_slice(arg_slice);
        expanded_args.extend(expand_argument(arg, &mut *guard.state, ctx, builder)?);
    }
    Ok((guard, expanded_args))
}

fn eval_function_call(
    func_bytes: &[u8],
    cmd_name: &BString,
    args: &[BString],
    guard: StateBackupGuard<'_>,
    ctx: &mut ExecutionContext,
) -> Result<EvalOutcome, String> {
    let mut func_builder = ASTBuilder::new();
    let root_cmd_ptr = func_builder.import_serialized_ast(func_bytes);

    let prev_args = std::mem::replace(&mut guard.state.args, args.to_vec());
    guard.state.frames.push(Frame { local_vars: FlatMap::new(), args: guard.state.args.clone() });
    let prev_script_name = std::mem::replace(&mut guard.state.script_name, cmd_name.clone());
    let prev_loop_nest = std::mem::replace(&mut guard.state.loop_nest, 0);

    let res = eval_command(&mut func_builder, root_cmd_ptr, &mut *guard.state, ctx);

    guard.state.loop_nest = prev_loop_nest;
    guard.state.frames.pop();
    guard.state.args = prev_args;
    guard.state.script_name = prev_script_name;

    let final_res = match res {
        Ok(EvalOutcome::Return(code)) => Ok(EvalOutcome::Code(code)),
        other => other,
    };

    if let Ok(code) = &final_res {
        let exit_code = code.exit_code();
        if matches!(code, EvalOutcome::Code(_)) {
            guard.state.set_last_status(exit_code);
        }
        if guard.state.opt_errexit && exit_code != 0 && guard.state.ignore_err_depth == 0 {
            return Ok(EvalOutcome::Exit(exit_code));
        }
    }

    final_res
}

pub fn eval_simple(
    builder: &mut ASTBuilder,
    cmd_ptr: relative::Ptr<Command>,
    state: &mut ShellState,
    ctx: &mut ExecutionContext,
) -> Result<EvalOutcome, String> {
    let (assignments_refs, cmd_args_refs) = parse_simple_command_args(builder, cmd_ptr);

    if cmd_args_refs.is_empty() {
        apply_assignments(builder, &assignments_refs, state, ctx)?.commit();
        let code = state.take_cmd_sub_status().unwrap_or(0);
        state.set_last_status(code);
        return Ok(state.handle_outcome(EvalOutcome::Code(code)));
    }

    let cmd_args = match resolve_alias_loop(builder, cmd_args_refs, state, ctx)? {
        ResolvedAlias::Command(new_cmd_ptr) => {
            let cmd_args_refs = parse_simple_command_args(builder, cmd_ptr).1;
            let name_opt = {
                let arg0 = builder.get_slice(cmd_args_refs[0]);
                get_literal_command_name(arg0, builder)
            };
            let guard = apply_assignments(builder, &assignments_refs, state, ctx)?;
            guard.state.take_cmd_sub_status();
            if let Some(name) = name_opt {
                ctx.active_aliases.insert(name.clone());
                let res = eval_command(builder, new_cmd_ptr, &mut *guard.state, ctx);
                ctx.active_aliases.remove(&name);
                return res;
            } else {
                return eval_command(builder, new_cmd_ptr, &mut *guard.state, ctx);
            }
        }
        ResolvedAlias::Words(words) => words,
    };

    let (mut guard, expanded_args) =
        expand_command_and_env(builder, &assignments_refs, &cmd_args, state, ctx)?;

    if expanded_args.is_empty() {
        guard.commit();
        let code = guard.state.take_cmd_sub_status().unwrap_or(0);
        guard.state.set_last_status(code);
        return Ok(guard.state.handle_outcome(EvalOutcome::Code(code)));
    }

    guard.state.take_cmd_sub_status();

    let cmd_name = &expanded_args[0];
    if is_builtin(cmd_name.as_bstr()) {
        let is_special = is_special_builtin(cmd_name.as_bstr());
        let needs_exported_env_during_builtin =
            cmd_name.as_bytes() == b"exec" && expanded_args.len() > 1;
        if is_special && !needs_exported_env_during_builtin {
            guard.commit();
        }
        let res = crate::builtins::run_builtin(
            cmd_name.as_bstr(),
            &expanded_args[1..],
            &mut *guard.state,
            ctx,
        );
        if is_special && needs_exported_env_during_builtin {
            guard.commit();
        }
        if let Ok(code) = &res {
            let exit_code = code.exit_code();
            if matches!(code, EvalOutcome::Code(_)) {
                guard.state.set_last_status(exit_code);
            }
            if guard.state.opt_errexit && exit_code != 0 && guard.state.ignore_err_depth == 0 {
                drop(guard);
                return Ok(EvalOutcome::Exit(exit_code));
            }
        }
        return res.map(|outcome| guard.state.handle_outcome(outcome));
    }

    if let Some(func_bytes) = guard.state.get_function(cmd_name).cloned() {
        return eval_function_call(&func_bytes, cmd_name, &expanded_args[1..], guard, ctx);
    }

    let spawned = spawn_external_command(expanded_args, &mut *guard.state, ctx);
    drop(guard);

    let exit_code = match spawned {
        SpawnedProcess::Running(proc) => wait_for_process_to_exit(&proc, ctx)?,
        SpawnedProcess::Failed(code) => code,
    };
    state.set_last_status(exit_code);
    Ok(state.handle_outcome(EvalOutcome::Code(exit_code)))
}

/// Checks if an argument word represents a valid variable assignment prefix (e.g. `VAR=val`).
pub fn is_assignment_flat(arg: &[WordPart], buf: &relative::Buffer) -> bool {
    if arg.is_empty() {
        return false;
    }
    if arg[0].tag == WordPartTag::LITERAL {
        let string = arg[0].text.as_bstr(buf);
        if let Some(pos) = string.as_bytes().iter().position(|&b| b == b'=') {
            let name = &string.as_bytes()[..pos];
            if name.is_empty() {
                return false;
            }
            let mut bytes = name.iter();
            let &first = bytes.next().unwrap();
            return (first.is_ascii_alphabetic() || first == b'_')
                && bytes.all(|&character| character.is_ascii_alphanumeric() || character == b'_');
        }
    }
    false
}

pub fn split_assignment_flat<'a>(
    arg: &'a [WordPart],
    buf: &'a relative::Buffer,
) -> (&'a BStr, &'a BStr, &'a [WordPart]) {
    let string = arg[0].text.as_bstr(buf);
    let pos = string.as_bytes().iter().position(|&b| b == b'=').unwrap();
    let name = BStr::new(&string.as_bytes()[..pos]);
    let val_start = BStr::new(&string.as_bytes()[pos + 1..]);
    (name, val_start, &arg[1..])
}
