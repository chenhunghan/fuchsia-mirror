// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::args::Args;
use crate::collections::{FlatMap, FlatSet};
use crate::eval::testing::{
    ExpandedCommand, append_args_to_command, expand_alias, expand_argument,
    expand_assignment_value, expand_var_with_modifiers, get_literal_command_name,
    needs_subshell_process,
};
use crate::eval::{ExecutionContext, ShellState, expand_prompt, expand_string};
use crate::parser::ast::{ASTBuilder, CommandTag, ResolvedWordPart, WordPart, WordPartTag};
use crate::parser::{parse_script, tokenize};
use crate::relative;
use bstr::{BStr, BString, ByteSlice};

fn is_assignment(arg: &[WordPart], buf: &relative::Buffer) -> bool {
    if arg.is_empty() {
        return false;
    }
    if arg[0].tag == WordPartTag::LITERAL {
        let s = arg[0].text.as_bstr(buf);
        if let Some(pos) = s.as_bytes().iter().position(|&b| b == b'=') {
            let name = &s.as_bytes()[..pos];
            if name.is_empty() {
                return false;
            }
            let mut bytes = name.iter();
            let &first = bytes.next().unwrap();
            return (first.is_ascii_alphabetic() || first == b'_')
                && bytes.all(|&c| c.is_ascii_alphanumeric() || c == b'_');
        }
    }
    false
}

fn make_slice(
    builder: &mut ASTBuilder,
    parts: &[ResolvedWordPart],
) -> relative::Slice<crate::parser::ast::WordPart> {
    builder.add_resolved_word(parts)
}

fn check_expand(
    builder: &mut ASTBuilder,
    parts: &[ResolvedWordPart],
    state: &mut ShellState,
    ctx: &ExecutionContext,
) -> Vec<BString> {
    let slice_ptr = make_slice(builder, parts);
    let arg = builder.get_slice(slice_ptr);
    expand_argument(arg, state, ctx, builder).unwrap()
}

fn check_assignment(builder: &mut ASTBuilder, parts: &[ResolvedWordPart]) -> bool {
    let slice_ptr = make_slice(builder, parts);
    let arg = builder.get_slice(slice_ptr);
    is_assignment(arg, builder)
}

#[test]
fn test_expand_argument() {
    let mut state = ShellState::new();
    state.set_var(BStr::new(b"FOO"), BStr::new(b"one two"));
    state.set_var(BStr::new(b"BAR"), BStr::new(b"three"));
    let ctx = ExecutionContext::initial().unwrap();

    // Literal
    let mut enc = ASTBuilder::new();
    assert_eq!(
        check_expand(
            &mut enc,
            &[ResolvedWordPart::Literal(BString::from("hello"))],
            &mut state,
            &ctx
        ),
        vec![BString::from("hello")]
    );

    // Var (unquoted) -> splits
    let mut enc = ASTBuilder::new();
    assert_eq!(
        check_expand(&mut enc, &[ResolvedWordPart::Var(BString::from("FOO"))], &mut state, &ctx),
        vec![BString::from("one"), BString::from("two")]
    );

    // QuotedVar -> no split
    let mut enc = ASTBuilder::new();
    assert_eq!(
        check_expand(
            &mut enc,
            &[ResolvedWordPart::QuotedVar(BString::from("FOO"))],
            &mut state,
            &ctx
        ),
        vec![BString::from("one two")]
    );

    // Combination: hello$FOO"world"
    let mut enc = ASTBuilder::new();
    assert_eq!(
        check_expand(
            &mut enc,
            &[
                ResolvedWordPart::Literal(BString::from("hello")),
                ResolvedWordPart::Var(BString::from("FOO")),
                ResolvedWordPart::QuotedLiteral(BString::from("world")),
            ],
            &mut state,
            &ctx
        ),
        vec![BString::from("helloone"), BString::from("twoworld")]
    );

    // Positional parameters
    let mut env_args = ShellState::with_args(
        Args::with_positionals(
            BString::from("script.sh"),
            vec![BString::from("a b"), BString::from("c")],
        ),
        FlatMap::new(),
    )
    .unwrap();

    // "$@" -> separate args, preserving spaces in args
    let mut enc = ASTBuilder::new();
    assert_eq!(
        check_expand(
            &mut enc,
            &[ResolvedWordPart::QuotedVar(BString::from("@"))],
            &mut env_args,
            &ctx
        ),
        vec![BString::from("a b"), BString::from("c")]
    );

    // "$*" -> single arg joined by space
    let mut enc = ASTBuilder::new();
    assert_eq!(
        check_expand(
            &mut enc,
            &[ResolvedWordPart::QuotedVar(BString::from("*"))],
            &mut env_args,
            &ctx
        ),
        vec![BString::from("a b c")]
    );

    // Empty args "$@" -> 0 args
    let mut env_empty = ShellState::with_args(
        Args::with_positionals(BString::from("script.sh"), Vec::new()),
        FlatMap::new(),
    )
    .unwrap();
    let mut enc = ASTBuilder::new();
    assert_eq!(
        check_expand(
            &mut enc,
            &[ResolvedWordPart::QuotedVar(BString::from("@"))],
            &mut env_empty,
            &ctx
        ),
        Vec::<BString>::new()
    );

    // Empty args "$@" with prefix -> 1 arg
    let mut enc = ASTBuilder::new();
    assert_eq!(
        check_expand(
            &mut enc,
            &[
                ResolvedWordPart::Literal(BString::from("prefix")),
                ResolvedWordPart::QuotedVar(BString::from("@")),
            ],
            &mut env_empty,
            &ctx
        ),
        vec![BString::from("prefix")]
    );
}

#[test]
fn test_is_assignment() {
    let mut enc = ASTBuilder::new();
    assert!(check_assignment(&mut enc, &[ResolvedWordPart::Literal(BString::from("FOO=bar"))]));
    let mut enc = ASTBuilder::new();
    assert!(check_assignment(&mut enc, &[ResolvedWordPart::Literal(BString::from("A_B_C=123"))]));
    let mut enc = ASTBuilder::new();
    assert!(!check_assignment(&mut enc, &[ResolvedWordPart::Literal(BString::from("foo"))]));
    let mut enc = ASTBuilder::new();
    assert!(!check_assignment(&mut enc, &[ResolvedWordPart::Literal(BString::from("=bar"))]));
    // Quoted start is not assignment
    let mut enc = ASTBuilder::new();
    assert!(!check_assignment(
        &mut enc,
        &[ResolvedWordPart::QuotedLiteral(BString::from("FOO=bar"))]
    ));
}

