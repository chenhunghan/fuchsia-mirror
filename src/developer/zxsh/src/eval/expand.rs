// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::ExecutionContext;
use super::arithmetic::evaluate_arithmetic;
use super::glob::{WordChar, expand_glob, match_segment_glob, word_chars_to_bstring};
use super::simple::{split_assignment_flat, split_simple_command_args};
use super::state::ShellState;
use crate::builtins::is_builtin;
use crate::collections::FlatSet;
use crate::errors::{io_err_str, zx_status_str};
use crate::parser::ast::*;
use crate::parser::{
    QuoteMode, Token, parse_script, parse_subshell_command, resolve_word_parts,
    scan_arithmetic_expansion, scan_backtick_command_substitution, scan_braced_param,
    scan_command_substitution, scan_unbraced_var_name, tokenize, tokenize_modifier_word,
};
use crate::process::{clone_fd_to_action, make_pipe, read_fd_to_end};
use crate::relative;
use crate::subshell::{SubshellScriptArgs, spawn_subshell_process};
use bstr::{BStr, BString, ByteSlice};

fn run_command_substitution(
    cmd: &Command,
    state: &mut ShellState,
    ctx: &ExecutionContext,
    source_buf: &relative::Buffer,
) -> Result<BString, String> {
    let (read_fd, write_fd) = make_pipe()?;

    let stdin = ctx.stdin();
    let stdout = Some(&write_fd);
    let stderr = ctx.stderr();

    let mut actions = Vec::new();
    for (fd_opt, target) in [(stdin, Fd::STDIN), (stdout, Fd::STDOUT), (stderr, Fd::STDERR)] {
        if let Some(fd) = fd_opt {
            if let Some(action) = clone_fd_to_action(fd, target) {
                actions.push(action);
            }
        }
    }

    let proc =
        spawn_subshell_process(cmd, state, &mut actions, SubshellScriptArgs::Pass, source_buf)?;

    drop(write_fd);

    let output_bytes =
        read_fd_to_end(read_fd).map_err(|e| format!("Failed to read pipe: {}", io_err_str(e)))?;

    proc.wait_one(zx::Signals::PROCESS_TERMINATED, zx::MonotonicInstant::INFINITE)
        .map_err(|e| format!("Wait for subshell failed: {}", zx_status_str(e)))?;

    let info = proc
        .info()
        .map_err(|e| format!("Failed to get subshell process info: {}", zx_status_str(e)))?;
    state.record_cmd_sub_status(info.return_code as i32);

    let mut output = output_bytes;
    while output.last() == Some(&b'\n') {
        output.pop();
    }
    Ok(BString::from(output))
}

fn is_special_param(b: u8) -> bool {
    matches!(b, b'@' | b'*' | b'#' | b'?' | b'-' | b'$' | b'!')
}

fn is_valid_var_name(bytes: &[u8]) -> bool {
    if let Some((&first, rest)) = bytes.split_first() {
        (first.is_ascii_alphabetic() || first == b'_')
            && rest.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'_')
    } else {
        false
    }
}

