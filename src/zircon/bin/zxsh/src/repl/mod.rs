// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::eval::{
    EXIT_FAILURE, EXIT_SYNTAX_ERROR, EvalOutcome, ExecutionContext, ShellState, eval_command,
    expand_prompt, run_exit_trap,
};
use crate::parser::ast::ASTBuilder;
use crate::parser::{ParseError, parse_script, tokenize};
use crate::tty::ShellSignals;
use bstr::{BStr, BString, ByteSlice, ByteVec};
use line_editor::{Config, Editor, ReadlineError};
use std::io::{BufRead, IsTerminal, Write};

mod completion;

const DEFAULT_PS1: &str = "$ ";
const DEFAULT_PS2: &str = "> ";

fn get_prompt(input_buffer: &BStr, state: &mut ShellState, ctx: &ExecutionContext) -> BString {
    let prompt_var =
        if input_buffer.is_empty() { bstr::BStr::new(b"PS1") } else { bstr::BStr::new(b"PS2") };
    let default_prompt =
        if input_buffer.is_empty() { BStr::new(DEFAULT_PS1) } else { BStr::new(DEFAULT_PS2) };
    expand_prompt(prompt_var, default_prompt, state, ctx)
}

/// Creates an interactive line editor configured for zxsh.
fn create_editor() -> Editor {
    Editor::with_config(Config { max_history_len: 100, ..Default::default() })
}

/// Starts the read-eval-print loop (REPL), using `line-editor` for command history and
/// autocompletion when `stdin` is a terminal.
pub fn run_repl(state: ShellState) -> i32 {
    let stdin = std::io::stdin();
    let mut editor = stdin.is_terminal().then(create_editor);
    run_repl_loop(stdin.lock(), line_editor::UnbufferedStdout, state, editor.as_mut())
}