#[test]
fn test_parameter_modifiers() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();

    // 1. Length
    state.set_var(b"VAR", b"hello");
    assert_eq!(expand_var_with_modifiers(BStr::new(b"#VAR"), &mut state, &ctx).unwrap(), "5");

    // 2. Default value (unset / null)
    state.unset_var(b"VAR");
    assert_eq!(
        expand_var_with_modifiers(BStr::new(b"VAR:-default"), &mut state, &ctx).unwrap(),
        "default"
    );
    state.set_var(b"VAR", b"");
    assert_eq!(
        expand_var_with_modifiers(BStr::new(b"VAR:-default"), &mut state, &ctx).unwrap(),
        "default"
    );
    // non-null only
    assert_eq!(expand_var_with_modifiers(BStr::new(b"VAR-default"), &mut state, &ctx).unwrap(), "");

    // 3. Assign default
    state.unset_var(b"VAR");
    assert_eq!(
        expand_var_with_modifiers(BStr::new(b"VAR:=assigned"), &mut state, &ctx).unwrap(),
        "assigned"
    );
    assert_eq!(state.get_var(b"VAR").unwrap(), "assigned");

    // 4. Alternative value
    state.set_var(b"VAR", b"hello");
    assert_eq!(
        expand_var_with_modifiers(BStr::new(b"VAR:+alternative"), &mut state, &ctx).unwrap(),
        "alternative"
    );
    state.unset_var(b"VAR");
    assert_eq!(
        expand_var_with_modifiers(BStr::new(b"VAR:+alternative"), &mut state, &ctx).unwrap(),
        ""
    );

    // 5. Remove prefix
    state.set_var(b"VAR", b"foobar");
    assert_eq!(expand_var_with_modifiers(BStr::new(b"VAR#foo"), &mut state, &ctx).unwrap(), "bar");
    assert_eq!(expand_var_with_modifiers(BStr::new(b"VAR#f*o"), &mut state, &ctx).unwrap(), "obar");
    // longest vs shortest prefix
    state.set_var(b"VAR", b"a/b/c");
    assert_eq!(expand_var_with_modifiers(BStr::new(b"VAR#*/"), &mut state, &ctx).unwrap(), "b/c");
    assert_eq!(expand_var_with_modifiers(BStr::new(b"VAR##*/"), &mut state, &ctx).unwrap(), "c");

    // 6. Remove suffix
    state.set_var(b"VAR", b"foobar");
    assert_eq!(expand_var_with_modifiers(BStr::new(b"VAR%bar"), &mut state, &ctx).unwrap(), "foo");
    assert_eq!(expand_var_with_modifiers(BStr::new(b"VAR%b*r"), &mut state, &ctx).unwrap(), "foo");
    // longest vs shortest suffix
    state.set_var(b"VAR", b"a/b/c");
    assert_eq!(expand_var_with_modifiers(BStr::new(b"VAR%/*"), &mut state, &ctx).unwrap(), "a/b");
    assert_eq!(expand_var_with_modifiers(BStr::new(b"VAR%%/*"), &mut state, &ctx).unwrap(), "a");
}

#[test]
fn test_expand_string_and_heredoc() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();
    state.set_var("FOO", "bar");

    let res = expand_string(BStr::new("hello $FOO $((1+2))"), &mut state, &ctx).unwrap();
    assert_eq!(res, "hello bar 3");

    let hd = expand_string(BStr::new("line 1: $FOO\nline 2: $((4+5))"), &mut state, &ctx).unwrap();
    assert_eq!(hd, "line 1: bar\nline 2: 9");
}

#[test]
fn test_needs_subshell_process() {
    let mut state = ShellState::new();
    let check_cmd = |s: &str, state: &ShellState| -> bool {
        let mut builder = ASTBuilder::new();
        let tokens = tokenize(BStr::new(s)).unwrap();
        let cmds = parse_script(&mut builder, &tokens).unwrap();
        let cmd = builder.get_ref(cmds[0]);
        needs_subshell_process(cmd, state, &builder)
    };

    // Builtins, prefix-assigned builtins, and composite-quoted builtins require subshell
    assert!(check_cmd("echo hello", &state));
    assert!(check_cmd("FOO=bar echo hello", &state));
    assert!(check_cmd("\"ec\"\"ho\" hello", &state));
    assert!(check_cmd("e'ch'o hello", &state));

    // Dynamic command names, bare assignments, and bare redirections require subshell
    assert!(check_cmd("$cmd hello", &state));
    assert!(check_cmd("$(echo grep) pattern", &state));
    assert!(check_cmd("FOO=bar", &state));
    assert!(check_cmd(">/dev/null", &state));

    // Plain external commands (including with plain prefix assignments or plain $VAR args) do not require subshell
    assert!(!check_cmd("grep pattern", &state));
    assert!(!check_cmd("FOO=bar grep $PATTERN", &state));
    assert!(!check_cmd("grep pattern >/dev/null", &state));
    assert!(!check_cmd("grep pattern <>/dev/null", &state));
    assert!(!check_cmd("grep pattern >&$fd", &state));

    // Side-effecting expansions in arguments, prefix assignments, or redirections require subshell
    assert!(check_cmd("grep $((x = 1))", &state));
    assert!(check_cmd("grep ${y:=default}", &state));
    assert!(check_cmd("grep $(echo pattern)", &state));
    assert!(check_cmd("FOO=$((x = 1)) grep pattern", &state));
    assert!(check_cmd("grep pattern >$((x = 1))", &state));
    assert!(check_cmd("grep pattern <>$((x = 1))", &state));
    assert!(check_cmd("grep pattern >&$((x = 1))", &state));

    // Functions and aliases require subshell
    state.add_function(BString::from("my_fn"), vec![0]);
    assert!(check_cmd("my_fn arg", &state));
    assert!(check_cmd("FOO=1 my_fn arg", &state));

    state.aliases.insert(BString::from("my_alias"), BString::from("grep"));
    assert!(check_cmd("my_alias pattern", &state));

    let mut builder = ASTBuilder::new();
    let sub_ptr = builder.add_unary_command(CommandTag::SUBSHELL, relative::Ptr::null());
    let sub_cmd = builder.get_ref(sub_ptr);
    assert!(needs_subshell_process(sub_cmd, &state, &builder));
}