fn is_pure_param_name(bytes: &[u8]) -> bool {
    (bytes.len() == 1 && is_special_param(bytes[0]))
        || (!bytes.is_empty() && bytes.iter().all(|b| b.is_ascii_digit()))
        || is_valid_var_name(bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NullCondition {
    UnsetOnly,
    UnsetOrNull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StripLength {
    Shortest,
    Longest,
}

enum Modifier<'a> {
    Length,
    Default(&'a BStr, NullCondition),
    Assign(&'a BStr, NullCondition),
    Error(Option<&'a BStr>, NullCondition),
    Alternative(&'a BStr, NullCondition),
    RemovePrefix(&'a BStr, StripLength),
    RemoveSuffix(&'a BStr, StripLength),
}

fn parse_modifier<'a>(name: &'a BStr) -> Result<(&'a BStr, Option<Modifier<'a>>), String> {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return Err("Bad substitution".to_string());
    }

    if bytes[0] == b'#' {
        if bytes.len() == 1 {
            return Ok((name, None));
        }
        let after_hash = &bytes[1..];
        if is_pure_param_name(after_hash) {
            return Ok((BStr::new(after_hash), Some(Modifier::Length)));
        }
        if !matches!(after_hash[0], b':' | b'-' | b'=' | b'?' | b'+' | b'#' | b'%') {
            return Err(format!("{}: bad substitution", name));
        }
    }

    let var_len = if is_special_param(bytes[0]) {
        1
    } else if bytes[0].is_ascii_digit() {
        bytes.iter().take_while(|b| b.is_ascii_digit()).count()
    } else if bytes[0].is_ascii_alphabetic() || bytes[0] == b'_' {
        bytes.iter().take_while(|&&b| b.is_ascii_alphanumeric() || b == b'_').count()
    } else {
        return Err(format!("{}: bad substitution", name));
    };

    let var_name = BStr::new(&bytes[..var_len]);
    let rest = &bytes[var_len..];
    if rest.is_empty() {
        return Ok((var_name, None));
    }

    let modifier = if let Some(word) = rest.strip_prefix(b":-") {
        Modifier::Default(BStr::new(word), NullCondition::UnsetOrNull)
    } else if let Some(word) = rest.strip_prefix(b"-") {
        Modifier::Default(BStr::new(word), NullCondition::UnsetOnly)
    } else if let Some(word) = rest.strip_prefix(b":=") {
        Modifier::Assign(BStr::new(word), NullCondition::UnsetOrNull)
    } else if let Some(word) = rest.strip_prefix(b"=") {
        Modifier::Assign(BStr::new(word), NullCondition::UnsetOnly)
    } else if let Some(msg) = rest.strip_prefix(b":?") {
        let opt_msg = if msg.is_empty() { None } else { Some(BStr::new(msg)) };
        Modifier::Error(opt_msg, NullCondition::UnsetOrNull)
    } else if let Some(msg) = rest.strip_prefix(b"?") {
        let opt_msg = if msg.is_empty() { None } else { Some(BStr::new(msg)) };
        Modifier::Error(opt_msg, NullCondition::UnsetOnly)
    } else if let Some(word) = rest.strip_prefix(b":+") {
        Modifier::Alternative(BStr::new(word), NullCondition::UnsetOrNull)
    } else if let Some(word) = rest.strip_prefix(b"+") {
        Modifier::Alternative(BStr::new(word), NullCondition::UnsetOnly)
    } else if let Some(pat) = rest.strip_prefix(b"##") {
        Modifier::RemovePrefix(BStr::new(pat), StripLength::Longest)
    } else if let Some(pat) = rest.strip_prefix(b"#") {
        Modifier::RemovePrefix(BStr::new(pat), StripLength::Shortest)
    } else if let Some(pat) = rest.strip_prefix(b"%%") {
        Modifier::RemoveSuffix(BStr::new(pat), StripLength::Longest)
    } else if let Some(pat) = rest.strip_prefix(b"%") {
        Modifier::RemoveSuffix(BStr::new(pat), StripLength::Shortest)
    } else {
        return Err(format!("{}: bad substitution", name));
    };

    Ok((var_name, Some(modifier)))
}

fn parse_and_expand_modifier_to_elems(
    modifier_str: &BStr,
    quote_mode: QuoteMode,
    state: &mut ShellState,
    ctx: &ExecutionContext,
) -> Result<Vec<PreSplitElem>, String> {
    if modifier_str.is_empty() {
        return Ok(Vec::new());
    }
    let mut builder = ASTBuilder::new();
    let raw_parts =
        tokenize_modifier_word(modifier_str.as_bytes(), quote_mode).map_err(|e| e.to_string())?;
    let resolved_parts = resolve_word_parts(&mut builder, &raw_parts).map_err(|e| e.to_string())?;
    let word_slice = builder.add_resolved_word(&resolved_parts);
    let slice = builder.get_slice(word_slice);
    let fields = expand_argument_to_pre_split_fields(
        slice,
        state,
        ctx,
        TildeColonMode::DoNotExpandAfterColons,
        FieldSplitMode::DoNotSplit,
        &builder,
    )?;
    Ok(fields
        .into_iter()
        .next()
        .unwrap_or_default()
        .into_iter()
        .map(|elem| match elem {
            PreSplitElem::Char(WordChar::Unquoted(b)) => PreSplitElem::Char(WordChar::Expansion(b)),
            other => other,
        })
        .collect())
}

fn parse_and_expand_modifier_to_word_chars(
    modifier_str: &BStr,
    quote_mode: QuoteMode,
    state: &mut ShellState,
    ctx: &ExecutionContext,
) -> Result<Vec<WordChar>, String> {
    let elems = parse_and_expand_modifier_to_elems(modifier_str, quote_mode, state, ctx)?;
    Ok(elems
        .into_iter()
        .filter_map(|elem| match elem {
            PreSplitElem::Char(wc) => Some(wc),
            PreSplitElem::EmptyQuote => None,
        })
        .collect())
}

/// Helper to parse and expand parameter modifier words.
pub fn parse_and_expand_modifier(
    modifier_str: &BStr,
    quote_mode: QuoteMode,
    state: &mut ShellState,
    ctx: &ExecutionContext,
) -> Result<BString, String> {
    let chars = parse_and_expand_modifier_to_word_chars(modifier_str, quote_mode, state, ctx)?;
    Ok(word_chars_to_bstring(&chars))
}

fn elems_to_bstring(elems: Vec<PreSplitElem>) -> BString {
    let bytes: Vec<u8> = elems
        .into_iter()
        .filter_map(|elem| match elem {
            PreSplitElem::Char(wc) => Some(wc.raw_byte()),
            PreSplitElem::EmptyQuote => None,
        })
        .collect();
    BString::from(bytes)
}

fn expand_var_to_elems(
    name: &BStr,
    quote_mode: QuoteMode,
    state: &mut ShellState,
    ctx: &ExecutionContext,
) -> Result<Vec<PreSplitElem>, String> {
    let (var_name, modifier) = parse_modifier(name)?;

    if state.opt_nounset {
        let is_unbound = state.get_var(var_name).is_none();
        if is_unbound {
            let needs_fail = match &modifier {
                None => true,
                Some(Modifier::Length)
                | Some(Modifier::RemovePrefix(_, _))
                | Some(Modifier::RemoveSuffix(_, _)) => true,
                _ => false,
            };
            if needs_fail {
                let msg = format!("{}: parameter not set", var_name);
                ctx.print_err(&msg)?;
                return Err(msg);
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum StripKind {
        Prefix,
        Suffix,
    }

    fn strip_pattern<'a>(
        val: &'a BStr,
        pattern: &[WordChar],
        kind: StripKind,
        length: StripLength,
    ) -> &'a BStr {
        let len = val.len();
        let val_bytes = val.as_bytes();

        let find_match_index = || {
            let indices: Box<dyn Iterator<Item = usize>> = match (kind, length) {
                (StripKind::Prefix, StripLength::Shortest) => Box::new(0..=len),
                (StripKind::Prefix, StripLength::Longest) => Box::new((0..=len).rev()),
                (StripKind::Suffix, StripLength::Shortest) => Box::new((0..=len).rev()),
                (StripKind::Suffix, StripLength::Longest) => Box::new(0..=len),
            };

            for i in indices {
                let candidate = match kind {
                    StripKind::Prefix => &val_bytes[..i],
                    StripKind::Suffix => &val_bytes[i..],
                };
                if match_segment_glob(pattern, BStr::new(candidate)) {
                    return Some(i);
                }
            }
            None
        };

        if let Some(i) = find_match_index() {
            match kind {
                StripKind::Prefix => BStr::new(&val_bytes[i..]),
                StripKind::Suffix => BStr::new(&val_bytes[..i]),
            }
        } else {
            val
        }
    }

    let bytes_to_elems = |bytes: &[u8]| -> Vec<PreSplitElem> {
        bytes.iter().map(|&b| PreSplitElem::Char(WordChar::Expansion(b))).collect()
    };

    if let Some(mod_type) = modifier {
        match mod_type {
            Modifier::Length => {
                let len = if var_name == "@" || var_name == "*" {
                    state.get_args().len()
                } else {
                    state.get_var(var_name).unwrap_or_default().len()
                };
                Ok(bytes_to_elems(len.to_string().as_bytes()))
            }
            Modifier::Default(_, null_condition)
            | Modifier::Assign(_, null_condition)
            | Modifier::Error(_, null_condition)
            | Modifier::Alternative(_, null_condition) => {
                if matches!(mod_type, Modifier::Assign(_, _))
                    && !is_valid_var_name(var_name.as_bytes())
                {
                    return Err(format!(
                        "{}: cannot assign in this way",
                        String::from_utf8_lossy(var_name.as_bytes())
                    ));
                }
                let val = state.get_var(var_name);
                let null_or_unset = val
                    .as_ref()
                    .map_or(true, |v| null_condition == NullCondition::UnsetOrNull && v.is_empty());
                match mod_type {
                    Modifier::Alternative(word, _) => {
                        if null_or_unset {
                            Ok(Vec::new())
                        } else {
                            parse_and_expand_modifier_to_elems(word, quote_mode, state, ctx)
                        }
                    }
                    _ if !null_or_unset => Ok(bytes_to_elems(val.unwrap().as_bytes())),
                    Modifier::Default(word, _) => {
                        parse_and_expand_modifier_to_elems(word, quote_mode, state, ctx)
                    }
                    Modifier::Assign(word, _) => {
                        let expanded_word =
                            parse_and_expand_modifier(word, quote_mode, state, ctx)?;
                        if state.is_readonly(var_name) {
                            return Err(format!(
                                "{}: is read only",
                                String::from_utf8_lossy(var_name.as_bytes())
                            ));
                        }
                        state.set_var(var_name, &expanded_word);
                        Ok(bytes_to_elems(expanded_word.as_bytes()))
                    }
                    Modifier::Error(opt_msg, _) => {
                        let msg = match opt_msg {
                            Some(w) => {
                                let expanded =
                                    parse_and_expand_modifier(w, quote_mode, state, ctx)?;
                                String::from_utf8_lossy(expanded.as_bytes()).into_owned()
                            }
                            None => format!(
                                "{}: parameter null or unset",
                                String::from_utf8_lossy(var_name.as_bytes())
                            ),
                        };
                        ctx.print_err(&msg)?;
                        Err(msg)
                    }
                    _ => unreachable!(),
                }
            }
            Modifier::RemovePrefix(pattern_word, length) => {
                let val = state.get_var(var_name).unwrap_or_default();
                let pattern = parse_and_expand_modifier_to_word_chars(
                    pattern_word,
                    QuoteMode::Unquoted,
                    state,
                    ctx,
                )?;
                Ok(bytes_to_elems(
                    strip_pattern(val.as_bstr(), &pattern, StripKind::Prefix, length).as_bytes(),
                ))
            }
            Modifier::RemoveSuffix(pattern_word, length) => {
                let val = state.get_var(var_name).unwrap_or_default();
                let pattern = parse_and_expand_modifier_to_word_chars(
                    pattern_word,
                    QuoteMode::Unquoted,
                    state,
                    ctx,
                )?;
                Ok(bytes_to_elems(
                    strip_pattern(val.as_bstr(), &pattern, StripKind::Suffix, length).as_bytes(),
                ))
            }
        }
    } else {
        let val = state.get_var(var_name).unwrap_or_default();
        Ok(bytes_to_elems(val.as_bytes()))
    }
}

/// Expands a shell parameter expression including modifiers (e.g. `${var:-default}`, `${#var}`,
/// `${var%pattern}`).
///
/// Evaluates defaults, alternate values, string slicing, length expansion, and prefix/suffix
/// stripping.
pub fn expand_var_with_modifiers(
    name: &BStr,
    state: &mut ShellState,
    ctx: &ExecutionContext,
) -> Result<BString, String> {
    let elems = expand_var_to_elems(name, QuoteMode::Unquoted, state, ctx)?;
    Ok(elems_to_bstring(elems))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TildeExpansionMode {
    Assignment,
    LeadingWordPart,
    SubsequentWordPart,
}

fn expand_tilde(word_part_string: &BStr, state: &ShellState, mode: TildeExpansionMode) -> BString {
    let home_directory = state.get_var(b"HOME").unwrap_or_default();

    match mode {
        TildeExpansionMode::Assignment => {
            let mut result_bytes =
                Vec::with_capacity(word_part_string.len() + home_directory.len());
            let mut is_first_colon_part = true;
            for colon_part in word_part_string.as_bytes().split(|&byte| byte == b':') {
                if !is_first_colon_part {
                    result_bytes.push(b':');
                }
                is_first_colon_part = false;

                let colon_part_string = BStr::new(colon_part);
                if colon_part_string == "~" {
                    result_bytes.extend_from_slice(home_directory.as_bytes());
                } else if colon_part_string.starts_with(b"~/") {
                    result_bytes.extend_from_slice(home_directory.as_bytes());
                    result_bytes.extend_from_slice(&colon_part[1..]);
                } else {
                    result_bytes.extend_from_slice(colon_part);
                }
            }
            BString::from(result_bytes)
        }
        TildeExpansionMode::LeadingWordPart => {
            if word_part_string == "~" {
                home_directory
            } else if word_part_string.starts_with(b"~/") {
                let mut result_bytes = Vec::from(home_directory);
                result_bytes.extend_from_slice(&word_part_string.as_bytes()[1..]);
                BString::from(result_bytes)
            } else {
                word_part_string.to_owned()
            }
        }
        TildeExpansionMode::SubsequentWordPart => word_part_string.to_owned(),
    }
}

#[derive(Clone)]
enum PreSplitElem {
    Char(WordChar),
    EmptyQuote,
}

impl PreSplitElem {
    fn is_ifs_whitespace(&self, ifs: &BStr) -> bool {
        match self {
            PreSplitElem::Char(wc) => wc.is_ifs_whitespace(ifs),
            PreSplitElem::EmptyQuote => false,
        }
    }

    fn is_ifs_non_whitespace(&self, ifs: &BStr) -> bool {
        match self {
            PreSplitElem::Char(wc) => wc.is_ifs_non_whitespace(ifs),
            PreSplitElem::EmptyQuote => false,
        }
    }
}

fn push_quoted_bytes(field: &mut Vec<PreSplitElem>, bytes: &[u8]) {
    if bytes.is_empty() {
        field.push(PreSplitElem::EmptyQuote);
    } else {
        for &byte in bytes {
            field.push(PreSplitElem::Char(WordChar::Quoted(byte)));
        }
    }
}

fn push_arg_bytes(field: &mut Vec<PreSplitElem>, bytes: &[u8], quote_mode: QuoteMode) {
    match quote_mode {
        QuoteMode::DoubleQuoted => push_quoted_bytes(field, bytes),
        QuoteMode::Unquoted => {
            for &byte in bytes {
                field.push(PreSplitElem::Char(WordChar::Expansion(byte)));
            }
        }
    }
}

fn expand_positional_args_to_fields(
    arguments: &[BString],
    quote_mode: QuoteMode,
    current_field: &mut Vec<PreSplitElem>,
    fields: &mut Vec<Vec<PreSplitElem>>,
) {
    if let Some((first, rest)) = arguments.split_first() {
        push_arg_bytes(current_field, first.as_bytes(), quote_mode);
        for arg in rest {
            if !current_field.is_empty() {
                fields.push(std::mem::take(current_field));
            }
            push_arg_bytes(current_field, arg.as_bytes(), quote_mode);
        }
    }
}

fn split_word_chars_by_ifs(word: &[PreSplitElem], ifs: &BStr) -> Vec<Vec<WordChar>> {
    let mut results = Vec::new();
    let mut start = 0;

    // Skip leading IFS whitespace
    while start < word.len() && word[start].is_ifs_whitespace(ifs) {
        start += 1;
    }

    if start >= word.len() {
        return results;
    }

    let mut current_field = Vec::new();
    let mut has_fields = false;
    let mut i = start;

    while i < word.len() {
        let w = &word[i];
        if w.is_ifs_whitespace(ifs) {
            let mut has_adjacent_non_whitespace = false;
            let mut next_i = i + 1;
            while next_i < word.len() {
                if word[next_i].is_ifs_whitespace(ifs) {
                    next_i += 1;
                } else if word[next_i].is_ifs_non_whitespace(ifs) {
                    has_adjacent_non_whitespace = true;
                    next_i += 1;
                    break;
                } else {
                    break;
                }
            }

            if has_adjacent_non_whitespace {
                while next_i < word.len() && word[next_i].is_ifs_whitespace(ifs) {
                    next_i += 1;
                }
            }

            results.push(std::mem::take(&mut current_field));
            has_fields = false;
            i = next_i;
        } else if w.is_ifs_non_whitespace(ifs) {
            let mut next_i = i + 1;
            while next_i < word.len() && word[next_i].is_ifs_whitespace(ifs) {
                next_i += 1;
            }

            results.push(std::mem::take(&mut current_field));
            has_fields = false;
            i = next_i;
        } else {
            if let PreSplitElem::Char(wc) = w {
                current_field.push(wc.clone());
            }
            has_fields = true;
            i += 1;
        }
    }

    if has_fields {
        results.push(current_field);
    }

    results
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TildeColonMode {
    ExpandAfterColons,
    DoNotExpandAfterColons,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldSplitMode {
    Split,
    DoNotSplit,
}

fn expand_argument_to_pre_split_fields(
    word_parts: &[WordPart],
    state: &mut ShellState,
    context: &ExecutionContext,
    tilde_colon_mode: TildeColonMode,
    field_split_mode: FieldSplitMode,
    buffer: &relative::Buffer,
) -> Result<Vec<Vec<PreSplitElem>>, String> {
    let mut fields: Vec<Vec<PreSplitElem>> = Vec::new();
    let mut current_field: Vec<PreSplitElem> = Vec::new();

    for (part_index, part) in word_parts.iter().enumerate() {
        match part.tag {
            WordPartTag::LITERAL => {
                let literal_string = part.text.as_bstr(buffer);
                let tilde_mode = if tilde_colon_mode == TildeColonMode::ExpandAfterColons {
                    TildeExpansionMode::Assignment
                } else if part_index == 0 {
                    TildeExpansionMode::LeadingWordPart
                } else {
                    TildeExpansionMode::SubsequentWordPart
                };
                let expanded_string = expand_tilde(literal_string, state, tilde_mode);
                for &byte in expanded_string.as_bytes() {
                    current_field.push(PreSplitElem::Char(WordChar::Unquoted(byte)));
                }
            }
            WordPartTag::QUOTED_LITERAL => {
                let quoted_string = part.text.as_bstr(buffer);
                push_quoted_bytes(&mut current_field, quoted_string.as_bytes());
            }
            WordPartTag::QUOTED_VAR => {
                let variable_name = part.text.as_bstr(buffer);
                if variable_name == "@" && field_split_mode == FieldSplitMode::Split {
                    let arguments = state.get_args();
                    expand_positional_args_to_fields(
                        &arguments,
                        QuoteMode::DoubleQuoted,
                        &mut current_field,
                        &mut fields,
                    );
                } else {
                    let elems = expand_var_to_elems(
                        variable_name,
                        QuoteMode::DoubleQuoted,
                        state,
                        context,
                    )?;
                    let value = elems_to_bstring(elems);
                    push_quoted_bytes(&mut current_field, value.as_bytes());
                }
            }
            WordPartTag::QUOTED_COMMAND_SUBSTITUTION => {
                let command = part.command.as_ref(buffer);
                let value = run_command_substitution(command, state, context, buffer)?;
                push_quoted_bytes(&mut current_field, value.as_bytes());
            }
            WordPartTag::VAR => {
                let variable_name = part.text.as_bstr(buffer);
                if (variable_name == "@" || variable_name == "*")
                    && field_split_mode == FieldSplitMode::Split
                {
                    let arguments = state.get_args();
                    expand_positional_args_to_fields(
                        &arguments,
                        QuoteMode::Unquoted,
                        &mut current_field,
                        &mut fields,
                    );
                } else {
                    let elems =
                        expand_var_to_elems(variable_name, QuoteMode::Unquoted, state, context)?;
                    current_field.extend(elems);
                }
            }
            WordPartTag::COMMAND_SUBSTITUTION => {
                let command = part.command.as_ref(buffer);
                let value = run_command_substitution(command, state, context, buffer)?;
                for &byte in value.as_bytes() {
                    current_field.push(PreSplitElem::Char(WordChar::Expansion(byte)));
                }
            }
            WordPartTag::ARITHMETIC => {
                let expression = part.text.as_bstr(buffer);
                let value = evaluate_arithmetic(expression, state, context)?.to_string();
                for &byte in value.as_bytes() {
                    current_field.push(PreSplitElem::Char(WordChar::Expansion(byte)));
                }
            }
            WordPartTag::QUOTED_ARITHMETIC => {
                let expression = part.text.as_bstr(buffer);
                let value = evaluate_arithmetic(expression, state, context)?.to_string();
                push_quoted_bytes(&mut current_field, value.as_bytes());
            }
            _ => unreachable!(),
        }
    }

    if !current_field.is_empty() {
        fields.push(current_field);
    }

    Ok(fields)
}

/// Expands an AST argument slice into sequences of `WordChar`s preserving quote metadata.
///
/// Handles tilde expansion, parameter expansion, command substitution, arithmetic expansion,
/// and optional IFS field splitting while distinguishing quoted literals from unquoted wildcards.
pub fn expand_argument_to_word_chars(
    word_parts: &[WordPart],
    state: &mut ShellState,
    context: &ExecutionContext,
    tilde_colon_mode: TildeColonMode,
    field_split_mode: FieldSplitMode,
    buffer: &relative::Buffer,
) -> Result<Vec<Vec<WordChar>>, String> {
    let internal_field_separator = state.get_var(b"IFS").unwrap_or_else(|| BString::from(" \t\n"));
    let fields = expand_argument_to_pre_split_fields(
        word_parts,
        state,
        context,
        tilde_colon_mode,
        field_split_mode,
        buffer,
    )?;

    if field_split_mode == FieldSplitMode::Split && !internal_field_separator.is_empty() {
        let mut split_fields = Vec::new();
        for field in fields {
            split_fields
                .extend(split_word_chars_by_ifs(&field, internal_field_separator.as_bstr()));
        }
        Ok(split_fields)
    } else {
        Ok(fields
            .into_iter()
            .map(|field| {
                field
                    .into_iter()
                    .filter_map(|elem| match elem {
                        PreSplitElem::Char(wc) => Some(wc),
                        PreSplitElem::EmptyQuote => None,
                    })
                    .collect()
            })
            .collect())
    }
}

/// Expands an AST argument word into a list of resulting byte strings.
///
/// Performs full POSIX word expansion including field splitting and glob pattern expansion
/// (unless `noglob` is set).
pub fn expand_argument(
    arg: &[WordPart],
    state: &mut ShellState,
    ctx: &ExecutionContext,
    buf: &relative::Buffer,
) -> Result<Vec<BString>, String> {
    let word_chars_list = expand_argument_to_word_chars(
        arg,
        state,
        ctx,
        TildeColonMode::DoNotExpandAfterColons,
        FieldSplitMode::Split,
        buf,
    )?;
    let mut final_results = Vec::new();
    for word in word_chars_list {
        if state.opt_noglob {
            final_results.push(word_chars_to_bstring(&word));
        } else {
            let cwd_path = state.cwd().to_path().unwrap_or_else(|_| std::path::Path::new("."));
            let matches = expand_glob(&word, cwd_path);
            final_results.extend(matches);
        }
    }
    Ok(final_results)
}

/// Expands an AST argument word into a single byte string without IFS field splitting or globbing.
///
/// Used for word expansions in contexts like double quotes or case statements.
pub fn expand_argument_no_split(
    arg: &[WordPart],
    state: &mut ShellState,
    ctx: &ExecutionContext,
    buf: &relative::Buffer,
) -> Result<BString, String> {
    let word_chars_list = expand_argument_to_word_chars(
        arg,
        state,
        ctx,
        TildeColonMode::DoNotExpandAfterColons,
        FieldSplitMode::DoNotSplit,
        buf,
    )?;
    Ok(word_chars_list.first().map(|w| word_chars_to_bstring(w)).unwrap_or_default())
}

/// Expands the value side of a variable assignment statement (e.g. `VAR=value`).
///
/// Supports tilde expansion after colons inside the assignment value (`PATH=~/bin:~/usr/bin`).
pub fn expand_assignment_value(
    val_start: &BStr,
    remaining: &[WordPart],
    state: &mut ShellState,
    ctx: &ExecutionContext,
    buf: &relative::Buffer,
) -> Result<BString, String> {
    let mut parts = Vec::new();
    let mut builder = ASTBuilder::new();
    if !val_start.is_empty() {
        parts.push(ResolvedWordPart::Literal(val_start.to_owned()));
    }
    for p in remaining {
        match p.tag {
            WordPartTag::LITERAL => parts.push(ResolvedWordPart::Literal(p.text.to_bstring(buf))),
            WordPartTag::VAR => parts.push(ResolvedWordPart::Var(p.text.to_bstring(buf))),
            WordPartTag::QUOTED_LITERAL => {
                parts.push(ResolvedWordPart::QuotedLiteral(p.text.to_bstring(buf)))
            }
            WordPartTag::QUOTED_VAR => {
                parts.push(ResolvedWordPart::QuotedVar(p.text.to_bstring(buf)))
            }
            WordPartTag::COMMAND_SUBSTITUTION => {
                let cmd = p.command.as_ref(buf);
                let bytes = cmd.serialize(buf);
                let off = builder.import_serialized_ast(&bytes);
                parts.push(ResolvedWordPart::CommandSubstitution(off));
            }
            WordPartTag::QUOTED_COMMAND_SUBSTITUTION => {
                let cmd = p.command.as_ref(buf);
                let bytes = cmd.serialize(buf);
                let off = builder.import_serialized_ast(&bytes);
                parts.push(ResolvedWordPart::QuotedCommandSubstitution(off));
            }
            WordPartTag::ARITHMETIC => {
                parts.push(ResolvedWordPart::Arithmetic(p.text.to_bstring(buf)))
            }
            WordPartTag::QUOTED_ARITHMETIC => {
                parts.push(ResolvedWordPart::QuotedArithmetic(p.text.to_bstring(buf)))
            }
            _ => unreachable!(),
        }
    }
    let word_slice = builder.add_resolved_word(&parts);
    let slice = builder.get_slice(word_slice);
    let word_chars_list = expand_argument_to_word_chars(
        slice,
        state,
        ctx,
        TildeColonMode::ExpandAfterColons,
        FieldSplitMode::DoNotSplit,
        &builder,
    )?;
    Ok(word_chars_list.first().map(|w| word_chars_to_bstring(w)).unwrap_or_default())
}

fn word_has_side_effects(
    parts: &[WordPart],
    state: &ShellState,
    buffer: &relative::Buffer,
) -> bool {
    for part in parts {
        match part.tag {
            WordPartTag::LITERAL | WordPartTag::QUOTED_LITERAL => {}
            WordPartTag::COMMAND_SUBSTITUTION
            | WordPartTag::QUOTED_COMMAND_SUBSTITUTION
            | WordPartTag::ARITHMETIC
            | WordPartTag::QUOTED_ARITHMETIC => return true,
            WordPartTag::VAR | WordPartTag::QUOTED_VAR => {
                if state.opt_nounset {
                    return true;
                }
                let var_text = part.text.as_bstr(buffer);
                if !matches!(parse_modifier(var_text), Ok((_, None))) {
                    return true;
                }
            }
            _ => return true,
        }
    }
    false
}

pub fn needs_subshell_process<'a>(
    mut command: &'a Command,
    state: &ShellState,
    buffer: &'a relative::Buffer,
) -> bool {
    loop {
        match command.tag {
            CommandTag::SIMPLE => {
                let (assignments, command_arguments) = split_simple_command_args(command, buffer);
                if command_arguments.is_empty() {
                    return true;
                }

                for assign_slice in &assignments {
                    let parts = assign_slice.as_slice(buffer);
                    let (name, _, _) = split_assignment_flat(parts, buffer);
                    if state.is_readonly(name) || word_has_side_effects(parts, state, buffer) {
                        return true;
                    }
                }

                for arg_slice in &command_arguments {
                    let parts = arg_slice.as_slice(buffer);
                    if word_has_side_effects(parts, state, buffer) {
                        return true;
                    }
                }

                let first_argument = command_arguments[0].as_slice(buffer);
                if first_argument.is_empty() {
                    return true;
                }

                let mut cmd_name = Vec::new();
                for (part_index, part) in first_argument.iter().enumerate() {
                    match part.tag {
                        WordPartTag::LITERAL => {
                            let literal = part.text.as_bstr(buffer);
                            if part_index == 0 && (literal == "~" || literal.starts_with(b"~/")) {
                                return true;
                            }
                            if !state.opt_noglob
                                && literal
                                    .as_bytes()
                                    .iter()
                                    .any(|&b| matches!(b, b'*' | b'?' | b'['))
                            {
                                return true;
                            }
                            cmd_name.extend_from_slice(literal.as_bytes());
                        }
                        WordPartTag::QUOTED_LITERAL => {
                            cmd_name.extend_from_slice(part.text.as_bstr(buffer).as_bytes());
                        }
                        _ => return true,
                    }
                }

                if cmd_name.is_empty() {
                    return true;
                }

                let cmd_bstr = BStr::new(&cmd_name);
                return is_builtin(cmd_bstr)
                    || state.get_function(cmd_bstr).is_some()
                    || state.aliases.contains_key(cmd_bstr);
            }
            CommandTag::REDIRECT => {
                for redirect in command.redirects.as_slice(buffer) {
                    match redirect.tag {
                        RedirectTag::TO_FILE
                        | RedirectTag::FROM_FILE
                        | RedirectTag::READ_WRITE
                        | RedirectTag::DUP_FD => {
                            if word_has_side_effects(
                                redirect.filename.as_slice(buffer),
                                state,
                                buffer,
                            ) {
                                return true;
                            }
                        }
                        RedirectTag::HERE_DOC => {
                            if redirect.expand != 0 {
                                let body = redirect.body.as_slice(buffer);
                                if body.contains(&b'$') || body.contains(&b'`') {
                                    return true;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                command = command.left.as_ref(buffer);
            }
            _ => return true,
        }
    }
}

fn eval_command_substitution_bytes(
    inner_bytes: &[u8],
    state: &mut ShellState,
    context: &ExecutionContext,
) -> Result<BString, String> {
    let mut sub_builder = ASTBuilder::new();
    let command_pointer =
        parse_subshell_command(&mut sub_builder, inner_bytes).map_err(|e| e.to_string())?;
    let command = sub_builder.get_ref(command_pointer);
    run_command_substitution(command, state, context, &sub_builder)
}

fn expand_dollar(
    bytes: &[u8],
    index: &mut usize,
    state: &mut ShellState,
    context: &ExecutionContext,
    result_bytes: &mut Vec<u8>,
) -> Result<(), String> {
    if *index + 1 < bytes.len() && bytes[*index + 1] == b'(' {
        *index += 2; // consume '$' and '('
        if *index < bytes.len() && bytes[*index] == b'(' {
            *index += 1; // consume second '('
            let inner_bytes = match scan_arithmetic_expansion(bytes, index) {
                Ok(s) | Err(s) => s,
            };
            let expanded_inner = expand_string(inner_bytes.as_bstr(), state, context)?;
            let value = evaluate_arithmetic(expanded_inner.as_bstr(), state, context)?;
            result_bytes.extend_from_slice(value.to_string().as_bytes());
        } else {
            let inner_bytes = match scan_command_substitution(bytes, index) {
                Ok(s) | Err(s) => s,
            };
            let value = eval_command_substitution_bytes(inner_bytes.as_bytes(), state, context)?;
            result_bytes.extend_from_slice(value.as_bytes());
        }
    } else if *index + 1 < bytes.len() && bytes[*index + 1] == b'{' {
        *index += 2; // consume '$' and '{'
        let var_expr = match scan_braced_param(bytes, index, QuoteMode::Unquoted) {
            Ok(s) | Err(s) => s,
        };
        let value = expand_var_with_modifiers(var_expr.as_bstr(), state, context)?;
        result_bytes.extend_from_slice(value.as_bytes());
    } else {
        *index += 1; // consume '$'
        if let Some(var_name) = scan_unbraced_var_name(bytes, index) {
            let value = expand_var_with_modifiers(var_name.as_bstr(), state, context)?;
            result_bytes.extend_from_slice(value.as_bytes());
        } else {
            result_bytes.push(b'$');
        }
    }
    Ok(())
}

/// Expands variable and arithmetic expressions inside an unquoted string or heredoc body.
pub fn expand_string(
    string: &BStr,
    state: &mut ShellState,
    context: &ExecutionContext,
) -> Result<BString, String> {
    let mut result_bytes: Vec<u8> = Vec::new();
    let bytes = string.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b'\\' => {
                if index + 1 < bytes.len() {
                    let next_byte = bytes[index + 1];
                    if next_byte == b'\n' {
                        index += 2;
                    } else if next_byte == b'$' || next_byte == b'\\' || next_byte == b'`' {
                        result_bytes.push(next_byte);
                        index += 2;
                    } else {
                        result_bytes.push(b'\\');
                        index += 1;
                    }
                } else {
                    result_bytes.push(b'\\');
                    index += 1;
                }
            }
            b'$' => {
                expand_dollar(bytes, &mut index, state, context, &mut result_bytes)?;
            }
            b'`' => {
                index += 1;
                match scan_backtick_command_substitution(bytes, &mut index, QuoteMode::Unquoted) {
                    Ok(inner_bytes) => {
                        let value = eval_command_substitution_bytes(
                            inner_bytes.as_bytes(),
                            state,
                            context,
                        )?;
                        result_bytes.extend_from_slice(value.as_bytes());
                    }
                    Err(inner_bytes) => {
                        result_bytes.push(b'`');
                        result_bytes.extend_from_slice(inner_bytes.as_bytes());
                    }
                }
            }
            _ => {
                result_bytes.push(byte);
                index += 1;
            }
        }
    }
    Ok(BString::from(result_bytes))
}

/// Reads a prompt variable by name, falling back to `default_prompt` if unset,
/// and expands variable and arithmetic expressions inside it.
pub fn expand_prompt(
    var_name: &BStr,
    default_prompt: &BStr,
    state: &mut ShellState,
    context: &ExecutionContext,
) -> BString {
    let prompt_owned = state.get_var(var_name);
    let prompt_raw = match &prompt_owned {
        Some(s) => s.as_ref(),
        None => default_prompt,
    };
    let saved_status = state.get_var(BStr::new("?"));
    let saved_cmd_sub = state.take_cmd_sub_status();
    let res = match expand_string(prompt_raw, state, context) {
        Ok(expanded) => expanded,
        Err(_) => BString::from(default_prompt),
    };
    state.take_cmd_sub_status();
    if let Some(code) = saved_cmd_sub {
        state.record_cmd_sub_status(code);
    }
    if let Some(status) = saved_status {
        state.set_var(BStr::new("?"), status.as_bstr());
    }
    res
}

/// Extracts the literal command string if the argument word consists solely of a single unquoted
/// literal.
pub fn get_literal_command_name(arg: &[WordPart], buf: &relative::Buffer) -> Option<BString> {
    if arg.len() == 1 {
        match arg[0].tag {
            WordPartTag::LITERAL => Some(arg[0].text.to_bstring(buf)),
            _ => None,
        }
    } else {
        None
    }
}

/// Appends additional argument words to an existing AST `Command` node in the buffer.
///
/// Recursively traverses pipelines, sequences, logical lists, and control flow branches to append
/// to the trailing command.
pub fn append_args_to_command(
    builder: &mut ASTBuilder,
    cmd_ptr: relative::Ptr<Command>,
    extra_args: &[relative::Slice<WordPart>],
) -> relative::Ptr<Command> {
    if extra_args.is_empty() {
        return cmd_ptr;
    }
    let tag = builder.get_ref(cmd_ptr).tag;
    match tag {
        CommandTag::SIMPLE => {
            let mut all_refs = Vec::new();
            {
                let cmd = builder.get_ref(cmd_ptr);
                let old_args_slice = cmd.simple_args.as_slice(builder);
                for &old_arg in old_args_slice {
                    all_refs.push(old_arg);
                }
            }
            for &new_arg in extra_args {
                all_refs.push(new_arg);
            }
            let new_simple_args = builder.add_argument_refs(&all_refs);

            let cmd_mut = builder.get_mut(cmd_ptr);
            cmd_mut.simple_args = new_simple_args;

            cmd_ptr
        }
        CommandTag::PIPELINE => {
            let right_ptr = {
                let cmd = builder.get_ref(cmd_ptr);
                cmd.right
            };
            append_args_to_command(builder, right_ptr, extra_args);
            cmd_ptr
        }
        CommandTag::SEQUENCE => {
            let last_ptr = {
                let cmd = builder.get_ref(cmd_ptr);
                let seq = cmd.sequence.as_slice(builder);
                seq.last().copied()
            };
            if let Some(last_ptr) = last_ptr {
                append_args_to_command(builder, last_ptr, extra_args);
            }
            cmd_ptr
        }
        CommandTag::LOGICAL_AND | CommandTag::LOGICAL_OR => {
            let right_ptr = {
                let cmd = builder.get_ref(cmd_ptr);
                cmd.right
            };
            append_args_to_command(builder, right_ptr, extra_args);
            cmd_ptr
        }
        CommandTag::IF => {
            let then_ptr = {
                let cmd = builder.get_ref(cmd_ptr);
                cmd.then_branch
            };
            append_args_to_command(builder, then_ptr, extra_args);
            cmd_ptr
        }
        CommandTag::WHILE | CommandTag::UNTIL | CommandTag::FOR => {
            let body_ptr = {
                let cmd = builder.get_ref(cmd_ptr);
                cmd.then_branch
            };
            append_args_to_command(builder, body_ptr, extra_args);
            cmd_ptr
        }
        CommandTag::NOT => {
            let left_ptr = {
                let cmd = builder.get_ref(cmd_ptr);
                cmd.left
            };
            append_args_to_command(builder, left_ptr, extra_args);
            cmd_ptr
        }
        _ => cmd_ptr,
    }
}

/// Represents the result of expanding an alias definition.
pub enum ExpandedCommand {
    /// A list of simple argument words resulting from alias expansion.
    Words(Vec<relative::Slice<WordPart>>),
    /// A complex compound AST command resulting from alias expansion.
    Command(relative::Ptr<Command>),
}

/// Evaluates and expands alias definitions for the initial command word, detecting recursive
/// expansion cycles.
pub fn expand_alias(
    builder: &mut ASTBuilder,
    args: &[relative::Slice<WordPart>],
    state: &ShellState,
    ctx: &mut ExecutionContext,
    active: &mut FlatSet<BString>,
) -> Result<Option<ExpandedCommand>, String> {
    if args.is_empty() {
        return Ok(None);
    }
    let name_opt = {
        let arg0 = builder.get_slice(args[0]);
        get_literal_command_name(arg0, builder)
    };
    if let Some(name) = name_opt {
        if let Some(val) = state.aliases.get(&name).cloned() {
            if !ctx.active_aliases.contains(&name) && !active.contains(&name) {
                let val_tokens = tokenize(val.as_bytes()).map_err(|e| e.to_string())?;
                let is_simple = val_tokens.iter().all(|t| matches!(t, Token::Word(_)))
                    && val_tokens.first().and_then(|t| t.as_unquoted_bstr())
                        != Some(BStr::new(b"!"));
                if is_simple {
                    let mut new_words = Vec::new();
                    for t in &val_tokens {
                        if let Token::Word(parts) = t {
                            let temp_parts =
                                resolve_word_parts(builder, parts).map_err(|e| e.to_string())?;
                            let word_slice = builder.add_resolved_word(&temp_parts);
                            new_words.push(word_slice);
                        }
                    }
                    active.insert(name.clone());

                    let mut resolved_replacement = new_words.clone();
                    if let Some(ExpandedCommand::Words(nested)) =
                        expand_alias(builder, &new_words, state, ctx, active)?
                    {
                        resolved_replacement = nested;
                    }
                    active.remove(&name);

                    let mut final_args = resolved_replacement;
                    if val.ends_with(b" ") && args.len() > 1 {
                        let mut remaining_refs = args[1..].to_vec();
                        if let Some(ExpandedCommand::Words(nested)) =
                            expand_alias(builder, &remaining_refs, state, ctx, active)?
                        {
                            remaining_refs = nested;
                        }
                        final_args.extend(remaining_refs);
                    } else {
                        for &arg in &args[1..] {
                            final_args.push(arg);
                        }
                    }
                    return Ok(Some(ExpandedCommand::Words(final_args)));
                } else {
                    let val_cmds = parse_script(builder, &val_tokens).map_err(|e| e.to_string())?;
                    let val_cmd_ptr = builder.add_sequence_or_single(&val_cmds);
                    let merged = append_args_to_command(builder, val_cmd_ptr, &args[1..]);
                    return Ok(Some(ExpandedCommand::Command(merged)));
                }
            }
        }
    }
    Ok(None)
}