fn run_repl_loop<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
    mut state: ShellState,
    mut editor: Option<&mut Editor>,
) -> i32 {
    let has_editor = editor.is_some();
    if has_editor {
        state.opt_interactive = true;
    }
    let mut ctx = match ExecutionContext::initial() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Context error: {}", e);
            return EXIT_FAILURE;
        }
    };

    let mut input_buffer = BString::default();
    let mut numeof = 0;

    'repl_loop: loop {
        ctx.signal_state.clear(ShellSignals::INT);

        let line = if let Some(ref mut ed) = editor {
            let prompt = get_prompt(input_buffer.as_ref(), &mut state, &ctx);
            let path = state.path();
            ed.set_completion_handler(move |line| completion::tab_complete(line, &path));
            let mode = ed.config.resolve_operating_mode(|| true);
            let read_result = ed.readline_from(&mut reader, &mut writer, mode, prompt.as_bstr());
            let mut line = match read_result {
                Ok(l) => {
                    numeof = 0;
                    l
                }
                Err(ReadlineError::Interrupted) => {
                    ctx.signal_state.clear(ShellSignals::INT);
                    state.set_last_status(130);
                    input_buffer.clear();
                    continue;
                }
                Err(ReadlineError::Eof) => {
                    if ctx.signal_state.is_pending(ShellSignals::INT) {
                        ctx.signal_state.clear(ShellSignals::INT);
                        state.set_last_status(130);
                        input_buffer.clear();
                        continue;
                    }
                    if input_buffer.trim_ascii().is_empty() && state.opt_ignoreeof && numeof < 10 {
                        numeof += 1;
                        let _ = writeln!(writer, "Use \"exit\" to leave shell.");
                        continue;
                    }
                    break 'repl_loop;
                }
                Err(ReadlineError::Io(e)) => {
                    eprintln!("Read line error: {}", e);
                    break 'repl_loop;
                }
            };

            ed.add_history(line.as_bstr());

            line.push_byte(b'\n');
            line
        } else {
            let mut l = Vec::new();
            match reader.read_until(b'\n', &mut l) {
                Ok(0) => break 'repl_loop,
                Ok(_) => BString::from(l),
                Err(e) => {
                    eprintln!("Read line error: {}", e);
                    break 'repl_loop;
                }
            }
        };

        if state.opt_verbose && !has_editor {
            if let Some(mut err) = ctx.stderr() {
                let _ = err.write_all(line.as_bytes());
                let _ = err.flush();
            }
        }

        let sigint = ctx.signal_state.is_pending(ShellSignals::INT);
        ctx.signal_state.clear(ShellSignals::INT);
        if sigint || line.as_bytes().contains(&b'\x03') {
            println!();
            state.set_last_status(130);
            input_buffer.clear();
            continue;
        }

        input_buffer.extend_from_slice(&line);
        let trimmed = input_buffer.trim_ascii();
        if trimmed.is_empty() {
            input_buffer.clear();
            continue;
        }
        let mut builder = ASTBuilder::new();
        let cmds = match tokenize(input_buffer.as_bytes())
            .and_then(|tokens| parse_script(&mut builder, &tokens))
        {
            Ok(c) => c,
            Err(ParseError::Incomplete(_)) => {
                continue;
            }
            Err(ParseError::Syntax(e)) => {
                eprintln!("Parse error: {}", e);
                state.set_last_status(EXIT_SYNTAX_ERROR);
                input_buffer.clear();
                if !state.opt_interactive {
                    break 'repl_loop;
                }
                continue;
            }
        };
        input_buffer.clear();
        for &cmd_offset in &cmds {
            match eval_command(&mut builder, cmd_offset, &mut state, &mut ctx) {
                Ok(EvalOutcome::Code(c)) => {
                    state.set_last_status(c);
                }
                Ok(EvalOutcome::Exit(c) | EvalOutcome::Return(c)) => {
                    return run_exit_trap(&mut state, &mut ctx, c);
                }
                Ok(EvalOutcome::Break(_)) => {
                    eprintln!("zxsh: break: can only break from a loop");
                    state.set_last_status(EXIT_SYNTAX_ERROR);
                }
                Ok(EvalOutcome::Continue(_)) => {
                    eprintln!("zxsh: continue: can only continue from a loop");
                    state.set_last_status(EXIT_SYNTAX_ERROR);
                }
                Err(e) => {
                    if !e.is_empty() {
                        eprintln!("Eval error: {}", e);
                    }
                    let code = match state.last_status() {
                        0 => EXIT_FAILURE,
                        c => c,
                    };
                    state.set_last_status(code);
                    if !state.opt_interactive {
                        break 'repl_loop;
                    }
                    break;
                }
            }
        }
    }
    if !input_buffer.trim_ascii().is_empty() {
        eprintln!("zxsh: syntax error: unexpected end of file");
        state.set_last_status(EXIT_SYNTAX_ERROR);
    }
    let exit_status = state.last_status();
    run_exit_trap(&mut state, &mut ctx, exit_status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn run_repl_reader<R: BufRead>(reader: R, state: ShellState, is_tty: bool) -> i32 {
        run_repl_stream(reader, line_editor::UnbufferedStdout, state, is_tty)
    }

    fn run_repl_stream<R: BufRead, W: Write>(
        reader: R,
        writer: W,
        state: ShellState,
        is_tty: bool,
    ) -> i32 {
        let mut editor = if is_tty { Some(create_editor()) } else { None };
        run_repl_loop(reader, writer, state, editor.as_mut())
    }

    #[test]
    fn test_get_prompt_defaults() {
        let mut state = ShellState::new();
        let ctx = ExecutionContext::initial().unwrap();

        assert_eq!(get_prompt(BStr::new(""), &mut state, &ctx), BStr::new(DEFAULT_PS1));
        assert_eq!(get_prompt(BStr::new("input"), &mut state, &ctx), BStr::new(DEFAULT_PS2));
    }

    #[test]
    fn test_get_prompt_custom_vars() {
        let mut state = ShellState::new();
        state.set_var("PS1", "MYPROMPT> ");
        state.set_var("PS2", "CONTINUE> ");
        let ctx = ExecutionContext::initial().unwrap();

        assert_eq!(get_prompt(BStr::new(""), &mut state, &ctx), BStr::new("MYPROMPT> "));
        assert_eq!(get_prompt(BStr::new("line1"), &mut state, &ctx), BStr::new("CONTINUE> "));
    }

    #[test]
    fn test_get_prompt_expansion_error() {
        let mut state = ShellState::new();
        state.set_var("PS1", "$(( 1 / 0 ))");
        let ctx = ExecutionContext::initial().unwrap();

        // Expands to error -> falls back to default prompt DEFAULT_PS1
        assert_eq!(get_prompt(BStr::new(""), &mut state, &ctx), BStr::new(DEFAULT_PS1));
    }

    #[test]
    fn test_run_repl_reader_execution() {
        let input = "x=10\nexport Y=$x\n\n  \nbreak\ncontinue\nexit 5\n";
        let cursor = Cursor::new(input);
        let res = run_repl_reader(cursor, ShellState::new(), false);
        assert_eq!(res, 5);
    }

    #[test]
    fn test_run_repl_reader_incomplete_and_errors() {
        // In interactive mode (-i), syntax errors continue to next line, and incomplete input at EOF returns 2.
        let input = "export Y='multi\nline'\nif; then\n'unterminated quote\n";
        let cursor = Cursor::new(input);
        let mut state = ShellState::new();
        state.opt_interactive = true;
        let res = run_repl_reader(cursor, state, false);
        assert_eq!(res, EXIT_SYNTAX_ERROR);
    }

    #[test]
    fn test_run_repl_reader_non_interactive_options_and_eof_status() {
        // 1. Non-TTY without -i: opt_interactive is false ($- does not contain 'i').
        let input = "case $- in *i*) exit 99 ;; esac\n";
        let res = run_repl_reader(Cursor::new(input), ShellState::new(), false);
        assert_eq!(res, 0);

        // 2. Non-TTY without -i: set -n (noexec) is honored because shell is non-interactive.
        let input = "set -n\nexit 88\n";
        let res = run_repl_reader(Cursor::new(input), ShellState::new(), false);
        assert_eq!(res, 0);

        // 3. Non-TTY EOF without explicit exit returns last command's exit status ($?).
        let res = run_repl_reader(Cursor::new("false\n"), ShellState::new(), false);
        assert_eq!(res, 1);

        let res = run_repl_reader(Cursor::new("(exit 42)\n"), ShellState::new(), false);
        assert_eq!(res, 42);

        // 4. Non-TTY with -i (opt_interactive = true before calling run_repl_reader): opt_interactive stays true.
        let mut state = ShellState::new();
        state.opt_interactive = true;
        let input = "case $- in *i*) (exit 33) ;; *) exit 99 ;; esac\n";
        let res = run_repl_reader(Cursor::new(input), state, false);
        assert_eq!(res, 33);
    }

    #[test]
    fn test_run_repl_reader_incomplete_at_eof_and_non_interactive_errors() {
        // 1. Incomplete compound command at EOF returns EXIT_SYNTAX_ERROR (2).
        let res = run_repl_reader(Cursor::new("if true; then\n"), ShellState::new(), false);
        assert_eq!(res, EXIT_SYNTAX_ERROR);

        // 2. Unclosed quote without trailing newline at EOF returns EXIT_SYNTAX_ERROR (2).
        let res = run_repl_reader(Cursor::new("echo \"unclosed"), ShellState::new(), false);
        assert_eq!(res, EXIT_SYNTAX_ERROR);

        // 3. Syntax error in non-interactive mode immediately terminates with EXIT_SYNTAX_ERROR (2)
        // without executing subsequent lines.
        let res = run_repl_reader(Cursor::new("if; then\nexit 0\n"), ShellState::new(), false);
        assert_eq!(res, EXIT_SYNTAX_ERROR);

        // 4. Eval error (e.g. readonly violation) in non-interactive mode immediately terminates
        // with non-zero status without executing subsequent lines.
        let res = run_repl_reader(
            Cursor::new("RO=1\nreadonly RO\nRO=2\nexit 0\n"),
            ShellState::new(),
            false,
        );
        assert_eq!(res, EXIT_FAILURE);

        // 5. Eval error in interactive mode sets non-zero status and continues to subsequent lines.
        let mut state = ShellState::new();
        state.opt_interactive = true;
        let res = run_repl_reader(Cursor::new("RO=1\nreadonly RO\nRO=2\nexit 7\n"), state, false);
        assert_eq!(res, 7);
    }

    #[test]
    fn test_run_repl_reader_verbose() {
        let input = "x=10\nexit 0\n";
        let cursor = Cursor::new(input);
        let mut state = ShellState::new();
        state.opt_verbose = true;
        let res = run_repl_reader(cursor, state, false);
        assert_eq!(res, 0);
    }

    #[test]
    fn test_repl_interactive_execution_and_history() {
        let input = "x=100\nexport VAL=$x\nexit 42\n";
        let cursor = Cursor::new(input.as_bytes());
        let mut output = Vec::new();
        let mut editor = Editor::with_config(
            Config::default()
                .with_terminal_mode(line_editor::TerminalMode::Tty)
                .with_column_width(line_editor::ColumnWidth::Fixed(80)),
        );

        let res = run_repl_loop(cursor, &mut output, ShellState::new(), Some(&mut editor));
        assert_eq!(res, 42);

        let history_entries: Vec<&str> =
            editor.history().entries().iter().map(|e| e.to_str().unwrap()).collect();
        assert_eq!(history_entries, vec!["x=100", "export VAL=$x", "exit 42"]);
    }

    #[test]
    fn test_repl_interactive_ctrl_d_eof() {
        let cursor = Cursor::new(b"");
        let mut output = Vec::new();
        let mut editor = Editor::with_config(
            Config::default()
                .with_terminal_mode(line_editor::TerminalMode::Tty)
                .with_column_width(line_editor::ColumnWidth::Fixed(80)),
        );

        let res = run_repl_loop(cursor, &mut output, ShellState::new(), Some(&mut editor));
        assert_eq!(res, 0);
        assert!(editor.history().entries().is_empty());

        // Interactive EOF after a failing command returns that command's exit status and has 'i' in $-.
        let input = b"case $- in *i*) (exit 19) ;; *) (exit 99) ;; esac\n";
        let res =
            run_repl_loop(Cursor::new(input), &mut output, ShellState::new(), Some(&mut editor));
        assert_eq!(res, 19);
    }

    #[test]
    fn test_repl_interactive_ignoreeof() {
        let cursor = Cursor::new(b"");
        let mut output = Vec::new();
        let mut editor = Editor::with_config(
            Config::default()
                .with_terminal_mode(line_editor::TerminalMode::Tty)
                .with_column_width(line_editor::ColumnWidth::Fixed(80)),
        );
        let mut state = ShellState::new();
        state.opt_ignoreeof = true;

        let res = run_repl_loop(cursor, &mut output, state, Some(&mut editor));
        assert_eq!(res, 0);
        let out_str = String::from_utf8_lossy(&output);
        assert!(out_str.contains("Use \"exit\" to leave shell."));
    }

    #[test]
    fn test_repl_interactive_multiline_ps2() {
        let input = "if true; then\nexport FOO=BAR\nfi\nexit 7\n";
        let cursor = Cursor::new(input.as_bytes());
        let mut output = Vec::new();
        let mut editor = Editor::with_config(
            Config::default()
                .with_terminal_mode(line_editor::TerminalMode::Tty)
                .with_column_width(line_editor::ColumnWidth::Fixed(80)),
        );

        let res = run_repl_loop(cursor, &mut output, ShellState::new(), Some(&mut editor));
        assert_eq!(res, 7);

        let history_entries: Vec<&str> =
            editor.history().entries().iter().map(|e| e.to_str().unwrap()).collect();
        assert_eq!(history_entries, vec!["if true; then", "export FOO=BAR", "fi", "exit 7"]);
    }

    #[test]
    fn test_repl_interactive_status_codes() {
        let input = "false\nexit $?\n";
        let cursor = Cursor::new(input.as_bytes());
        let mut output = Vec::new();
        let mut editor = Editor::with_config(
            Config::default()
                .with_terminal_mode(line_editor::TerminalMode::Tty)
                .with_column_width(line_editor::ColumnWidth::Fixed(80)),
        );

        let res = run_repl_loop(cursor, &mut output, ShellState::new(), Some(&mut editor));
        assert_eq!(res, 1);
    }

    #[test]
    fn test_repl_interactive_completion_integration() {
        let temp_dir = std::env::temp_dir();
        let test_dir = temp_dir.join("zxsh_repl_complete_test");
        let _ = std::fs::create_dir_all(&test_dir);
        let f1 = test_dir.join("cmd_alpha");
        let f2 = test_dir.join("cmd_beta");
        let _ = std::fs::write(&f1, "1");
        let _ = std::fs::write(&f2, "2");

        let mut state = ShellState::new();
        state.set_var("PATH", test_dir.to_str().unwrap());

        let path = state.path();
        let comps = completion::tab_complete(BStr::new("cmd_"), &path);
        assert_eq!(comps.len(), 2);
        assert!(comps.contains(&BString::from("cmd_alpha")));
        assert!(comps.contains(&BString::from("cmd_beta")));

        let _ = std::fs::remove_file(f1);
        let _ = std::fs::remove_file(f2);
        let _ = std::fs::remove_dir(test_dir);
    }

    #[test]
    fn test_repl_uart_execution() {
        let input = "x=42\nexit $x\n";
        let cursor = Cursor::new(input.as_bytes());
        let mut output = Vec::new();
        let mut editor = Editor::with_config(Config::default().with_term_name(Some("uart")));

        let res = run_repl_loop(cursor, &mut output, ShellState::new(), Some(&mut editor));
        assert_eq!(res, 42);

        let out_str = String::from_utf8_lossy(&output);
        // In UartEcho mode, prompt is written and characters are echoed back.
        assert!(out_str.contains("$ "));
        assert!(out_str.contains("x=42"));
        assert!(out_str.contains("exit $x"));

        let history_entries: Vec<&str> =
            editor.history().entries().iter().map(|e| e.to_str().unwrap()).collect();
        assert_eq!(history_entries, vec!["x=42", "exit $x"]);
    }

    #[test]
    fn test_repl_dumb_terminal() {
        let input = "echo hello\nexit 0\n";
        let cursor = Cursor::new(input.as_bytes());
        let mut output = Vec::new();
        let mut editor = Editor::with_config(Config::default().with_term_name(Some("dumb")));

        let res = run_repl_loop(cursor, &mut output, ShellState::new(), Some(&mut editor));
        assert_eq!(res, 0);

        let out_str = String::from_utf8_lossy(&output);
        // Prompt is rendered, but PromptOnly mode does not echo typed characters.
        assert!(out_str.contains("$ "));
        assert!(!out_str.contains("echo hello"));
    }

    #[test]
    fn test_repl_interactive_ctrl_c_interrupted() {
        let input = b"\x03exit 42\n";
        let cursor = Cursor::new(input);
        let mut output = Vec::new();
        let mut editor = Editor::with_config(
            Config::default()
                .with_terminal_mode(line_editor::TerminalMode::Tty)
                .with_column_width(line_editor::ColumnWidth::Fixed(80)),
        );

        let state = ShellState::new();
        let res = run_repl_loop(cursor, &mut output, state, Some(&mut editor));
        assert_eq!(res, 42);
    }

    #[test]
    fn test_repl_interactive_incomplete_eof_and_stdin_process() {
        // 1. Interactive mode hitting EOF while input_buffer is incomplete returns EXIT_SYNTAX_ERROR (2),
        // even when ignoreeof is enabled.
        let mut output = Vec::new();
        let mut editor = Editor::with_config(
            Config::default()
                .with_terminal_mode(line_editor::TerminalMode::Tty)
                .with_column_width(line_editor::ColumnWidth::Fixed(80)),
        );
        let mut state = ShellState::new();
        state.opt_ignoreeof = true;
        let res =
            run_repl_loop(Cursor::new(b"if true; then\n"), &mut output, state, Some(&mut editor));
        assert_eq!(res, EXIT_SYNTAX_ERROR);

        // 2. End-to-end process test: piped stdin to `/pkg/bin/zxsh` and `/pkg/bin/zxsh -s` exits with
        // the last command's status on EOF and does not set `-i`.
        let mut state = ShellState::new();
        let mut ctx = ExecutionContext::initial().unwrap();
        let outcome = crate::eval::eval_string(
            BStr::new("printf 'case $- in *i*) exit 99 ;; esac; (exit 37)\\n' | /pkg/bin/zxsh"),
            &mut state,
            &mut ctx,
        )
        .unwrap();
        assert_eq!(outcome, EvalOutcome::Code(37));

        let outcome_s = crate::eval::eval_string(
            BStr::new("printf '(exit $1)\\n' | /pkg/bin/zxsh -s 29"),
            &mut state,
            &mut ctx,
        )
        .unwrap();
        assert_eq!(outcome_s, EvalOutcome::Code(29));
    }

    #[test]
    fn test_repl_exit_trap_semantics() {
        // 1. `trap 'exit 99' EXIT` overrides exit status on both EOF and explicit `exit 5`.
        let res_eof = run_repl_reader(
            Cursor::new("trap 'exit 99' EXIT\n(exit 7)\n"),
            ShellState::new(),
            false,
        );
        assert_eq!(res_eof, 99);

        let res_explicit =
            run_repl_reader(Cursor::new("trap 'exit 99' EXIT\nexit 5\n"), ShellState::new(), false);
        assert_eq!(res_explicit, 99);

        // 2. `trap 'false' EXIT` (without `exit`) preserves the pre-trap exit status.
        let res_preserve_explicit =
            run_repl_reader(Cursor::new("trap 'false' EXIT\nexit 5\n"), ShellState::new(), false);
        assert_eq!(res_preserve_explicit, 5);

        let res_preserve_eof =
            run_repl_reader(Cursor::new("trap 'false' EXIT\n(exit 7)\n"), ShellState::new(), false);
        assert_eq!(res_preserve_eof, 7);

        // 3. `trap 'echo status=$?; exit' EXIT` sees `$?` equal to the pre-trap exit status and
        // bare `exit` inside the trap exits with that pre-trap status even after `echo` runs.
        let res_bare_exit_eof = run_repl_reader(
            Cursor::new(
                "trap 'case $? in 7) echo status=$?; exit ;; *) exit 88 ;; esac' EXIT\n(exit 7)\n",
            ),
            ShellState::new(),
            false,
        );
        assert_eq!(res_bare_exit_eof, 7);

        let res_bare_exit_explicit = run_repl_reader(
            Cursor::new("trap 'false; exit' EXIT\nexit 13\n"),
            ShellState::new(),
            false,
        );
        assert_eq!(res_bare_exit_explicit, 13);

        // 4. `eval` syntax error sets EXIT_SYNTAX_ERROR (2), which is preserved on non-interactive Err(e).
        let res_eval_syn =
            run_repl_reader(Cursor::new("eval 'if; then'\nexit 0\n"), ShellState::new(), false);
        assert_eq!(res_eval_syn, EXIT_SYNTAX_ERROR);
    }
}