#[test]
fn test_expand_assignment_value() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();
    state.set_var(BStr::new("HOME"), BStr::new("/home/test"));

    let mut builder = ASTBuilder::new();
    let parts = vec![
        ResolvedWordPart::Literal(BString::from("PATH=")),
        ResolvedWordPart::Literal(BString::from("~/bin:~/usr")),
    ];
    let w_slice = builder.add_resolved_word(&parts);
    let slice = builder.get_slice(w_slice);

    let res = expand_assignment_value(BStr::new("PATH="), &slice[1..], &mut state, &ctx, &builder)
        .unwrap();
    assert_eq!(res, "PATH=/home/test/bin:/home/test/usr");
}

#[test]
fn test_append_args_to_command() {
    let mut builder = ASTBuilder::new();
    let tokens = tokenize(BStr::new("echo foo | grep bar")).unwrap();
    let cmds = parse_script(&mut builder, &tokens).unwrap();
    let pipe_ptr = cmds[0];

    let parts = vec![ResolvedWordPart::Literal(BString::from("baz"))];
    let w_slice = builder.add_resolved_word(&parts);
    let extra = vec![w_slice];

    append_args_to_command(&mut builder, pipe_ptr, &extra);
}

#[test]
fn test_expand_alias() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();
    state.aliases.insert(BString::from("ll"), BString::from("ls -l"));
    state.aliases.insert(BString::from("rec"), BString::from("rec"));

    let mut builder = ASTBuilder::new();
    let parts = vec![ResolvedWordPart::Literal(BString::from("ll"))];
    let w_slice = builder.add_resolved_word(&parts);
    let args = vec![w_slice];

    let mut active = FlatSet::new();
    let res = expand_alias(&mut builder, &args, &state, &mut ctx, &mut active).unwrap();
    assert!(matches!(res, Some(ExpandedCommand::Words(_))));

    let rec_parts = vec![ResolvedWordPart::Literal(BString::from("rec"))];
    let rec_slice = builder.add_resolved_word(&rec_parts);
    let rec_args = vec![rec_slice];
    active.insert(BString::from("rec"));
    let res_rec = expand_alias(&mut builder, &rec_args, &state, &mut ctx, &mut active).unwrap();
    assert!(res_rec.is_none());
}

#[test]
fn test_parameter_modifiers_error() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();

    // Custom message
    state.unset_var(BStr::new("VAR"));
    assert!(expand_var_with_modifiers(BStr::new("VAR:?custom error"), &mut state, &ctx).is_err());
    assert!(expand_var_with_modifiers(BStr::new("VAR?custom error"), &mut state, &ctx).is_err());

    // Default message
    assert!(expand_var_with_modifiers(BStr::new("VAR:?"), &mut state, &ctx).is_err());
    assert!(expand_var_with_modifiers(BStr::new("VAR?"), &mut state, &ctx).is_err());

    // Set variable shouldn't error
    state.set_var(BStr::new("VAR"), BStr::new("set_val"));
    assert_eq!(
        expand_var_with_modifiers(BStr::new("VAR:?custom error"), &mut state, &ctx).unwrap(),
        "set_val"
    );
    assert_eq!(
        expand_var_with_modifiers(BStr::new("VAR?custom error"), &mut state, &ctx).unwrap(),
        "set_val"
    );
}

#[test]
fn test_parameter_modifiers_assign() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();

    // Assign non-null
    state.unset_var(BStr::new("VAR"));
    assert_eq!(
        expand_var_with_modifiers(BStr::new("VAR=new_val"), &mut state, &ctx).unwrap(),
        "new_val"
    );

    // Readonly assignment fails
    state.make_readonly(BStr::new("RO"));
    assert!(expand_var_with_modifiers(BStr::new("RO:=val"), &mut state, &ctx).is_err());
}

#[test]
fn test_parameter_modifiers_alternative() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();

    state.unset_var(BStr::new("VAR"));
    assert_eq!(expand_var_with_modifiers(BStr::new("VAR+alt"), &mut state, &ctx).unwrap(), "");

    state.set_var(BStr::new("VAR"), BStr::new("orig"));
    assert_eq!(expand_var_with_modifiers(BStr::new("VAR+alt"), &mut state, &ctx).unwrap(), "alt");
}

#[test]
fn test_parameter_modifiers_nounset() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();
    state.opt_nounset = true;

    state.unset_var(BStr::new("UNBOUND"));
    assert!(expand_var_with_modifiers(BStr::new("UNBOUND"), &mut state, &ctx).is_err());
    assert!(expand_var_with_modifiers(BStr::new("#UNBOUND"), &mut state, &ctx).is_err());
}

#[test]
fn test_positional_args_at_expansion() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();
    state.set_args(vec![BString::from("arg1"), BString::from("arg2"), BString::from("arg3")]);

    let mut enc = ASTBuilder::new();
    let res = check_expand(
        &mut enc,
        &[ResolvedWordPart::QuotedVar(BString::from("@"))],
        &mut state,
        &ctx,
    );
    assert_eq!(res, vec![BString::from("arg1"), BString::from("arg2"), BString::from("arg3")]);
}

#[test]
fn test_opt_noglob() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();
    state.opt_noglob = true;

    let mut enc = ASTBuilder::new();
    let res = check_expand(
        &mut enc,
        &[ResolvedWordPart::Literal(BString::from("*.rs"))],
        &mut state,
        &ctx,
    );
    assert_eq!(res, vec![BString::from("*.rs")]);
}

#[test]
fn test_expand_string_escapes_and_special_vars() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();
    state.set_args(vec![BString::from("first")]);
    state.set_var(BStr::new("?"), BStr::new("0"));

    let res =
        expand_string(BStr::new("escaped \\$FOO and \\\\ and \\` and $1 and $?"), &mut state, &ctx)
            .unwrap();
    assert_eq!(res, "escaped $FOO and \\ and ` and first and 0");
}

