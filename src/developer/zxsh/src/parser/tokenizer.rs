// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::string::parse_int;

use super::{IncompleteReason, ParseError, RawWordPart, Token};
use bstr::{BString, ByteSlice};

const WHITESPACE: u8 = 1 << 0; // ' ', '\t', '\r'
const NEWLINE: u8 = 1 << 1; // '\n'
const META_CHAR: u8 = 1 << 2; // ';', '|', '&', '>', '<', '(', ')'
const QUOTE_CHAR: u8 = 1 << 3; // '\'', '"', '\\', '`'
const IDENT_START: u8 = 1 << 4; // 'a'..='z', 'A'..='Z', '_'
const IDENT_CHAR: u8 = 1 << 5; // 'a'..='z', 'A'..='Z', '0'..='9', '_'
const DIGIT: u8 = 1 << 6; // '0'..='9'
const VAR_SPECIAL: u8 = 1 << 7; // '#', '?', '@', '*', '$', '!', '-'

const CHAR_CLASS_TABLE_SIZE: usize = 256;

pub const fn make_char_class_table() -> [u8; CHAR_CLASS_TABLE_SIZE] {
    let mut table = [0u8; CHAR_CLASS_TABLE_SIZE];
    let mut i = 0;
    while i < CHAR_CLASS_TABLE_SIZE {
        let ch = i as u8;
        let mut class = 0;
        if ch == b' ' || ch == b'\t' || ch == b'\r' {
            class |= WHITESPACE;
        }
        if ch == b'\n' {
            class |= NEWLINE;
        }
        if ch == b';'
            || ch == b'|'
            || ch == b'&'
            || ch == b'>'
            || ch == b'<'
            || ch == b'('
            || ch == b')'
        {
            class |= META_CHAR;
        }
        if ch == b'\'' || ch == b'"' || ch == b'\\' || ch == b'`' {
            class |= QUOTE_CHAR;
        }
        if (ch >= b'a' && ch <= b'z') || (ch >= b'A' && ch <= b'Z') || ch == b'_' {
            class |= IDENT_START;
        }
        if (ch >= b'a' && ch <= b'z')
            || (ch >= b'A' && ch <= b'Z')
            || (ch >= b'0' && ch <= b'9')
            || ch == b'_'
        {
            class |= IDENT_CHAR;
        }
        if ch >= b'0' && ch <= b'9' {
            class |= DIGIT;
        }
        if ch == b'#'
            || ch == b'?'
            || ch == b'@'
            || ch == b'*'
            || ch == b'$'
            || ch == b'!'
            || ch == b'-'
        {
            class |= VAR_SPECIAL;
        }
        table[i] = class;
        i += 1;
    }
    table
}

static CHAR_CLASSES: [u8; CHAR_CLASS_TABLE_SIZE] = make_char_class_table();

fn trim_start_tabs(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < bytes.len() && bytes[start] == b'\t' {
        start += 1;
    }
    &bytes[start..]
}

fn skip_single_quoted(bytes: &[u8], index: &mut usize) -> bool {
    while *index < bytes.len() {
        let ch = bytes[*index];
        *index += 1;
        if ch == b'\'' {
            return true;
        }
    }
    false
}

fn skip_backtick_quoted(bytes: &[u8], index: &mut usize) -> bool {
    while *index < bytes.len() {
        let ch = bytes[*index];
        *index += 1;
        if ch == b'`' {
            return true;
        }
        if ch == b'\\' && *index < bytes.len() {
            *index += 1;
        }
    }
    false
}

fn skip_dollar_expansion(bytes: &[u8], index: &mut usize, quote_mode: QuoteMode) -> bool {
    if *index >= bytes.len() {
        return true;
    }
    if bytes[*index] == b'{' {
        *index += 1;
        scan_braced_param(bytes, index, quote_mode).is_ok()
    } else if bytes[*index] == b'(' {
        *index += 1;
        if *index < bytes.len() && bytes[*index] == b'(' {
            *index += 1;
            scan_arithmetic_expansion(bytes, index).is_ok()
        } else {
            scan_command_substitution(bytes, index).is_ok()
        }
    } else {
        let _ = scan_unbraced_var_name(bytes, index);
        true
    }
}

