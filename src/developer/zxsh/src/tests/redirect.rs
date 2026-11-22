// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::eval::testing::{HEREDOC_INLINE_THRESHOLD, apply_redirects};
use crate::eval::{EvalOutcome, ExecutionContext, ShellState, eval_redirect, eval_string};
use crate::fd::Fd;
use crate::parser::ast::{ASTBuilder, Redirect, RedirectTag, RedirectTemplate, ResolvedWordPart};
use crate::relative;
use bstr::{BStr, BString};
use std::fs;

#[test]
fn test_redirect_to_dev_null() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let mut builder = ASTBuilder::new();
    let parts = vec![ResolvedWordPart::Literal(BString::from("/dev/null"))];
    let w_slice = builder.add_resolved_word(&parts);

    let redirect = Redirect {
        tag: RedirectTag::TO_FILE,
        src_fd: Fd(1),
        dest_fd: Fd(0),
        filename: w_slice,
        append: 0,
        clobber: 1,
        expand: 0,
        body: relative::BStr::empty(),
    };

    apply_redirects(&[redirect], &mut state, &mut ctx, &builder).unwrap();
    assert!(ctx.stdout().is_some());
}

#[test]
fn test_redirect_from_dev_null() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let mut builder = ASTBuilder::new();
    let parts = vec![ResolvedWordPart::Literal(BString::from("/dev/null"))];
    let w_slice = builder.add_resolved_word(&parts);

    let redirect = Redirect {
        tag: RedirectTag::FROM_FILE,
        src_fd: Fd(0),
        dest_fd: Fd(0),
        filename: w_slice,
        append: 0,
        clobber: 0,
        expand: 0,
        body: relative::BStr::empty(),
    };

    apply_redirects(&[redirect], &mut state, &mut ctx, &builder).unwrap();
    assert!(ctx.stdin().is_some());
}

#[test]
fn test_redirect_dup_and_close_fd() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let dup_redirect = Redirect {
        tag: RedirectTag::DUP_FD,
        src_fd: Fd(2),
        dest_fd: Fd(1),
        filename: relative::Slice::empty(),
        append: 0,
        clobber: 0,
        expand: 0,
        body: relative::BStr::empty(),
    };

    let close_redirect = Redirect {
        tag: RedirectTag::CLOSE_FD,
        src_fd: Fd(1),
        dest_fd: Fd(0),
        filename: relative::Slice::empty(),
        append: 0,
        clobber: 0,
        expand: 0,
        body: relative::BStr::empty(),
    };

    let builder = ASTBuilder::new();
    apply_redirects(&[dup_redirect, close_redirect], &mut state, &mut ctx, &builder).unwrap();
    assert!(ctx.stdout().is_none());
    assert!(ctx.stderr().is_some());
}

#[test]
fn test_redirect_heredoc_unexpanded_and_expanded() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();
    state.set_var(BStr::new("USER"), BStr::new("fuchsia"));

    let mut builder = ASTBuilder::new();
    let body_expanded = builder.add_bstr(b"Hello $USER");
    let body_unexpanded = builder.add_bstr(b"Hello $USER plain");

    let heredoc_exp = Redirect {
        tag: RedirectTag::HERE_DOC,
        src_fd: Fd(0),
        dest_fd: Fd(0),
        filename: relative::Slice::empty(),
        append: 0,
        clobber: 0,
        expand: 1,
        body: body_expanded,
    };

    let heredoc_unexp = Redirect {
        tag: RedirectTag::HERE_DOC,
        src_fd: Fd(0),
        dest_fd: Fd(0),
        filename: relative::Slice::empty(),
        append: 0,
        clobber: 0,
        expand: 0,
        body: body_unexpanded,
    };

    apply_redirects(&[heredoc_exp, heredoc_unexp], &mut state, &mut ctx, &builder).unwrap();
    assert!(ctx.stdin().is_some());
}

#[test]
fn test_redirect_heredoc_short_inline() {
    use std::io::Read;

    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let mut builder = ASTBuilder::new();
    let body_bstr = builder.add_bstr(b"Short heredoc content inline");

    let heredoc = Redirect {
        tag: RedirectTag::HERE_DOC,
        src_fd: Fd(0),
        dest_fd: Fd(0),
        filename: relative::Slice::empty(),
        append: 0,
        clobber: 0,
        expand: 0,
        body: body_bstr,
    };

    apply_redirects(&[heredoc], &mut state, &mut ctx, &builder).unwrap();
    let mut stdin_file = ctx.stdin().unwrap();
    let mut content = Vec::new();
    stdin_file.read_to_end(&mut content).unwrap();
    assert_eq!(content, b"Short heredoc content inline");
}