#[test]
fn test_expand_alias_trailing_space() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();
    state.aliases.insert(BString::from("echo_sp"), BString::from("echo "));
    state.aliases.insert(BString::from("foo"), BString::from("bar"));

    let mut builder = ASTBuilder::new();
    let parts1 = vec![ResolvedWordPart::Literal(BString::from("echo_sp"))];
    let parts2 = vec![ResolvedWordPart::Literal(BString::from("foo"))];
    let slice1 = builder.add_resolved_word(&parts1);
    let slice2 = builder.add_resolved_word(&parts2);
    let args = vec![slice1, slice2];

    let mut active = FlatSet::new();
    let res = expand_alias(&mut builder, &args, &state, &mut ctx, &mut active).unwrap();
    assert!(matches!(res, Some(ExpandedCommand::Words(_))));
}

#[test]
fn test_expand_alias_compound() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();
    state.aliases.insert(BString::from("myif"), BString::from("if true; then echo hi; fi"));

    let mut builder = ASTBuilder::new();
    let parts = vec![ResolvedWordPart::Literal(BString::from("myif"))];
    let slice = builder.add_resolved_word(&parts);
    let args = vec![slice];

    let mut active = FlatSet::new();
    let res = expand_alias(&mut builder, &args, &state, &mut ctx, &mut active).unwrap();
    assert!(matches!(res, Some(ExpandedCommand::Command(_))));
}

#[test]
fn test_expand_argument_arithmetic() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();

    let mut builder = ASTBuilder::new();
    let parts = vec![
        ResolvedWordPart::Arithmetic(BString::from("2 + 3")),
        ResolvedWordPart::Literal(BString::from("-")),
        ResolvedWordPart::QuotedArithmetic(BString::from("10 * 2")),
    ];
    let slice_ptr = make_slice(&mut builder, &parts);
    let arg = builder.get_slice(slice_ptr);
    let expanded = expand_argument(arg, &mut state, &ctx, &builder).unwrap();
    assert_eq!(expanded, vec![BString::from("5-20")]);
}

#[test]
fn test_tilde_expansion_assignment_and_leading() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();
    state.set_var(BStr::new("HOME"), BStr::new("/home/user"));

    let mut builder = ASTBuilder::new();

    // Leading tilde: ~ and ~/sub
    let parts1 = vec![ResolvedWordPart::Literal(BString::from("~"))];
    let parts2 = vec![ResolvedWordPart::Literal(BString::from("~/sub"))];
    assert_eq!(
        check_expand(&mut builder, &parts1, &mut state, &ctx),
        vec![BString::from("/home/user")]
    );
    assert_eq!(
        check_expand(&mut builder, &parts2, &mut state, &ctx),
        vec![BString::from("/home/user/sub")]
    );
}

#[test]
fn test_ifs_non_whitespace_splitting() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();
    state.set_var(BStr::new("IFS"), BStr::new(":"));
    state.set_var(BStr::new("VAR"), BStr::new("one:two::three"));

    let mut builder = ASTBuilder::new();
    let parts = vec![ResolvedWordPart::Var(BString::from("VAR"))];
    let res = check_expand(&mut builder, &parts, &mut state, &ctx);
    assert_eq!(
        res,
        vec![BString::from("one"), BString::from("two"), BString::from(""), BString::from("three")]
    );
}

#[test]
fn test_expand_string_braced_var_and_trailing_dollar() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();
    state.set_var(BStr::new("FOO"), BStr::new("bar"));

    let res =
        expand_string(BStr::new("val: ${FOO} and trailing $ or $5"), &mut state, &ctx).unwrap();
    assert_eq!(res, "val: bar and trailing $ or ");
}

#[test]
fn test_get_literal_command_name_edge_cases() {
    let mut builder = ASTBuilder::new();
    let parts = vec![
        ResolvedWordPart::Literal(BString::from("echo")),
        ResolvedWordPart::Literal(BString::from("extra")),
    ];
    let slice_ptr = make_slice(&mut builder, &parts);
    let arg = builder.get_slice(slice_ptr);
    assert!(get_literal_command_name(arg, &builder).is_none());

    let var_parts = vec![ResolvedWordPart::Var(BString::from("VAR"))];
    let var_slice = make_slice(&mut builder, &var_parts);
    let var_arg = builder.get_slice(var_slice);
    assert!(get_literal_command_name(var_arg, &builder).is_none());
}

#[test]
fn test_expand_alias_nested() {
    let mut state = ShellState::new();
    let mut ctx = ExecutionContext::initial().unwrap();
    state.aliases.insert(BString::from("a"), BString::from("b"));
    state.aliases.insert(BString::from("b"), BString::from("ls"));

    let mut builder = ASTBuilder::new();
    let parts = vec![ResolvedWordPart::Literal(BString::from("a"))];
    let slice = builder.add_resolved_word(&parts);
    let args = vec![slice];

    let mut active = FlatSet::new();
    let res = expand_alias(&mut builder, &args, &state, &mut ctx, &mut active).unwrap();
    assert!(matches!(res, Some(ExpandedCommand::Words(_))));
}

#[test]
fn test_expand_prompt() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();

    // Fall back to default when unset
    assert_eq!(
        expand_prompt(BStr::new("MY_PS"), BStr::new("default> "), &mut state, &ctx),
        "default> "
    );

    // Expand variable when set
    state.set_var(BStr::new("MY_PS"), BStr::new("${USER}@host$ "));
    state.set_var(BStr::new("USER"), BStr::new("testuser"));
    assert_eq!(
        expand_prompt(BStr::new("MY_PS"), BStr::new("default> "), &mut state, &ctx),
        "testuser@host$ "
    );

    // Fall back to default on invalid expansion syntax
    state.set_var(BStr::new("MY_PS"), BStr::new("$(( 1 / 0 ))"));
    assert_eq!(
        expand_prompt(BStr::new("MY_PS"), BStr::new("default> "), &mut state, &ctx),
        "default> "
    );
}