fn skip_double_quoted(bytes: &[u8], index: &mut usize) -> bool {
    while *index < bytes.len() {
        let ch = bytes[*index];
        *index += 1;
        match ch {
            b'"' => return true,
            b'\\' => {
                if *index < bytes.len() {
                    *index += 1;
                }
            }
            b'`' => {
                if !skip_backtick_quoted(bytes, index) {
                    return false;
                }
            }
            b'$' => {
                if !skip_dollar_expansion(bytes, index, QuoteMode::DoubleQuoted) {
                    return false;
                }
            }
            _ => {}
        }
    }
    false
}

fn starts_with_pattern_modifier(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let first_class = CHAR_CLASSES[bytes[0] as usize];
    let var_len = if (first_class & (DIGIT | VAR_SPECIAL)) != 0 {
        if (first_class & DIGIT) != 0 {
            bytes.iter().take_while(|&&b| (CHAR_CLASSES[b as usize] & DIGIT) != 0).count()
        } else {
            1
        }
    } else if (first_class & IDENT_START) != 0 {
        bytes.iter().take_while(|&&b| (CHAR_CLASSES[b as usize] & IDENT_CHAR) != 0).count()
    } else {
        return false;
    };
    matches!(bytes.get(var_len), Some(b'#' | b'%'))
}

fn is_assignment_word_bytes(word: &[u8]) -> bool {
    let Some(eq_pos) = word.iter().position(|&b| b == b'=') else {
        return false;
    };
    let Some((&first, rest)) = word[..eq_pos].split_first() else {
        return false;
    };
    (CHAR_CLASSES[first as usize] & IDENT_START) != 0
        && rest.iter().all(|&b| (CHAR_CLASSES[b as usize] & IDENT_CHAR) != 0)
}

/// Scans an unbraced parameter name (e.g. `$1`, `$?`, `$FOO`) starting immediately after `$`.
pub fn scan_unbraced_var_name(bytes: &[u8], index: &mut usize) -> Option<BString> {
    let first_ch = *bytes.get(*index)?;
    let first_class = CHAR_CLASSES[first_ch as usize];
    let start = *index;
    if (first_class & (DIGIT | VAR_SPECIAL)) != 0 {
        *index += 1;
        Some(BString::from(&bytes[start..*index]))
    } else if (first_class & IDENT_START) != 0 {
        while *index < bytes.len() && (CHAR_CLASSES[bytes[*index] as usize] & IDENT_CHAR) != 0 {
            *index += 1;
        }
        Some(BString::from(&bytes[start..*index]))
    } else {
        None
    }
}

/// Scans a `${...}` parameter expansion body starting immediately after `${`.
///
/// Tracks single quotes, double quotes, backslash escapes, backticks, and nested `$` expansions so
/// that `}` characters inside quoted strings or nested expansions do not prematurely close `${...}`.
/// Returns `Ok(inner)` when closed by `}`, or `Err(inner)` if EOF is reached before `}`.
pub fn scan_braced_param(
    bytes: &[u8],
    index: &mut usize,
    quote_mode: QuoteMode,
) -> Result<BString, BString> {
    let start = *index;
    let allow_single_quotes =
        quote_mode == QuoteMode::Unquoted || starts_with_pattern_modifier(&bytes[start..]);

    while *index < bytes.len() {
        let ch = bytes[*index];
        match ch {
            b'}' => {
                let content = BString::from(&bytes[start..*index]);
                *index += 1;
                return Ok(content);
            }
            b'\\' => {
                *index += 1;
                if *index < bytes.len() {
                    *index += 1;
                }
            }
            b'\'' if allow_single_quotes => {
                *index += 1;
                if !skip_single_quoted(bytes, index) {
                    break;
                }
            }
            b'"' => {
                *index += 1;
                if !skip_double_quoted(bytes, index) {
                    break;
                }
            }
            b'`' => {
                *index += 1;
                if !skip_backtick_quoted(bytes, index) {
                    break;
                }
            }
            b'$' => {
                *index += 1;
                if !skip_dollar_expansion(bytes, index, quote_mode) {
                    break;
                }
            }
            _ => {
                *index += 1;
            }
        }
    }
    Err(BString::from(&bytes[start..*index]))
}