#[test]
fn test_redirect_heredoc_max_inline() {
    use std::io::Read;

    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let mut builder = ASTBuilder::new();
    let max_inline_body = vec![b'a'; HEREDOC_INLINE_THRESHOLD];
    let body_bstr = builder.add_bstr(&max_inline_body);

    let heredoc = Redirect {
        tag: RedirectTag::HERE_DOC,
        src_fd: Fd(0),
        dest_fd: Fd(0),
        filename: relative::Slice::empty(),
        append: 0,
        clobber: 0,
        expand: 0,
        body: body_bstr,
    };

    apply_redirects(&[heredoc], &mut state, &mut ctx, &builder).unwrap();
    let mut stdin_file = ctx.stdin().unwrap();
    let mut content = Vec::new();
    stdin_file.read_to_end(&mut content).unwrap();
    assert_eq!(content, max_inline_body);
}

#[test]
fn test_redirect_heredoc_large_threaded() {
    use std::io::Read;

    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let mut builder = ASTBuilder::new();
    let large_body = vec![b'x'; HEREDOC_INLINE_THRESHOLD + 1024];
    let body_bstr = builder.add_bstr(&large_body);

    let heredoc = Redirect {
        tag: RedirectTag::HERE_DOC,
        src_fd: Fd(0),
        dest_fd: Fd(0),
        filename: relative::Slice::empty(),
        append: 0,
        clobber: 0,
        expand: 0,
        body: body_bstr,
    };

    apply_redirects(&[heredoc], &mut state, &mut ctx, &builder).unwrap();
    let mut stdin_file = ctx.stdin().unwrap();
    let mut content = Vec::new();
    stdin_file.read_to_end(&mut content).unwrap();
    assert_eq!(content, large_body);
}

#[test]
fn test_redirect_ambiguous_filename_error() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let mut builder = ASTBuilder::new();
    let parts = vec![ResolvedWordPart::Var(BString::from("UNSET_VAR"))];
    let w_slice = builder.add_resolved_word(&parts);

    let redirect = Redirect {
        tag: RedirectTag::TO_FILE,
        src_fd: Fd(1),
        dest_fd: Fd(0),
        filename: w_slice,
        append: 0,
        clobber: 1,
        expand: 0,
        body: relative::BStr::empty(),
    };

    let res = apply_redirects(&[redirect], &mut state, &mut ctx, &builder);
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("ambiguous redirect"));
}

#[test]
fn test_redirect_from_file_nonexistent_error() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let mut builder = ASTBuilder::new();
    let parts = vec![ResolvedWordPart::Literal(BString::from("/nonexistent_file_xyz_123"))];
    let w_slice = builder.add_resolved_word(&parts);

    let redirect = Redirect {
        tag: RedirectTag::FROM_FILE,
        src_fd: Fd(0),
        dest_fd: Fd(0),
        filename: w_slice,
        append: 0,
        clobber: 0,
        expand: 0,
        body: relative::BStr::empty(),
    };

    let res = apply_redirects(&[redirect], &mut state, &mut ctx, &builder);
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("Failed to open"));
}

#[test]
fn test_eval_redirect_wrapper() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

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

    let res = eval_redirect(&mut builder, redirect_cmd, &mut state, &mut ctx).unwrap();
    assert_eq!(res, EvalOutcome::Code(0));
}

#[test]
fn test_redirect_read_write() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    // 1. <> /dev/null
    let mut builder = ASTBuilder::new();
    let dev_null =
        builder.add_resolved_word(&[ResolvedWordPart::Literal(BString::from("/dev/null"))]);
    let redirect_null = Redirect {
        tag: RedirectTag::READ_WRITE,
        src_fd: Fd(0),
        dest_fd: Fd(0),
        filename: dev_null,
        append: 0,
        clobber: 0,
        expand: 0,
        body: relative::BStr::empty(),
    };
    apply_redirects(&[redirect_null], &mut state, &mut ctx, &builder).unwrap();
    assert!(ctx.stdin().is_some());

    // 2. <> creates missing file, reads/writes without truncating existing file, and ignores noclobber
    let test_file = "/tmp/zxsh_test_redirect_rw.txt";
    let _ = fs::remove_file(test_file);

    state.opt_noclobber = true;
    let script = format!(
        "exec 3<>{f}; printf 'abcdef' >&3; exec 3<&-; exec 3<>{f}; printf 'XY' >&3; exec 3<&-; exec 3<>{f}; read line <&3; exec 3<&-",
        f = test_file
    );
    let outcome = eval_string(BStr::new(script.as_bytes()), &mut state, &mut ctx).unwrap();
    assert_eq!(outcome, EvalOutcome::Code(0));
    assert_eq!(state.get_var(BStr::new("line")), Some(BString::from("XYcdef")));
    assert_eq!(fs::read_to_string(test_file).unwrap(), "XYcdef");

    let _ = fs::remove_file(test_file);
}