#[test]
fn test_command_substitution_records_status() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();
    let mut builder = ASTBuilder::new();

    // 1. WordPart::CMD_SUB records exit status in $? and last_cmd_sub_status
    let sub_tokens = tokenize(BStr::new("printf ok; exit 33")).unwrap();
    let sub_cmds = parse_script(&mut builder, &sub_tokens).unwrap();
    let sub_root = builder.add_sequence_or_single(&sub_cmds);
    let res = check_expand(
        &mut builder,
        &[ResolvedWordPart::CommandSubstitution(sub_root)],
        &mut state,
        &ctx,
    );
    assert_eq!(res, vec![BString::from("ok")]);
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("33")));
    assert_eq!(state.take_cmd_sub_status(), Some(33));
    assert_eq!(state.take_cmd_sub_status(), None);

    // 2. expand_string with $(...) records exit status
    let s = expand_string(BStr::new("val=$(printf hi; exit 12)"), &mut state, &ctx).unwrap();
    assert_eq!(s, "val=hi");
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("12")));
    assert_eq!(state.take_cmd_sub_status(), Some(12));

    // 3. expand_string with backticks `...` and backslash escapes records exit status
    let bt = expand_string(BStr::new("`printf '%s' 'a\\\\b\\`c\\$d'; exit 19`"), &mut state, &ctx)
        .unwrap();
    assert_eq!(bt, "a\\b`c$d");
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("19")));
    assert_eq!(state.take_cmd_sub_status(), Some(19));

    // 4. expand_string with unclosed backtick preserves literal
    let unclosed = expand_string(BStr::new("`unclosed"), &mut state, &ctx).unwrap();
    assert_eq!(unclosed, "`unclosed");

    // 5. expand_prompt preserves $? and last_cmd_sub_status even if prompt runs $(...)
    state.record_cmd_sub_status(44);
    state.set_var(BStr::new("MY_PS"), BStr::new("$(printf prompt; exit 88)> "));
    let prompt = expand_prompt(BStr::new("MY_PS"), BStr::new("$ "), &mut state, &ctx);
    assert_eq!(prompt, "prompt> ");
    assert_eq!(state.get_var(BStr::new("?")), Some(BString::from("44")));
    assert_eq!(state.take_cmd_sub_status(), Some(44));
}

#[test]
fn test_empty_quoted_and_unquoted_null_expansions() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();

    let expand_word_str = |word_src: &str, state: &mut ShellState| -> Vec<BString> {
        let mut builder = ASTBuilder::new();
        let tokens = tokenize(word_src.as_bytes()).unwrap();
        assert_eq!(tokens.len(), 1);
        let crate::parser::Token::Word(raw_parts) = &tokens[0] else {
            panic!("expected word token");
        };
        let resolved = crate::parser::resolve_word_parts(&mut builder, raw_parts).unwrap();
        check_expand(&mut builder, &resolved, state, &ctx)
    };

    // Unquoted null expansions produce 0 fields with default IFS
    assert_eq!(expand_word_str("$unset", &mut state), Vec::<BString>::new());
    assert_eq!(expand_word_str("$(true)", &mut state), Vec::<BString>::new());
    assert_eq!(expand_word_str("$unset$(true)", &mut state), Vec::<BString>::new());

    // Quoted or partially-quoted empty words produce 1 empty field ("") with default IFS
    assert_eq!(expand_word_str("\"\"", &mut state), vec![BString::from("")]);
    assert_eq!(expand_word_str("''", &mut state), vec![BString::from("")]);
    assert_eq!(expand_word_str("\"$unset\"", &mut state), vec![BString::from("")]);
    assert_eq!(expand_word_str("\"\"$unset", &mut state), vec![BString::from("")]);
    assert_eq!(expand_word_str("''$unset", &mut state), vec![BString::from("")]);
    assert_eq!(expand_word_str("$unset\"\"", &mut state), vec![BString::from("")]);
    assert_eq!(expand_word_str("$unset''", &mut state), vec![BString::from("")]);
    assert_eq!(expand_word_str("\"$(true)\"", &mut state), vec![BString::from("")]);
    assert_eq!(expand_word_str("\"\"$(true)", &mut state), vec![BString::from("")]);

    // Empty quote combined with unquoted IFS whitespace preserves empty fields at quote positions
    state.set_var(BStr::new("SPACE"), BStr::new("   "));
    assert_eq!(expand_word_str("$SPACE", &mut state), Vec::<BString>::new());
    assert_eq!(expand_word_str("\"\"$SPACE", &mut state), vec![BString::from("")]);
    assert_eq!(expand_word_str("$SPACE\"\"", &mut state), vec![BString::from("")]);
    assert_eq!(
        expand_word_str("\"\"$SPACE\"a\"", &mut state),
        vec![BString::from(""), BString::from("a")]
    );
    assert_eq!(
        expand_word_str("\"a\"$SPACE\"\"", &mut state),
        vec![BString::from("a"), BString::from("")]
    );
    assert_eq!(
        expand_word_str("\"a\"$SPACE\"\"$SPACE\"b\"", &mut state),
        vec![BString::from("a"), BString::from(""), BString::from("b")]
    );

    // "$@" when $# == 0 produces 0 fields unless combined with another quoted segment
    state.set_args(Vec::new());
    assert_eq!(expand_word_str("\"$@\"", &mut state), Vec::<BString>::new());
    assert_eq!(expand_word_str("$unset\"$@\"", &mut state), Vec::<BString>::new());
    assert_eq!(expand_word_str("\"\"\"$@\"", &mut state), vec![BString::from("")]);

    // Non-whitespace IFS splitting with empty quoted prefix/suffix
    state.set_var(BStr::new("IFS"), BStr::new(":"));
    state.set_var(BStr::new("COLON"), BStr::new(":"));
    assert_eq!(expand_word_str("$COLON", &mut state), vec![BString::from("")]);
    assert_eq!(expand_word_str("\"\"$COLON", &mut state), vec![BString::from("")]);
    assert_eq!(
        expand_word_str("$COLON\"\"", &mut state),
        vec![BString::from(""), BString::from("")]
    );

    // When IFS="" (empty), unquoted null expansions still produce 0 fields,
    // while quoted empty words still produce 1 empty field.
    state.set_var(BStr::new("IFS"), BStr::new(""));
    assert_eq!(expand_word_str("$unset", &mut state), Vec::<BString>::new());
    assert_eq!(expand_word_str("$(true)", &mut state), Vec::<BString>::new());
    assert_eq!(expand_word_str("$unset$(true)", &mut state), Vec::<BString>::new());
    assert_eq!(expand_word_str("\"$unset\"", &mut state), vec![BString::from("")]);
    assert_eq!(expand_word_str("\"\"$unset", &mut state), vec![BString::from("")]);
    assert_eq!(expand_word_str("''$unset", &mut state), vec![BString::from("")]);
    assert_eq!(expand_word_str("$unset\"\"", &mut state), vec![BString::from("")]);
    assert_eq!(expand_word_str("\"$(true)\"", &mut state), vec![BString::from("")]);

    // Unquoted non-empty variable does not split when IFS=""
    state.set_var(BStr::new("WORDS"), BStr::new("one two"));
    assert_eq!(expand_word_str("$WORDS", &mut state), vec![BString::from("one two")]);

    // Assignment to unquoted unset variable produces empty value
    let mut builder = ASTBuilder::new();
    let var_word = builder.add_resolved_word(&[ResolvedWordPart::Var(BString::from("unset"))]);
    let var_slice = builder.get_slice(var_word);
    let assigned =
        expand_assignment_value(BStr::new(""), var_slice, &mut state, &ctx, &builder).unwrap();
    assert_eq!(assigned, "");
}