/// Scans a `$(...)` command substitution body starting immediately after `$(`.
///
/// Tracks parenthesis depth, single/double quotes, backslash escapes, backticks, `#` comments,
/// nested `$` expansions, and `case ... in ... pattern) ... esac` blocks.
/// Returns `Ok(inner)` when closed by `)`, or `Err(inner)` if EOF is reached before `)`.
pub fn scan_command_substitution(bytes: &[u8], index: &mut usize) -> Result<BString, BString> {
    struct CaseState {
        paren_depth: usize,
        in_body: bool,
    }

    let start = *index;
    let mut paren_depth = 1usize;
    let mut case_stack: Vec<CaseState> = Vec::new();
    let mut at_token_start = true;
    let mut at_cmd_start = true;
    let mut in_prefix_assignment = false;

    while *index < bytes.len() {
        let ch = bytes[*index];
        match ch {
            b' ' | b'\t' | b'\r' => {
                *index += 1;
                at_token_start = true;
                in_prefix_assignment = false;
            }
            b'\n' | b'&' | b'|' => {
                *index += 1;
                at_token_start = true;
                at_cmd_start = true;
                in_prefix_assignment = false;
            }
            b';' => {
                *index += 1;
                if *index < bytes.len() && bytes[*index] == b';' {
                    *index += 1;
                    if let Some(top) = case_stack.last_mut()
                        && top.paren_depth == paren_depth
                    {
                        top.in_body = false;
                    }
                }
                at_token_start = true;
                at_cmd_start = true;
                in_prefix_assignment = false;
            }
            b'>' | b'<' => {
                *index += 1;
                at_token_start = true;
                in_prefix_assignment = false;
            }
            b'#' if at_token_start => {
                *index += 1;
                while *index < bytes.len() && bytes[*index] != b'\n' {
                    *index += 1;
                }
            }
            b'(' => {
                *index += 1;
                paren_depth += 1;
                at_token_start = true;
                at_cmd_start = true;
                in_prefix_assignment = false;
            }
            b')' => {
                if let Some(top) = case_stack.last_mut()
                    && top.paren_depth == paren_depth
                {
                    top.in_body = true;
                    *index += 1;
                    at_token_start = true;
                    at_cmd_start = true;
                    in_prefix_assignment = false;
                } else {
                    paren_depth -= 1;
                    if paren_depth == 0 {
                        let content = BString::from(&bytes[start..*index]);
                        *index += 1;
                        return Ok(content);
                    }
                    if let Some(top) = case_stack.last_mut()
                        && top.paren_depth == paren_depth
                        && !top.in_body
                    {
                        top.in_body = true;
                    }
                    *index += 1;
                    at_token_start = true;
                    at_cmd_start = true;
                    in_prefix_assignment = false;
                }
            }
            b'\\' => {
                *index += 1;
                if *index < bytes.len() {
                    *index += 1;
                }
                at_token_start = false;
                if !in_prefix_assignment {
                    at_cmd_start = false;
                }
            }
            b'\'' => {
                *index += 1;
                if !skip_single_quoted(bytes, index) {
                    break;
                }
                at_token_start = false;
                if !in_prefix_assignment {
                    at_cmd_start = false;
                }
            }
            b'"' => {
                *index += 1;
                if !skip_double_quoted(bytes, index) {
                    break;
                }
                at_token_start = false;
                if !in_prefix_assignment {
                    at_cmd_start = false;
                }
            }
            b'`' => {
                *index += 1;
                if !skip_backtick_quoted(bytes, index) {
                    break;
                }
                at_token_start = false;
                if !in_prefix_assignment {
                    at_cmd_start = false;
                }
            }
            b'$' => {
                *index += 1;
                if !skip_dollar_expansion(bytes, index, QuoteMode::Unquoted) {
                    break;
                }
                at_token_start = false;
                if !in_prefix_assignment {
                    at_cmd_start = false;
                }
            }
            _ => {
                let word_start = *index;
                while *index < bytes.len() {
                    let b = bytes[*index];
                    if (CHAR_CLASSES[b as usize] & (WHITESPACE | NEWLINE | META_CHAR | QUOTE_CHAR))
                        != 0
                        || b == b'$'
                    {
                        break;
                    }
                    *index += 1;
                }
                let word = &bytes[word_start..*index];
                let ended_at_delimiter = *index == bytes.len()
                    || (CHAR_CLASSES[bytes[*index] as usize] & (WHITESPACE | NEWLINE | META_CHAR))
                        != 0;
                let in_case_pattern = case_stack.last().is_some_and(|top| !top.in_body);
                if at_token_start && ended_at_delimiter {
                    if in_case_pattern {
                        if word == b"esac" && at_cmd_start {
                            case_stack.pop();
                            at_cmd_start = false;
                        } else if word == b"in" {
                            at_cmd_start = true;
                        } else {
                            at_cmd_start = false;
                        }
                    } else if at_cmd_start {
                        if word == b"case" {
                            case_stack.push(CaseState { paren_depth, in_body: false });
                            at_cmd_start = false;
                        } else if word == b"esac" {
                            if case_stack.last().is_some_and(|top| top.paren_depth == paren_depth) {
                                case_stack.pop();
                            }
                            at_cmd_start = false;
                        } else if matches!(
                            word,
                            b"{" | b"!"
                                | b"if"
                                | b"then"
                                | b"else"
                                | b"elif"
                                | b"while"
                                | b"until"
                                | b"do"
                        ) || is_assignment_word_bytes(word)
                        {
                            at_cmd_start = true;
                        } else {
                            at_cmd_start = false;
                        }
                    }
                } else if at_token_start && at_cmd_start && is_assignment_word_bytes(word) {
                    in_prefix_assignment = true;
                    at_cmd_start = true;
                } else if !in_prefix_assignment && !in_case_pattern {
                    at_cmd_start = false;
                }
                at_token_start = false;
            }
        }
    }
    Err(BString::from(&bytes[start..*index]))
}

