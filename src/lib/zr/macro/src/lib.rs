// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Procedural macro support for `zr::assert!` and `zr::debug_assert!`.

use proc_macro::{Delimiter, Group, Ident, Literal, Punct, Spacing, Span, TokenStream, TokenTree};

/// Peels away synthetic `Delimiter::None` groups introduced by `macro_rules!` fragment captures.
fn unwrap_none_group(mut token: TokenTree) -> TokenTree {
    while let TokenTree::Group(group) = &token {
        if group.delimiter() == Delimiter::None {
            let mut iter = group.stream().into_iter();
            if let (Some(inner), None) = (iter.next(), iter.next()) {
                token = inner;
                continue;
            }
        }
        break;
    }
    token
}

/// Returns `true` if `lit` is a regular or raw string literal containing an unescaped `{` format
/// placeholder (such as `"{addr:#x}"`).
///
/// Plain string literals without format placeholders (including escaped `"{{"` and Unicode escapes
/// like `"\u{1F980}"`) return `false` so they can expand to `::core::assert!`, preserving
/// `const`-evaluation compatibility while already avoiding caller stack frame formatting overhead.
fn literal_has_format_placeholder(lit: &Literal) -> bool {
    let text = lit.to_string();
    let bytes = text.as_bytes();
    let (inner, is_raw) = if bytes.first() == Some(&b'"') {
        let Some(end) = text.rfind('"') else {
            return false;
        };
        if end == 0 {
            return false;
        }
        (&bytes[1..end], false)
    } else if bytes.first() == Some(&b'r') {
        let rest = &bytes[1..];
        let hashes = rest.iter().take_while(|&&b| b == b'#').count();
        if rest.get(hashes) != Some(&b'"') {
            return false;
        }
        let Some(end) = text.rfind('"') else {
            return false;
        };
        let start = 1 + hashes + 1;
        if end < start {
            return false;
        }
        (&bytes[start..end], true)
    } else {
        return false;
    };

    let mut index = 0;
    while index < inner.len() {
        match inner[index] {
            b'\\' if !is_raw => {
                index += 1;
                if index >= inner.len() {
                    break;
                }
                if inner[index] == b'u' && inner.get(index + 1) == Some(&b'{') {
                    index += 2;
                    while index < inner.len() && inner[index] != b'}' {
                        index += 1;
                    }
                }
                index += 1;
            }
            b'{' => {
                if inner.get(index + 1) == Some(&b'{') {
                    index += 2;
                } else {
                    return true;
                }
            }
            _ => {
                index += 1;
            }
        }
    }
    false
}

/// Dispatches 2-argument `zr::assert!(cond, literal)` calls between `::core::assert!` (when
/// `literal` has no format placeholders, keeping `const` compatibility) and
/// `$crate::__assert_outlined!` (when `literal` interpolates captured variables such as `"{x}"`).
///
/// Invoked internally as `__assert_literal!(($crate), ($cond), $msg)`.
#[proc_macro]
pub fn __assert_literal(input: TokenStream) -> TokenStream {
    let mut iter = input.into_iter();
    let Some(TokenTree::Group(crate_group)) = iter.next() else {
        panic!("expected crate path group");
    };
    let _comma1 = iter.next();
    let Some(TokenTree::Group(cond_group)) = iter.next() else {
        panic!("expected condition group");
    };
    let _comma2 = iter.next();
    let Some(msg_raw) = iter.next() else {
        panic!("expected message literal");
    };

    let msg_tt = unwrap_none_group(msg_raw);
    let has_placeholder = match &msg_tt {
        TokenTree::Literal(lit) => literal_has_format_placeholder(lit),
        _ => false,
    };

    let span = Span::call_site();
    let mut args = cond_group.stream();
    args.extend([TokenTree::Punct(Punct::new(',', Spacing::Alone)), msg_tt]);

    if !has_placeholder {
        TokenStream::from_iter([
            TokenTree::Punct(Punct::new(':', Spacing::Joint)),
            TokenTree::Punct(Punct::new(':', Spacing::Alone)),
            TokenTree::Ident(Ident::new("core", span)),
            TokenTree::Punct(Punct::new(':', Spacing::Joint)),
            TokenTree::Punct(Punct::new(':', Spacing::Alone)),
            TokenTree::Ident(Ident::new("assert", span)),
            TokenTree::Punct(Punct::new('!', Spacing::Alone)),
            TokenTree::Group(Group::new(Delimiter::Parenthesis, args)),
        ])
    } else {
        let mut out = crate_group.stream();
        out.extend([
            TokenTree::Punct(Punct::new(':', Spacing::Joint)),
            TokenTree::Punct(Punct::new(':', Spacing::Alone)),
            TokenTree::Ident(Ident::new("__assert_outlined", span)),
            TokenTree::Punct(Punct::new('!', Spacing::Alone)),
            TokenTree::Group(Group::new(Delimiter::Parenthesis, args)),
        ]);
        out
    }
}