#[test]
fn test_star_and_at_expansion_with_ifs() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();

    let expand_word_str = |word_src: &str, state: &mut ShellState| -> Vec<BString> {
        let mut builder = ASTBuilder::new();
        let tokens = tokenize(word_src.as_bytes()).unwrap();
        assert_eq!(tokens.len(), 1);
        let crate::parser::Token::Word(raw_parts) = &tokens[0] else {
            panic!("expected word token");
        };
        let resolved = crate::parser::resolve_word_parts(&mut builder, raw_parts).unwrap();
        check_expand(&mut builder, &resolved, state, &ctx)
    };

    // 1. "$*" with default IFS, custom IFS=":", empty IFS="", and unset IFS
    state.set_args(vec![BString::from("a"), BString::from("b"), BString::from("c")]);
    assert_eq!(expand_word_str("\"$*\"", &mut state), vec![BString::from("a b c")]);

    state.set_var(BStr::new("IFS"), BStr::new(":"));
    assert_eq!(expand_word_str("\"$*\"", &mut state), vec![BString::from("a:b:c")]);

    state.set_var(BStr::new("IFS"), BStr::new(""));
    assert_eq!(expand_word_str("\"$*\"", &mut state), vec![BString::from("abc")]);

    state.unset_var(BStr::new("IFS"));
    assert_eq!(expand_word_str("\"$*\"", &mut state), vec![BString::from("a b c")]);

    // 2. Unquoted $@ and $* with set -- "a b" "c:d"
    state.set_args(vec![BString::from("a b"), BString::from("c:d")]);

    // Default IFS (" \t\n")
    state.set_var(BStr::new("IFS"), BStr::new(" \t\n"));
    assert_eq!(
        expand_word_str("$@", &mut state),
        vec![BString::from("a"), BString::from("b"), BString::from("c:d")]
    );
    assert_eq!(
        expand_word_str("$*", &mut state),
        vec![BString::from("a"), BString::from("b"), BString::from("c:d")]
    );

    // Custom IFS=":"
    state.set_var(BStr::new("IFS"), BStr::new(":"));
    assert_eq!(
        expand_word_str("$@", &mut state),
        vec![BString::from("a b"), BString::from("c"), BString::from("d")]
    );
    assert_eq!(
        expand_word_str("$*", &mut state),
        vec![BString::from("a b"), BString::from("c"), BString::from("d")]
    );

    // Empty IFS=""
    state.set_var(BStr::new("IFS"), BStr::new(""));
    assert_eq!(expand_word_str("$@", &mut state), vec![BString::from("a b"), BString::from("c:d")]);
    assert_eq!(expand_word_str("$*", &mut state), vec![BString::from("a b"), BString::from("c:d")]);

    // 3. Unquoted $@ and $* with empty positional parameter: set -- "" "a"
    state.set_args(vec![BString::from(""), BString::from("a")]);
    state.set_var(BStr::new("IFS"), BStr::new(""));
    assert_eq!(expand_word_str("\"$@\"", &mut state), vec![BString::from(""), BString::from("a")]);
    assert_eq!(expand_word_str("$@", &mut state), vec![BString::from("a")]);
    assert_eq!(expand_word_str("$*", &mut state), vec![BString::from("a")]);
    assert_eq!(expand_word_str("\"\"$@", &mut state), vec![BString::from(""), BString::from("a")]);

    // Prefix/suffix concatenation with unquoted $@
    state.set_args(vec![BString::from(""), BString::from("")]);
    assert_eq!(
        expand_word_str("foo$@bar", &mut state),
        vec![BString::from("foo"), BString::from("bar")]
    );
    state.set_args(vec![BString::from("1"), BString::from("2")]);
    assert_eq!(
        expand_word_str("foo$@bar", &mut state),
        vec![BString::from("foo1"), BString::from("2bar")]
    );

    // 4. Unquoted $@ and $* when $# == 0 -> 0 arguments under default IFS, IFS=":", and IFS=""
    state.set_args(Vec::new());
    for ifs_val in [BStr::new(" \t\n"), BStr::new(":"), BStr::new("")] {
        state.set_var(BStr::new("IFS"), ifs_val);
        assert_eq!(expand_word_str("$@", &mut state), Vec::<BString>::new());
        assert_eq!(expand_word_str("$*", &mut state), Vec::<BString>::new());
    }

    // 5. Assignments (FieldSplitMode::DoNotSplit): x="$*", x=$*, x="$@", x=$@
    let eval_assign = |part: ResolvedWordPart, state: &mut ShellState| -> BString {
        let mut builder = ASTBuilder::new();
        let word = builder.add_resolved_word(&[part]);
        let slice = builder.get_slice(word);
        expand_assignment_value(BStr::new(""), slice, state, &ctx, &builder).unwrap()
    };

    state.set_args(vec![BString::from("a"), BString::from("b"), BString::from("c")]);
    for (ifs_val, expected_star, expected_at) in
        [(" \t\n", "a b c", "a b c"), (":", "a:b:c", "a b c"), ("", "abc", "a b c")]
    {
        state.set_var(BStr::new("IFS"), BStr::new(ifs_val));
        assert_eq!(
            eval_assign(ResolvedWordPart::QuotedVar(BString::from("*")), &mut state),
            expected_star
        );
        assert_eq!(
            eval_assign(ResolvedWordPart::Var(BString::from("*")), &mut state),
            expected_star
        );
        assert_eq!(
            eval_assign(ResolvedWordPart::QuotedVar(BString::from("@")), &mut state),
            expected_at
        );
        assert_eq!(eval_assign(ResolvedWordPart::Var(BString::from("@")), &mut state), expected_at);
    }
}