/// Scans a `$((...))` arithmetic expansion body starting immediately after `$((`.
///
/// Tracks inner parenthesis depth, quotes, escapes, and nested expansions, closing on `))` when
/// `inner_paren_depth == 0`. Stray `)` characters at `inner_paren_depth == 0` do not underflow the
/// depth counter so the closing `))` is still recognized.
pub fn scan_arithmetic_expansion(bytes: &[u8], index: &mut usize) -> Result<BString, BString> {
    let start = *index;
    let mut inner_paren_depth = 0usize;

    while *index < bytes.len() {
        let ch = bytes[*index];
        match ch {
            b'(' => {
                inner_paren_depth += 1;
                *index += 1;
            }
            b')' => {
                if inner_paren_depth == 0 {
                    if *index + 1 < bytes.len() && bytes[*index + 1] == b')' {
                        let content = BString::from(&bytes[start..*index]);
                        *index += 2;
                        return Ok(content);
                    }
                    *index += 1;
                } else {
                    inner_paren_depth -= 1;
                    *index += 1;
                }
            }
            b'\\' => {
                *index += 1;
                if *index < bytes.len() {
                    *index += 1;
                }
            }
            b'\'' => {
                *index += 1;
                if !skip_single_quoted(bytes, index) {
                    break;
                }
            }
            b'"' => {
                *index += 1;
                if !skip_double_quoted(bytes, index) {
                    break;
                }
            }
            b'`' => {
                *index += 1;
                if !skip_backtick_quoted(bytes, index) {
                    break;
                }
            }
            b'$' => {
                *index += 1;
                if !skip_dollar_expansion(bytes, index, QuoteMode::Unquoted) {
                    break;
                }
            }
            _ => {
                *index += 1;
            }
        }
    }
    Err(BString::from(&bytes[start..*index]))
}