#[test]
fn test_redirect_dynamic_dup_and_close_fd() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    // 1. Dynamic FD duplication (2>&$fd where fd=1) and dynamic close (1>&$close where close=-)
    state.set_var(BStr::new("fd"), BStr::new("1"));
    state.set_var(BStr::new("close"), BStr::new("-"));

    let mut builder = ASTBuilder::new();
    let fd_word = builder.add_resolved_word(&[ResolvedWordPart::Var(BString::from("fd"))]);
    let close_word = builder.add_resolved_word(&[ResolvedWordPart::Var(BString::from("close"))]);

    let dup_redirect = Redirect {
        tag: RedirectTag::DUP_FD,
        src_fd: Fd(2),
        dest_fd: Fd(0),
        filename: fd_word,
        append: 0,
        clobber: 0,
        expand: 0,
        body: relative::BStr::empty(),
    };
    let close_redirect = Redirect {
        tag: RedirectTag::DUP_FD,
        src_fd: Fd(1),
        dest_fd: Fd(0),
        filename: close_word,
        append: 0,
        clobber: 0,
        expand: 0,
        body: relative::BStr::empty(),
    };

    apply_redirects(&[dup_redirect, close_redirect], &mut state, &mut ctx, &builder).unwrap();
    assert!(ctx.stdout().is_none());
    assert!(ctx.stderr().is_some());

    // 2. Ambiguous redirect when variable is unset or expands to multiple words
    let mut ctx2 = ExecutionContext::initial().unwrap();
    state.unset_var(BStr::new("unset_fd"));
    let mut builder2 = ASTBuilder::new();
    let unset_word =
        builder2.add_resolved_word(&[ResolvedWordPart::Var(BString::from("unset_fd"))]);
    let unset_redirect = Redirect {
        tag: RedirectTag::DUP_FD,
        src_fd: Fd(2),
        dest_fd: Fd(0),
        filename: unset_word,
        append: 0,
        clobber: 0,
        expand: 0,
        body: relative::BStr::empty(),
    };
    let err_unset =
        apply_redirects(&[unset_redirect], &mut state, &mut ctx2, &builder2).unwrap_err();
    assert!(err_unset.contains("ambiguous redirect"));

    state.set_var(BStr::new("multi_fd"), BStr::new("1 2"));
    let multi_word =
        builder2.add_resolved_word(&[ResolvedWordPart::Var(BString::from("multi_fd"))]);
    let multi_redirect = Redirect {
        tag: RedirectTag::DUP_FD,
        src_fd: Fd(2),
        dest_fd: Fd(0),
        filename: multi_word,
        append: 0,
        clobber: 0,
        expand: 0,
        body: relative::BStr::empty(),
    };
    let err_multi =
        apply_redirects(&[multi_redirect], &mut state, &mut ctx2, &builder2).unwrap_err();
    assert!(err_multi.contains("ambiguous redirect"));

    // 3. Invalid FD number at runtime returns "bad file descriptor"
    state.set_var(BStr::new("bad_fd"), BStr::new("not_an_fd"));
    let bad_word = builder2.add_resolved_word(&[ResolvedWordPart::Var(BString::from("bad_fd"))]);
    let bad_redirect = Redirect {
        tag: RedirectTag::DUP_FD,
        src_fd: Fd(2),
        dest_fd: Fd(0),
        filename: bad_word,
        append: 0,
        clobber: 0,
        expand: 0,
        body: relative::BStr::empty(),
    };
    let err_bad = apply_redirects(&[bad_redirect], &mut state, &mut ctx2, &builder2).unwrap_err();
    assert!(err_bad.contains("not_an_fd: bad file descriptor"));

    state.set_var(BStr::new("bad_fd"), BStr::new("-1"));
    let neg_redirect = Redirect {
        tag: RedirectTag::DUP_FD,
        src_fd: Fd(2),
        dest_fd: Fd(0),
        filename: bad_word,
        append: 0,
        clobber: 0,
        expand: 0,
        body: relative::BStr::empty(),
    };
    let err_neg = apply_redirects(&[neg_redirect], &mut state, &mut ctx2, &builder2).unwrap_err();
    assert!(err_neg.contains("-1: bad file descriptor"));
}

#[test]
fn test_bare_redirection_in_subshell() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();

    let test_file = "/tmp/zxsh_test_bare_subshell_redir.txt";
    let _ = fs::remove_file(test_file);

    let script = format!("(> {})", test_file);
    let outcome = eval_string(BStr::new(script.as_bytes()), &mut state, &mut ctx).unwrap();
    assert_eq!(outcome, EvalOutcome::Code(0));
    assert_eq!(fs::read_to_string(test_file).unwrap(), "");

    let _ = fs::remove_file(test_file);

    let bad_outcome =
        eval_string(BStr::new(b"(< /nonexistent_zxsh_subshell_redir_xyz)"), &mut state, &mut ctx)
            .unwrap();
    assert_eq!(bad_outcome, EvalOutcome::Code(1));
}