#[test]
fn test_parameter_modifier_parsing_and_quoting() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();

    let expand_word_str = |word_src: &str, state: &mut ShellState| -> Vec<BString> {
        let mut builder = ASTBuilder::new();
        let tokens = tokenize(word_src.as_bytes()).unwrap();
        assert_eq!(tokens.len(), 1);
        let crate::parser::Token::Word(raw_parts) = &tokens[0] else {
            panic!("expected word token");
        };
        let resolved = crate::parser::resolve_word_parts(&mut builder, raw_parts).unwrap();
        check_expand(&mut builder, &resolved, state, &ctx)
    };

    // 1. Left-to-right operator matching: ${var-a:-b} and ${var#a:-b}
    state.unset_var(BStr::new("var"));
    assert_eq!(
        expand_var_with_modifiers(BStr::new(b"var-a:-b"), &mut state, &ctx).unwrap(),
        "a:-b"
    );
    state.set_var(BStr::new("var"), BStr::new(""));
    assert_eq!(expand_var_with_modifiers(BStr::new(b"var-a:-b"), &mut state, &ctx).unwrap(), "");
    assert_eq!(expand_var_with_modifiers(BStr::new(b"var:-a-b"), &mut state, &ctx).unwrap(), "a-b");

    state.set_var(BStr::new("var"), BStr::new("a:-bhello"));
    assert_eq!(
        expand_var_with_modifiers(BStr::new(b"var#a:-b"), &mut state, &ctx).unwrap(),
        "hello"
    );
    state.set_var(BStr::new("var"), BStr::new("helloa:-b"));
    assert_eq!(
        expand_var_with_modifiers(BStr::new(b"var%a:-b"), &mut state, &ctx).unwrap(),
        "hello"
    );

    // 2. Special parameters with modifiers and length prefix: ${-:-default}, ${?:-0}, ${#-default}, ${#?}, ${##}
    assert_eq!(
        expand_var_with_modifiers(BStr::new(b"-:-default"), &mut state, &ctx).unwrap(),
        "default"
    );
    state.opt_errexit = true;
    assert_eq!(expand_var_with_modifiers(BStr::new(b"-:-default"), &mut state, &ctx).unwrap(), "e");
    state.opt_errexit = false;

    state.set_var(BStr::new("?"), BStr::new("0"));
    assert_eq!(expand_var_with_modifiers(BStr::new(b"?:-99"), &mut state, &ctx).unwrap(), "0");
    assert_eq!(expand_var_with_modifiers(BStr::new(b"#?"), &mut state, &ctx).unwrap(), "1");
    state.set_var(BStr::new("?"), BStr::new("127"));
    assert_eq!(expand_var_with_modifiers(BStr::new(b"#?"), &mut state, &ctx).unwrap(), "3");

    state.set_args(vec![BString::from("ab"), BString::from("cd")]);
    assert_eq!(expand_var_with_modifiers(BStr::new(b"#-default"), &mut state, &ctx).unwrap(), "2");
    assert_eq!(expand_var_with_modifiers(BStr::new(b"##"), &mut state, &ctx).unwrap(), "1");
    assert_eq!(expand_var_with_modifiers(BStr::new(b"#@"), &mut state, &ctx).unwrap(), "2");
    assert_eq!(expand_var_with_modifiers(BStr::new(b"#*"), &mut state, &ctx).unwrap(), "2");

    // 3. Quote removal in modifier word: ${unset:-"hello"}, ${unset:-'hello'}, ${unset:-\hello}
    state.unset_var(BStr::new("unset"));
    assert_eq!(
        expand_var_with_modifiers(BStr::new(b"unset:-\"hello\""), &mut state, &ctx).unwrap(),
        "hello"
    );
    assert_eq!(
        expand_var_with_modifiers(BStr::new(b"unset:-'hello'"), &mut state, &ctx).unwrap(),
        "hello"
    );
    assert_eq!(
        expand_var_with_modifiers(BStr::new(b"unset:-\\hello"), &mut state, &ctx).unwrap(),
        "hello"
    );

    // 4. Outer double quotes vs unquoted modifier words:
    // "${unset:-'hello'}" preserves single quotes, whereas ${unset:-'hello'} strips them.
    // "${unset:-a b}" does not field-split, whereas ${unset:-a b} splits into ["a", "b"].
    assert_eq!(
        expand_word_str("\"${unset:-'hello'}\"", &mut state),
        vec![BString::from("'hello'")]
    );
    assert_eq!(expand_word_str("${unset:-'hello'}", &mut state), vec![BString::from("hello")]);
    assert_eq!(expand_word_str("\"${unset:-a b}\"", &mut state), vec![BString::from("a b")]);
    assert_eq!(
        expand_word_str("${unset:-a b}", &mut state),
        vec![BString::from("a"), BString::from("b")]
    );
    assert_eq!(
        expand_word_str("${unset:-\"a b\" c}", &mut state),
        vec![BString::from("a b"), BString::from("c")]
    );
    assert_eq!(
        expand_word_str("${unset:-'a b' c}", &mut state),
        vec![BString::from("a b"), BString::from("c")]
    );
    assert_eq!(
        expand_word_str("${unset:-a\\ b c}", &mut state),
        vec![BString::from("a b"), BString::from("c")]
    );
    assert_eq!(expand_word_str("${unset:-\"\"}", &mut state), vec![BString::from("")]);
    assert_eq!(expand_word_str("${unset:-''}", &mut state), vec![BString::from("")]);
    assert_eq!(expand_word_str("${unset:-}", &mut state), Vec::<BString>::new());

    // 5. Pattern quoting vs wildcard matching in #, ##, %, %%
    state.set_var(BStr::new("var"), BStr::new("*hello*"));
    // Quoted/escaped '*' matches literal '*' only (even inside outer double quotes)
    assert_eq!(expand_word_str("${var#\"*\"}", &mut state), vec![BString::from("hello*")]);
    assert_eq!(expand_word_str("${var#'*'}", &mut state), vec![BString::from("hello*")]);
    assert_eq!(expand_word_str("${var#\\*}", &mut state), vec![BString::from("hello*")]);
    assert_eq!(expand_word_str("\"${var#\"*\"}\"", &mut state), vec![BString::from("hello*")]);
    assert_eq!(expand_word_str("\"${var#'*'}\"", &mut state), vec![BString::from("hello*")]);
    assert_eq!(expand_word_str("${var%\"*\"}", &mut state), vec![BString::from("*hello")]);
    assert_eq!(expand_word_str("${var%'*'}", &mut state), vec![BString::from("*hello")]);
    assert_eq!(expand_word_str("${var%\\*}", &mut state), vec![BString::from("*hello")]);

    // When var does not start with literal '*', quoted/escaped '*' does not strip anything
    state.set_var(BStr::new("var"), BStr::new("ahello"));
    assert_eq!(expand_word_str("${var#\"*\"}", &mut state), vec![BString::from("ahello")]);
    assert_eq!(expand_word_str("${var#'*'}", &mut state), vec![BString::from("ahello")]);
    assert_eq!(expand_word_str("${var#\\*}", &mut state), vec![BString::from("ahello")]);
    assert_eq!(expand_word_str("\"${var#'*'}\"", &mut state), vec![BString::from("ahello")]);

    // Unquoted '*' is a wildcard both outside and inside outer double quotes
    assert_eq!(expand_word_str("${var#*}", &mut state), vec![BString::from("ahello")]);
    assert_eq!(expand_word_str("\"${var#*}\"", &mut state), vec![BString::from("ahello")]);
    assert_eq!(expand_word_str("${var##*}", &mut state), Vec::<BString>::new());
    assert_eq!(expand_word_str("\"${var##*}\"", &mut state), vec![BString::from("")]);
    assert_eq!(expand_word_str("\"${var#*h}\"", &mut state), vec![BString::from("ello")]);

    // 6. Bad substitution and invalid := assignment errors
    for bad in
        [b"" as &[u8], b"var:", b"var:x", b"1a", b"1a:-default", b":-default", b"#var:-default"]
    {
        assert!(
            expand_var_with_modifiers(BStr::new(bad), &mut state, &ctx).is_err(),
            "expected error for ${{{}}}",
            String::from_utf8_lossy(bad)
        );
    }

    assert!(expand_var_with_modifiers(BStr::new(b"1:=foo"), &mut state, &ctx).is_err());
    assert!(expand_var_with_modifiers(BStr::new(b"?:=foo"), &mut state, &ctx).is_err());
    state.make_readonly(BStr::new("RO_MOD"));
    assert!(expand_var_with_modifiers(BStr::new(b"RO_MOD:=foo"), &mut state, &ctx).is_err());
}