/// Scans a backtick `` `...` `` command substitution starting immediately after the opening `` ` ``.
///
/// Unescapes `\\`, `` \` ``, `\$`, `\n` (line continuation), and (when `quote_mode` is
/// `QuoteMode::DoubleQuoted`) `\"`. Returns `Ok(unescaped)` when closed by `` ` ``, or
/// `Err(unescaped)` if EOF is reached.
pub fn scan_backtick_command_substitution(
    bytes: &[u8],
    index: &mut usize,
    quote_mode: QuoteMode,
) -> Result<BString, BString> {
    let mut inner = Vec::new();
    while *index < bytes.len() {
        let ch = bytes[*index];
        *index += 1;
        if ch == b'`' {
            return Ok(inner.into());
        }
        if ch == b'\\' {
            if let Some(&next_ch) = bytes.get(*index) {
                if next_ch == b'`'
                    || next_ch == b'\\'
                    || next_ch == b'$'
                    || (quote_mode == QuoteMode::DoubleQuoted && next_ch == b'"')
                {
                    inner.push(next_ch);
                    *index += 1;
                } else if next_ch == b'\n' {
                    *index += 1;
                } else {
                    inner.push(b'\\');
                }
            } else {
                inner.push(b'\\');
            }
        } else {
            inner.push(ch);
        }
    }
    Err(inner.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartQuoting {
    Unquoted,
    Quoted,
}

struct Tokenizer<'a> {
    input: &'a [u8],
    pos: usize,
    tokens: Vec<Token>,
    parsing_heredoc_delimiter: bool,
    pending_heredoc_src_fd: Option<i32>,
    pending_heredoc_strip_tabs: bool,
    pending_indices: Vec<usize>,
}

impl<'a> Tokenizer<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            pos: 0,
            tokens: Vec::new(),
            parsing_heredoc_delimiter: false,
            pending_heredoc_src_fd: None,
            pending_heredoc_strip_tabs: false,
            pending_indices: Vec::new(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let ch = self.peek()?;
        self.pos += 1;
        Some(ch)
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn read_dollar_word_part(
        &mut self,
        quote_mode: QuoteMode,
        part_quoting: PartQuoting,
    ) -> Result<Option<RawWordPart>, ParseError> {
        self.next(); // consume '$'
        if self.consume(b'(') {
            if self.consume(b'(') {
                let expr = scan_arithmetic_expansion(self.input, &mut self.pos)
                    .map_err(|_| ParseError::Incomplete(IncompleteReason::Arithmetic))?;
                Ok(Some(match part_quoting {
                    PartQuoting::Quoted => RawWordPart::QuotedArithmetic(expr),
                    PartQuoting::Unquoted => RawWordPart::Arithmetic(expr),
                }))
            } else {
                let inner = scan_command_substitution(self.input, &mut self.pos)
                    .map_err(|_| ParseError::Incomplete(IncompleteReason::Paren))?;
                Ok(Some(match part_quoting {
                    PartQuoting::Quoted => RawWordPart::QuotedCommandSubstitution(inner),
                    PartQuoting::Unquoted => RawWordPart::CommandSubstitution(inner),
                }))
            }
        } else if self.consume(b'{') {
            let var_name = scan_braced_param(self.input, &mut self.pos, quote_mode)
                .map_err(|_| ParseError::Incomplete(IncompleteReason::Brace))?;
            Ok(Some(match part_quoting {
                PartQuoting::Quoted => RawWordPart::QuotedVar(var_name),
                PartQuoting::Unquoted => RawWordPart::Var(var_name),
            }))
        } else if let Some(var_name) = scan_unbraced_var_name(self.input, &mut self.pos) {
            Ok(Some(match part_quoting {
                PartQuoting::Quoted => RawWordPart::QuotedVar(var_name),
                PartQuoting::Unquoted => RawWordPart::Var(var_name),
            }))
        } else {
            Ok(None)
        }
    }

    fn process_heredocs(&mut self) -> Result<(), ParseError> {
        let indices: Vec<usize> = std::mem::take(&mut self.pending_indices);
        for idx in indices {
            let Token::RedirectHereDocPlaceholder { src_fd, delimiter, strip_tabs } =
                &self.tokens[idx]
            else {
                unreachable!();
            };
            let (src_fd, delimiter, strip_tabs) = (*src_fd, delimiter.clone(), *strip_tabs);

            let mut delimiter_string = Vec::new();
            let mut expand = true;
            for part in delimiter.iter() {
                match part {
                    RawWordPart::Literal(s) => {
                        delimiter_string.extend_from_slice(s.as_bytes());
                    }
                    RawWordPart::QuotedLiteral(s) => {
                        delimiter_string.extend_from_slice(s.as_bytes());
                        expand = false;
                    }
                    _ => unreachable!(),
                }
            }

            let mut body = Vec::new();
            let mut current_line = Vec::new();
            let mut found = false;
            while let Some(ch) = self.next() {
                if ch == b'\n' {
                    let check_line = current_line.clone();
                    if strip_tabs {
                        let trimmed = trim_start_tabs(&check_line);
                        if trimmed == delimiter_string {
                            found = true;
                            break;
                        }
                    } else if current_line == delimiter_string {
                        found = true;
                        break;
                    }

                    if strip_tabs {
                        let trimmed = trim_start_tabs(&current_line);
                        body.extend_from_slice(trimmed);
                    } else {
                        body.extend_from_slice(&current_line);
                    }
                    body.push(b'\n');
                    current_line.clear();
                } else {
                    current_line.push(ch);
                }
            }
            if !found {
                if strip_tabs {
                    let trimmed = trim_start_tabs(&current_line);
                    if trimmed == delimiter_string {
                        found = true;
                    }
                } else if current_line == delimiter_string {
                    found = true;
                }
                if !found {
                    if strip_tabs {
                        let trimmed = trim_start_tabs(&current_line);
                        body.extend_from_slice(trimmed);
                    } else {
                        body.extend_from_slice(&current_line);
                    }
                }
            }
            if !found {
                return Err(ParseError::Incomplete(IncompleteReason::Heredoc));
            }

            self.tokens[idx] = Token::RedirectHereDoc {
                src_fd,
                delimiter: delimiter.clone(),
                body: body.into(),
                expand,
            };
        }
        Ok(())
    }

    fn tokenize_redirect(&mut self, op: u8, src_fd: Option<i32>) {
        if op == b'>' {
            if self.consume(b'>') {
                self.tokens.push(Token::RedirectAppend(src_fd));
            } else if self.consume(b'&') {
                self.tokens.push(Token::RedirectDupOut(src_fd));
            } else if self.consume(b'|') {
                self.tokens.push(Token::RedirectOutClobber(src_fd));
            } else {
                self.tokens.push(Token::RedirectOut(src_fd));
            }
        } else if self.consume(b'<') {
            if self.consume(b'-') {
                self.pending_heredoc_strip_tabs = true;
            }
            self.parsing_heredoc_delimiter = true;
            self.pending_heredoc_src_fd = src_fd;
        } else if self.consume(b'>') {
            self.tokens.push(Token::RedirectReadWrite(src_fd));
        } else if self.consume(b'&') {
            self.tokens.push(Token::RedirectDupIn(src_fd));
        } else {
            self.tokens.push(Token::RedirectIn(src_fd));
        }
    }

    fn tokenize(mut self) -> Result<Vec<Token>, ParseError> {
        while let Some(c) = self.peek() {
            // Redirect with fd logic
            let mut lookahead_pos = self.pos;
            while let Some(&lc) = self.input.get(lookahead_pos) {
                if (CHAR_CLASSES[lc as usize] & DIGIT) != 0 {
                    lookahead_pos += 1;
                } else {
                    break;
                }
            }
            let digits = &self.input[self.pos..lookahead_pos];

            if !digits.is_empty()
                && matches!(self.input.get(lookahead_pos), Some(b'>' | b'<'))
                && let Some(src_fd) = parse_int::<i32>(digits)
            {
                self.pos = lookahead_pos;
                let next_c = self.next().unwrap();
                self.tokenize_redirect(next_c, Some(src_fd));
                continue;
            }

            match c {
                b'#' => {
                    self.next();
                    while let Some(nc) = self.peek() {
                        if nc == b'\n' {
                            break;
                        }
                        self.next();
                    }
                }
                b' ' | b'\t' | b'\r' => {
                    self.next();
                }
                b'\n' => {
                    self.next();
                    self.tokens.push(Token::Newline);
                    self.process_heredocs()?;
                }
                b';' => {
                    self.next();
                    if self.consume(b';') {
                        self.tokens.push(Token::DoubleSemi);
                    } else {
                        self.tokens.push(Token::Semi);
                    }
                }
                b'|' => {
                    self.next();
                    if self.consume(b'|') {
                        self.tokens.push(Token::Or);
                    } else {
                        self.tokens.push(Token::Pipe);
                    }
                }
                b'&' => {
                    self.next();
                    if self.consume(b'&') {
                        self.tokens.push(Token::And);
                    } else {
                        self.tokens.push(Token::Ampersand);
                    }
                }
                b'>' | b'<' => {
                    self.next();
                    self.tokenize_redirect(c, None);
                }
                b'(' => {
                    self.next();
                    self.tokens.push(Token::LParen);
                }
                b')' => {
                    self.next();
                    self.tokens.push(Token::RParen);
                }
                _ => {
                    let parts = self.read_word_parts(WordMode::NormalWord, QuoteMode::Unquoted)?;

                    if self.parsing_heredoc_delimiter {
                        self.pending_indices.push(self.tokens.len());
                        self.tokens.push(Token::RedirectHereDocPlaceholder {
                            src_fd: self.pending_heredoc_src_fd,
                            delimiter: parts,
                            strip_tabs: self.pending_heredoc_strip_tabs,
                        });
                        self.parsing_heredoc_delimiter = false;
                        self.pending_heredoc_src_fd = None;
                        self.pending_heredoc_strip_tabs = false;
                    } else {
                        self.tokens.push(Token::Word(parts));
                    }
                }
            }
        }
        self.process_heredocs()?;
        Ok(self.tokens)
    }

    fn read_word_parts(
        &mut self,
        word_mode: WordMode,
        quote_mode: QuoteMode,
    ) -> Result<Vec<RawWordPart>, ParseError> {
        let mut parts = Vec::new();
        let mut current_bytes = Vec::new();
        let mut state = TokenizeState::Unquoted;
        let mut double_quote_emitted_part = false;

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum TokenizeState {
            Unquoted,
            SingleQuoted,
            DoubleQuoted,
        }

        while let Some(ch) = self.peek() {
            match state {
                TokenizeState::Unquoted => match ch {
                    _ if word_mode == WordMode::NormalWord
                        && (CHAR_CLASSES[ch as usize] & (WHITESPACE | NEWLINE | META_CHAR))
                            != 0 =>
                    {
                        break;
                    }
                    b'\'' if quote_mode == QuoteMode::Unquoted => {
                        self.next();
                        if !current_bytes.is_empty() {
                            parts.push(RawWordPart::Literal(
                                std::mem::take(&mut current_bytes).into(),
                            ));
                        }
                        state = TokenizeState::SingleQuoted;
                    }
                    b'"' => {
                        self.next();
                        if !current_bytes.is_empty() {
                            parts.push(RawWordPart::Literal(
                                std::mem::take(&mut current_bytes).into(),
                            ));
                        }
                        double_quote_emitted_part = false;
                        state = TokenizeState::DoubleQuoted;
                    }
                    b'\\' => {
                        self.next();
                        if let Some(next_ch) = self.peek() {
                            if next_ch == b'\n' {
                                self.next();
                                if self.peek().is_none() {
                                    return Err(ParseError::Incomplete(
                                        IncompleteReason::LineContinuation,
                                    ));
                                }
                            } else {
                                let next_ch = self.next().unwrap();
                                if word_mode == WordMode::ModifierWord {
                                    if !current_bytes.is_empty() {
                                        parts.push(RawWordPart::Literal(
                                            std::mem::take(&mut current_bytes).into(),
                                        ));
                                    }
                                    parts.push(RawWordPart::QuotedLiteral(vec![next_ch].into()));
                                } else {
                                    current_bytes.push(next_ch);
                                }
                            }
                        } else {
                            current_bytes.push(b'\\');
                        }
                    }
                    b'`' => {
                        self.next();
                        let inner = scan_backtick_command_substitution(
                            self.input,
                            &mut self.pos,
                            quote_mode,
                        )
                        .map_err(|_| ParseError::Incomplete(IncompleteReason::Quote))?;
                        if !current_bytes.is_empty() {
                            parts.push(RawWordPart::Literal(
                                std::mem::take(&mut current_bytes).into(),
                            ));
                        }
                        parts.push(RawWordPart::CommandSubstitution(inner));
                    }
                    b'$' if !self.parsing_heredoc_delimiter => {
                        if let Some(part) =
                            self.read_dollar_word_part(quote_mode, PartQuoting::Unquoted)?
                        {
                            if !current_bytes.is_empty() {
                                parts.push(RawWordPart::Literal(
                                    std::mem::take(&mut current_bytes).into(),
                                ));
                            }
                            parts.push(part);
                        } else {
                            current_bytes.push(b'$');
                        }
                    }
                    _ => {
                        self.next();
                        current_bytes.push(ch);
                    }
                },
                TokenizeState::SingleQuoted => {
                    self.next();
                    if ch == b'\'' {
                        parts.push(RawWordPart::QuotedLiteral(
                            std::mem::take(&mut current_bytes).into(),
                        ));
                        state = TokenizeState::Unquoted;
                    } else {
                        current_bytes.push(ch);
                    }
                }
                TokenizeState::DoubleQuoted => match ch {
                    b'"' => {
                        self.next();
                        if !current_bytes.is_empty() || !double_quote_emitted_part {
                            parts.push(RawWordPart::QuotedLiteral(
                                std::mem::take(&mut current_bytes).into(),
                            ));
                        }
                        state = TokenizeState::Unquoted;
                    }
                    b'\\' => {
                        self.next();
                        if let Some(next_ch) = self.peek() {
                            if next_ch == b'\n' {
                                self.next();
                                if self.peek().is_none() {
                                    return Err(ParseError::Incomplete(
                                        IncompleteReason::LineContinuation,
                                    ));
                                }
                            } else if next_ch == b'"'
                                || next_ch == b'\\'
                                || next_ch == b'$'
                                || next_ch == b'`'
                            {
                                current_bytes.push(next_ch);
                                self.next();
                            } else {
                                current_bytes.push(b'\\');
                            }
                        } else {
                            current_bytes.push(b'\\');
                        }
                    }
                    b'`' => {
                        self.next();
                        let inner = scan_backtick_command_substitution(
                            self.input,
                            &mut self.pos,
                            QuoteMode::DoubleQuoted,
                        )
                        .map_err(|_| ParseError::Incomplete(IncompleteReason::Quote))?;
                        if !current_bytes.is_empty() {
                            parts.push(RawWordPart::QuotedLiteral(
                                std::mem::take(&mut current_bytes).into(),
                            ));
                        }
                        parts.push(RawWordPart::QuotedCommandSubstitution(inner));
                        double_quote_emitted_part = true;
                    }
                    b'$' if !self.parsing_heredoc_delimiter => {
                        if let Some(part) = self
                            .read_dollar_word_part(QuoteMode::DoubleQuoted, PartQuoting::Quoted)?
                        {
                            if !current_bytes.is_empty() {
                                parts.push(RawWordPart::QuotedLiteral(
                                    std::mem::take(&mut current_bytes).into(),
                                ));
                            }
                            parts.push(part);
                            double_quote_emitted_part = true;
                        } else {
                            current_bytes.push(b'$');
                        }
                    }
                    _ => {
                        self.next();
                        current_bytes.push(ch);
                    }
                },
            }
        }

        if state != TokenizeState::Unquoted {
            return Err(ParseError::Incomplete(IncompleteReason::Quote));
        }

        if !current_bytes.is_empty() {
            parts.push(RawWordPart::Literal(current_bytes.into()));
        }

        Ok(parts)
    }
}

/// Indicates whether a word or expansion is being parsed/expanded inside double quotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteMode {
    Unquoted,
    DoubleQuoted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordMode {
    NormalWord,
    ModifierWord,
}

pub fn tokenize(input: &[u8]) -> Result<Vec<Token>, ParseError> {
    Tokenizer::new(input).tokenize()
}

pub fn tokenize_modifier_word(
    input: &[u8],
    quote_mode: QuoteMode,
) -> Result<Vec<RawWordPart>, ParseError> {
    Tokenizer::new(input).read_word_parts(WordMode::ModifierWord, quote_mode)
}
