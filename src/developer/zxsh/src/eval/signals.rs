// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::eval::{EvalOutcome, ExecutionContext, ShellState, eval_string};
use crate::tty::ShellSignals;
use bstr::BStr;

pub fn run_pending_traps(
    state: &mut ShellState,
    ctx: &mut ExecutionContext,
) -> Result<Option<EvalOutcome>, String> {
    let pending = ctx.signal_state.take_pending();
    if pending.is_empty() {
        return Ok(None);
    }
    for &(sig, sig_name) in ShellSignals::ALL {
        if pending.contains(sig) {
            if let Some(action) = state.traps.get(BStr::new(sig_name)).cloned() {
                if action.is_empty() {
                    continue;
                }
                let saved_status = state.last_status();
                let prev_trap_status = state.set_trap_exit_status(Some(saved_status));
                let outcome_res = eval_string(action.as_ref(), state, ctx);
                state.set_trap_exit_status(prev_trap_status);
                let outcome = outcome_res?;
                if !matches!(outcome, EvalOutcome::Code(_)) {
                    return Ok(Some(outcome));
                }
                state.set_last_status(saved_status);
            } else if sig == ShellSignals::INT && state.opt_interactive {
                return Err("".to_string());
            } else if let Some(exit_code) = sig.exit_code() {
                return Ok(Some(EvalOutcome::Exit(exit_code)));
            }
        }
    }
    Ok(None)
}

pub fn run_exit_trap(state: &mut ShellState, ctx: &mut ExecutionContext, exit_status: i32) -> i32 {
    state.set_last_status(exit_status);
    if let Some(action) = state.traps.get(BStr::new(b"EXIT")).cloned()
        && !action.is_empty()
    {
        let prev_trap_status = state.set_trap_exit_status(Some(exit_status));
        let res = eval_string(action.as_ref(), state, ctx);
        state.set_trap_exit_status(prev_trap_status);
        match res {
            Ok(EvalOutcome::Exit(code) | EvalOutcome::Return(code)) => {
                state.set_last_status(code);
                return code;
            }
            Ok(_) => {}
            Err(err) => {
                eprintln!("trap EXIT error: {}", err);
            }
        }
    }
    state.set_last_status(exit_status);
    exit_status
}