#[test]
fn test_nested_and_quoted_delimiter_expansions() {
    let mut state = ShellState::new();
    let ctx = ExecutionContext::initial().unwrap();

    let expand_word_str = |word_src: &str, state: &mut ShellState| -> Vec<BString> {
        let mut builder = ASTBuilder::new();
        let tokens = tokenize(word_src.as_bytes()).unwrap();
        assert_eq!(tokens.len(), 1, "expected 1 token for {word_src:?}, got {tokens:?}");
        let crate::parser::Token::Word(raw_parts) = &tokens[0] else {
            panic!("expected word token");
        };
        let resolved = crate::parser::resolve_word_parts(&mut builder, raw_parts).unwrap();
        check_expand(&mut builder, &resolved, state, &ctx)
    };

    // 1. ${...} with quoted, escaped, and nested '}'
    state.unset_var(BStr::new("x"));
    state.unset_var(BStr::new("y"));
    assert_eq!(expand_word_str("${x:-\"}\"}", &mut state), vec![BString::from("}")]);
    assert_eq!(expand_word_str("${x:-\"\\\"}\"}", &mut state), vec![BString::from("\"}")]);
    assert_eq!(expand_word_str("${x:-\"`printf '}'`\"}", &mut state), vec![BString::from("}")]);
    assert_eq!(expand_word_str("${x:-'}'}", &mut state), vec![BString::from("}")]);
    assert_eq!(expand_word_str("${x:-\\}}", &mut state), vec![BString::from("}")]);
    assert_eq!(expand_word_str("\"${x:-\"}\"}\"", &mut state), vec![BString::from("}")]);
    assert_eq!(expand_word_str("${x:-${y:-inner}}", &mut state), vec![BString::from("inner")]);
    assert_eq!(expand_word_str("${x:-$(printf \"}\")}", &mut state), vec![BString::from("}")]);

    // 2. $(...) with quoted/escaped ')', comments, and `case ... esac`
    assert_eq!(expand_word_str("$(printf \")\")", &mut state), vec![BString::from(")")]);
    assert_eq!(expand_word_str("$(printf \"\\\")\")", &mut state), vec![BString::from("\")")]);
    assert_eq!(expand_word_str("$(printf \"`printf ')'`\")", &mut state), vec![BString::from(")")]);
    assert_eq!(expand_word_str("$(printf ')')", &mut state), vec![BString::from(")")]);
    assert_eq!(expand_word_str("$(printf \\))", &mut state), vec![BString::from(")")]);
    assert_eq!(expand_word_str("\"$(printf \")\")\"", &mut state), vec![BString::from(")")]);
    assert_eq!(
        expand_word_str("$(printf ok # comment )\n)", &mut state),
        vec![BString::from("ok")]
    );
    assert_eq!(
        expand_word_str("$(case x in x) printf matched ;; esac)", &mut state),
        vec![BString::from("matched")]
    );
    assert_eq!(
        expand_word_str("$(case x in (x) printf matched ;; esac)", &mut state),
        vec![BString::from("matched")]
    );

    // 3. Double-quoted backticks with escaped `\"`
    assert_eq!(
        expand_word_str("\"hello `printf \\\"world\\\"`\"", &mut state),
        vec![BString::from("hello world")]
    );

    // 4. Heredoc / expand_string with ${x:-"}"}, $(case ...), and \\\n line continuation
    let heredoc_expanded = expand_string(
        BStr::new("brace=${x:-\"}\"} case=$(case x in x) printf ok;; esac) cont=hel\\\nlo"),
        &mut state,
        &ctx,
    )
    .unwrap();
    assert_eq!(heredoc_expanded, "brace=} case=ok cont=hello");
}
