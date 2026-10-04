// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{Expr, LitByteStr, LitStr, Token};

enum Family {
    SignedInt(char),
    UnsignedInt(char),
    String,
    CString,
    Pointer,
    Char,
    Bool,
    Float(char),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SpecAlign {
    Default,
    Left,
    Right,
}

struct ParsedSpec {
    align: SpecAlign,
    sign_plus: bool,
    space_sign: bool,
    alternate: bool,
    zero_pad: bool,
    width: Option<usize>,
    precision: Option<usize>,
    explicit_type: Option<String>,
}

fn parse_spec(spec: &str, fmt_lit: &LitStr) -> syn::Result<ParsedSpec> {
    let chars: Vec<char> = spec.chars().collect();
    let mut i = 0;

    let mut align = SpecAlign::Default;
    let mut sign_plus = false;
    let mut space_sign = false;
    let mut alternate = false;
    let mut zero_pad = false;

    if chars.len() >= 2 && matches!(chars[1], '<' | '^' | '>') {
        let fill = chars[0];
        if chars[1] == '^' {
            return Err(syn::Error::new_spanned(
                fmt_lit,
                "center alignment ('^') is not supported by kprint",
            ));
        }
        if fill == '0' && chars[1] == '>' {
            zero_pad = true;
            align = SpecAlign::Right;
            i = 2;
        } else if fill != ' ' && !matches!(fill, '<' | '>' | '+' | '-' | '#' | '0') {
            return Err(syn::Error::new_spanned(
                fmt_lit,
                "custom fill characters are not supported by kprint",
            ));
        } else if fill == ' ' {
            align = if chars[1] == '<' { SpecAlign::Left } else { SpecAlign::Right };
            i = 2;
        }
    }

    while i < chars.len() {
        match chars[i] {
            '<' => {
                align = SpecAlign::Left;
                i += 1;
            }
            '>' => {
                align = SpecAlign::Right;
                i += 1;
            }
            '^' => {
                return Err(syn::Error::new_spanned(
                    fmt_lit,
                    "center alignment ('^') is not supported by kprint",
                ));
            }
            '+' => {
                sign_plus = true;
                i += 1;
            }
            '-' => {
                // In Rust format strings, '-' is the sign flag (default behavior), not left-alignment.
                i += 1;
            }
            ' ' => {
                space_sign = true;
                i += 1;
            }
            '#' => {
                alternate = true;
                i += 1;
            }
            '0' => {
                zero_pad = true;
                i += 1;
            }
            _ => break,
        }
    }

    let mut width_str = String::new();
    while i < chars.len() && chars[i].is_ascii_digit() {
        width_str.push(chars[i]);
        i += 1;
    }
    let width = if width_str.is_empty() { None } else { width_str.parse::<usize>().ok() };

    let precision = if i < chars.len() && chars[i] == '.' {
        i += 1;
        let mut prec_str = String::new();
        while i < chars.len() && chars[i].is_ascii_digit() {
            prec_str.push(chars[i]);
            i += 1;
        }
        if prec_str.is_empty() { Some(0) } else { prec_str.parse::<usize>().ok() }
    } else {
        None
    };

    let explicit_type = if i < chars.len() {
        let remaining: String = chars[i..].iter().collect();
        if remaining == "cs" || remaining == "z" {
            Some("cs".to_string())
        } else if let Some(&c) = chars.get(i) {
            if matches!(
                c,
                'x' | 'X' | 'o' | 'p' | 'b' | 'e' | 'E' | 'f' | 'd' | 'i' | 'u' | 's' | 'c'
            ) {
                Some(c.to_string())
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    Ok(ParsedSpec {
        align,
        sign_plus,
        space_sign,
        alternate,
        zero_pad,
        width,
        precision,
        explicit_type,
    })
}

fn infer_family(arg: &Expr) -> Family {
    match arg {
        Expr::Lit(lit) => match &lit.lit {
            syn::Lit::Str(_) | syn::Lit::ByteStr(_) => Family::String,
            syn::Lit::Char(_) => Family::Char,
            syn::Lit::Byte(_) => Family::UnsignedInt('u'),
            syn::Lit::Bool(_) => Family::Bool,
            syn::Lit::Float(_) => Family::Float('f'),
            syn::Lit::Int(int_lit) => {
                let suffix = int_lit.suffix();
                if suffix.starts_with('u') || suffix == "usize" {
                    Family::UnsignedInt('u')
                } else {
                    Family::SignedInt('d')
                }
            }
            _ => Family::SignedInt('d'),
        },
        Expr::Unary(u) if matches!(u.op, syn::UnOp::Neg(_)) => match &*u.expr {
            Expr::Lit(lit) => match &lit.lit {
                syn::Lit::Float(_) => Family::Float('f'),
                _ => Family::SignedInt('d'),
            },
            _ => infer_family(&u.expr),
        },
        Expr::Reference(r) => infer_family(&r.expr),
        Expr::Group(g) => infer_family(&g.expr),
        Expr::Paren(p) => infer_family(&p.expr),
        Expr::Cast(cast) => {
            let ty_str = quote!(#cast.ty).to_string();
            if ty_str.contains("str") || ty_str.contains("CStr") || ty_str.contains("[u8]") {
                Family::String
            } else if ty_str.contains("* const c_char")
                || ty_str.contains("* mut c_char")
                || ty_str.contains("* const i8")
                || ty_str.contains("* mut i8")
            {
                Family::CString
            } else if ty_str.contains("char") {
                Family::Char
            } else if ty_str.contains("bool") {
                Family::Bool
            } else if ty_str.contains('*') || ty_str.contains("ptr") || ty_str.contains("c_void") {
                Family::Pointer
            } else if ty_str.contains("u8")
                || ty_str.contains("u16")
                || ty_str.contains("u32")
                || ty_str.contains("u64")
                || ty_str.contains("u128")
                || ty_str.contains("usize")
            {
                Family::UnsignedInt('u')
            } else if ty_str.contains("f32") || ty_str.contains("f64") {
                Family::Float('f')
            } else {
                Family::SignedInt('d')
            }
        }
        _ => Family::SignedInt('d'),
    }
}

fn translate_unsigned_or_ptr(
    kprint_crate: &syn::Path,
    mut parsed: ParsedSpec,
    c: char,
    val_expr: proc_macro2::TokenStream,
    is_pointer: bool,
) -> (String, Vec<proc_macro2::TokenStream>) {
    if is_pointer {
        if parsed.alternate {
            parsed.zero_pad = true;
            if parsed.width.is_none() {
                parsed.width = Some(18);
            }
        }
        parsed.alternate = true;
    }

    if parsed.alternate && (c == 'x' || c == 'X') {
        let w = parsed.width.unwrap_or(0);
        if w > 2 && !parsed.zero_pad && parsed.align != SpecAlign::Left {
            (
                format!("%*s%ll{}", c),
                vec![
                    quote! {
                        #kprint_crate::backend::hex_alt_prefix_width(#val_expr, #w)
                    },
                    quote! { (c"0x").as_ptr() },
                    val_expr,
                ],
            )
        } else if parsed.zero_pad {
            if w > 2 {
                (format!("0x%0{}ll{}", w - 2, c), vec![val_expr])
            } else {
                (format!("0x%ll{}", c), vec![val_expr])
            }
        } else if parsed.align == SpecAlign::Left && w > 2 {
            (format!("0x%-{}ll{}", w - 2, c), vec![val_expr])
        } else {
            (format!("0x%ll{}", c), vec![val_expr])
        }
    } else {
        let mut c_flags = String::new();
        if parsed.alternate && c == 'o' {
            c_flags.push('#');
        }
        if parsed.zero_pad {
            c_flags.push('0');
        } else if parsed.align == SpecAlign::Left {
            c_flags.push('-');
        }
        let width_str = parsed.width.map(|w| w.to_string()).unwrap_or_default();
        (format!("%{}{}ll{}", c_flags, width_str, c), vec![val_expr])
    }
}

fn translate_arg(
    kprint_crate: &syn::Path,
    spec: &str,
    fmt_lit: &LitStr,
    arg: &proc_macro2::TokenStream,
    expr_for_inference: &Expr,
    extra_bindings: &mut Vec<proc_macro2::TokenStream>,
    char_counter: &mut usize,
) -> syn::Result<(String, Vec<proc_macro2::TokenStream>)> {
    let parsed = parse_spec(spec, fmt_lit)?;
    let family = match parsed.explicit_type.as_deref() {
        Some("d" | "i") => Family::SignedInt('d'),
        Some("u") => Family::UnsignedInt('u'),
        Some("o") => Family::UnsignedInt('o'),
        Some("x") => Family::UnsignedInt('x'),
        Some("X") => Family::UnsignedInt('X'),
        Some("s") => Family::String,
        Some("cs" | "z") => Family::CString,
        Some("p") => Family::Pointer,
        Some("c") => Family::Char,
        Some("b") => Family::Bool,
        Some("f") => Family::Float('f'),
        Some("e") => Family::Float('e'),
        Some("E") => Family::Float('E'),
        _ => infer_family(expr_for_inference),
    };

    let res = match family {
        Family::SignedInt(c) => {
            let mut c_flags = String::new();
            if parsed.sign_plus {
                c_flags.push('+');
            } else if parsed.space_sign {
                c_flags.push(' ');
            }
            if parsed.zero_pad {
                c_flags.push('0');
            } else if parsed.align == SpecAlign::Left {
                c_flags.push('-');
            }
            let width_str = parsed.width.map(|w| w.to_string()).unwrap_or_default();
            (
                format!("%{}{}ll{}", c_flags, width_str, c),
                vec![quote! { #kprint_crate::backend::AsKPrintSignedInt::as_c_longlong(&#arg) }],
            )
        }
        Family::UnsignedInt(c) => {
            let val_expr =
                quote! { #kprint_crate::backend::AsKPrintUnsignedInt::as_c_ulonglong(&#arg) };
            translate_unsigned_or_ptr(kprint_crate, parsed, c, val_expr, false)
        }
        Family::Pointer => {
            let val_expr = quote! {
                (#kprint_crate::backend::AsKPrintPointer::as_c_ptr_void(&#arg) as usize)
                    as core::ffi::c_ulonglong
            };
            translate_unsigned_or_ptr(kprint_crate, parsed, 'x', val_expr, true)
        }
        Family::String => {
            let prec_clamp = if let Some(limit) = parsed.precision {
                let limit_lit = limit as i32;
                quote! { core::cmp::min(#limit_lit, #kprint_crate::backend::AsKPrintStr::kprint_len(&#arg)) }
            } else {
                quote! { #kprint_crate::backend::AsKPrintStr::kprint_len(&#arg) }
            };
            let w = parsed.width.unwrap_or(0);
            let c_flags = if w > 0 && parsed.align != SpecAlign::Right { "-" } else { "" };
            let width_str = parsed.width.map(|w| w.to_string()).unwrap_or_default();
            (
                format!("%{}{}.*s", c_flags, width_str),
                vec![prec_clamp, quote! { #kprint_crate::backend::AsKPrintStr::kprint_ptr(&#arg) }],
            )
        }
        Family::CString => {
            let w = parsed.width.unwrap_or(0);
            let c_flags = if w > 0 && parsed.align != SpecAlign::Right { "-" } else { "" };
            let width_str = parsed.width.map(|w| w.to_string()).unwrap_or_default();
            let prec_str = parsed.precision.map(|p| format!(".{}", p)).unwrap_or_default();
            (
                format!("%{}{}{}s", c_flags, width_str, prec_str),
                vec![quote! { #kprint_crate::backend::AsKPrintCString::as_c_ptr(&#arg) }],
            )
        }
        Family::Char => {
            let char_buf_ident = quote::format_ident!("__kprint_char_buf_{}", *char_counter);
            let char_res_ident = quote::format_ident!("__kprint_char_res_{}", *char_counter);
            *char_counter += 1;
            extra_bindings.push(quote! {
                let mut #char_buf_ident = [0u8; 4];
                let #char_res_ident = #kprint_crate::backend::AsKPrintChar::kprint_encode_char(
                    &#arg,
                    &mut #char_buf_ident,
                );
            });
            let len_expr = if let Some(limit) = parsed.precision {
                let limit_lit = limit as i32;
                quote! { core::cmp::min(#limit_lit, #char_res_ident.0) }
            } else {
                quote! { #char_res_ident.0 }
            };
            let w = parsed.width.unwrap_or(0);
            let c_flags = if w > 0 && parsed.align != SpecAlign::Right { "-" } else { "" };
            let width_str = parsed.width.map(|w| w.to_string()).unwrap_or_default();
            (format!("%{}{}.*s", c_flags, width_str), vec![len_expr, quote! { #char_res_ident.1 }])
        }
        Family::Bool => {
            let raw_len = quote! { (if #arg { 4 } else { 5 }) as core::ffi::c_int };
            let len_expr = if let Some(limit) = parsed.precision {
                let limit_lit = limit as i32;
                quote! { core::cmp::min(#limit_lit, #raw_len) }
            } else {
                raw_len
            };
            let w = parsed.width.unwrap_or(0);
            let c_flags = if w > 0 && parsed.align != SpecAlign::Right { "-" } else { "" };
            let width_str = parsed.width.map(|w| w.to_string()).unwrap_or_default();
            (
                format!("%{}{}.*s", c_flags, width_str),
                vec![
                    len_expr,
                    quote! { (if #arg { &b"true\0"[..] } else { &b"false\0"[..] }).as_ptr() as *const core::ffi::c_char },
                ],
            )
        }
        Family::Float(c) => {
            let mut c_flags = String::new();
            if parsed.sign_plus {
                c_flags.push('+');
            } else if parsed.space_sign {
                c_flags.push(' ');
            }
            if parsed.alternate {
                c_flags.push('#');
            }
            if parsed.zero_pad {
                c_flags.push('0');
            } else if parsed.align == SpecAlign::Left {
                c_flags.push('-');
            }
            let width_str = parsed.width.map(|w| w.to_string()).unwrap_or_default();
            let prec_str = parsed.precision.map(|p| format!(".{}", p)).unwrap_or_default();
            (
                format!("%{}{}{}{}", c_flags, width_str, prec_str, c),
                vec![quote! { (#arg) as core::ffi::c_double }],
            )
        }
    };

    Ok(res)
}

enum ArgItem {
    Positional(Expr),
    Named(syn::Ident, Expr),
}

impl Parse for ArgItem {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if input.peek(syn::Ident) && input.peek2(Token![=]) {
            let name: syn::Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            let expr: Expr = input.parse()?;
            Ok(ArgItem::Named(name, expr))
        } else {
            let expr: Expr = input.parse()?;
            Ok(ArgItem::Positional(expr))
        }
    }
}

fn parse_fmt_str(input: ParseStream<'_>) -> syn::Result<LitStr> {
    if input.peek(LitStr) {
        return input.parse::<LitStr>();
    }
    if input.peek(syn::Ident) {
        let ident: syn::Ident = input.parse()?;
        if ident == "concat" {
            input.parse::<Token![!]>()?;
            let content;
            syn::parenthesized!(content in input);
            let mut combined = String::new();
            while !content.is_empty() {
                if content.peek(LitStr) {
                    let s: LitStr = content.parse()?;
                    combined.push_str(&s.value());
                } else if content.peek(syn::LitInt) {
                    let i: syn::LitInt = content.parse()?;
                    combined.push_str(&i.to_string());
                } else if content.peek(syn::LitBool) {
                    let b: syn::LitBool = content.parse()?;
                    combined.push_str(if b.value { "true" } else { "false" });
                } else if content.peek(syn::LitChar) {
                    let c: syn::LitChar = content.parse()?;
                    combined.push(c.value());
                } else {
                    let expr: Expr = content.parse()?;
                    return Err(syn::Error::new_spanned(
                        expr,
                        "concat! in kprint format string only supports literal tokens",
                    ));
                }
                if !content.is_empty() {
                    content.parse::<Token![,]>()?;
                }
            }
            return Ok(LitStr::new(&combined, ident.span()));
        }
    }
    Err(input.error("expected string literal or concat!(...) format string"))
}

struct ParsedCall {
    kprint_crate: syn::Path,
    buf: Option<Expr>,
    fmt: LitStr,
    args: Vec<ArgItem>,
}

fn parse_internal_kprint(input: ParseStream<'_>) -> syn::Result<ParsedCall> {
    let kprint_crate: syn::Path = input.parse()?;
    input.parse::<Token![,]>()?;
    let fmt = parse_fmt_str(input)?;
    let mut args = Vec::new();
    while !input.is_empty() {
        input.parse::<Token![,]>()?;
        if input.is_empty() {
            break;
        }
        args.push(input.parse::<ArgItem>()?);
    }
    Ok(ParsedCall { kprint_crate, buf: None, fmt, args })
}

fn parse_internal_kformat(input: ParseStream<'_>) -> syn::Result<ParsedCall> {
    let kprint_crate: syn::Path = input.parse()?;
    input.parse::<Token![,]>()?;
    let buf: Expr = input.parse()?;
    input.parse::<Token![,]>()?;
    let fmt = parse_fmt_str(input)?;
    let mut args = Vec::new();
    while !input.is_empty() {
        input.parse::<Token![,]>()?;
        if input.is_empty() {
            break;
        }
        args.push(input.parse::<ArgItem>()?);
    }
    Ok(ParsedCall { kprint_crate, buf: Some(buf), fmt, args })
}

struct GeneratedOutput {
    fmt_bytes: Vec<u8>,
    bindings: Vec<proc_macro2::TokenStream>,
    emitted_args: Vec<proc_macro2::TokenStream>,
}

fn build_output(
    kprint_crate: &syn::Path,
    prefix: &str,
    suffix: &str,
    fmt_lit: &LitStr,
    args: &[ArgItem],
) -> syn::Result<GeneratedOutput> {
    let mut bindings = Vec::new();
    let mut positional_vars = Vec::new();
    let mut named_vars = std::collections::HashMap::new();
    let mut named_order = Vec::new();

    // Bind all supplied arguments once in lexical order to guarantee single evaluation.
    for (pos_idx, item) in args.iter().enumerate() {
        match item {
            ArgItem::Positional(e) => {
                let var_name = quote::format_ident!("__kprint_pos_{}", pos_idx);
                bindings.push(quote! {
                    let #var_name = #e;
                });
                positional_vars.push((var_name, e));
            }
            ArgItem::Named(ident, e) => {
                let var_name = quote::format_ident!("__kprint_named_{}", ident);
                bindings.push(quote! {
                    let #var_name = #e;
                });
                named_order.push((ident.to_string(), e));
                named_vars.insert(ident.to_string(), (var_name, e));
            }
        }
    }

    let mut positional_used = vec![false; positional_vars.len()];
    let mut named_used = std::collections::HashSet::new();

    let mut emitted_args = Vec::new();
    let mut c_fmt = String::from(prefix);
    let mut next_auto_pos = 0;
    let chars: Vec<char> = fmt_lit.value().chars().collect();
    let mut i = 0;
    let mut char_counter = 0usize;

    while i < chars.len() {
        if chars[i] == '{' {
            if i + 1 < chars.len() && chars[i + 1] == '{' {
                c_fmt.push('{');
                i += 2;
                continue;
            }
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '}' {
                j += 1;
            }
            if j >= chars.len() {
                return Err(syn::Error::new_spanned(fmt_lit, "Unclosed '{' in format string"));
            }
            let placeholder_content: String = chars[i + 1..j].iter().collect();
            i = j + 1;

            let (target, spec_part) = if let Some(colon_idx) = placeholder_content.find(':') {
                (&placeholder_content[..colon_idx], &placeholder_content[colon_idx + 1..])
            } else {
                (placeholder_content.as_str(), "")
            };

            let (var_token, expr_for_inference) = if target.is_empty() {
                if next_auto_pos >= positional_vars.len() {
                    return Err(syn::Error::new_spanned(
                        fmt_lit,
                        "Not enough positional arguments for format string",
                    ));
                }
                let pos = next_auto_pos;
                next_auto_pos += 1;
                positional_used[pos] = true;
                let (ref var_name, orig_expr) = positional_vars[pos];
                (quote! { #var_name }, (*orig_expr).clone())
            } else if let Ok(idx) = target.parse::<usize>() {
                if idx >= positional_vars.len() {
                    return Err(syn::Error::new_spanned(
                        fmt_lit,
                        format!("Positional index {{{}}} is out of range", idx),
                    ));
                }
                positional_used[idx] = true;
                let (ref var_name, orig_expr) = positional_vars[idx];
                (quote! { #var_name }, (*orig_expr).clone())
            } else if let Some((var_name, orig_expr)) = named_vars.get(target) {
                named_used.insert(target.to_string());
                (quote! { #var_name }, (*orig_expr).clone())
            } else {
                let ident = syn::Ident::new(target, fmt_lit.span());
                let expr_path = syn::Expr::Path(syn::ExprPath {
                    attrs: Vec::new(),
                    qself: None,
                    path: syn::Path::from(ident.clone()),
                });
                (quote! { #ident }, expr_path)
            };

            let (c_spec, emitted) = translate_arg(
                kprint_crate,
                spec_part,
                fmt_lit,
                &var_token,
                &expr_for_inference,
                &mut bindings,
                &mut char_counter,
            )?;
            c_fmt.push_str(&c_spec);
            emitted_args.extend(emitted);
        } else if chars[i] == '}' {
            if i + 1 < chars.len() && chars[i + 1] == '}' {
                c_fmt.push('}');
                i += 2;
            } else {
                return Err(syn::Error::new_spanned(fmt_lit, "Unmatched '}' in format string"));
            }
        } else if chars[i] == '%' {
            c_fmt.push_str("%%");
            i += 1;
        } else {
            c_fmt.push(chars[i]);
            i += 1;
        }
    }

    for (idx, used) in positional_used.iter().enumerate() {
        if !*used {
            let (_, orig_expr) = &positional_vars[idx];
            return Err(syn::Error::new_spanned(orig_expr, "argument never used in format string"));
        }
    }

    for (name, orig_expr) in &named_order {
        if !named_used.contains(name) {
            return Err(syn::Error::new_spanned(
                orig_expr,
                "named argument never used in format string",
            ));
        }
    }

    c_fmt.push_str(suffix);
    c_fmt.push('\0');

    Ok(GeneratedOutput { fmt_bytes: c_fmt.into_bytes(), bindings, emitted_args })
}

fn emit_print_parsed(prefix: &str, suffix: &str, parsed: ParsedCall) -> TokenStream {
    let GeneratedOutput { fmt_bytes, bindings, emitted_args } =
        match build_output(&parsed.kprint_crate, prefix, suffix, &parsed.fmt, &parsed.args) {
            Ok(res) => res,
            Err(e) => return e.to_compile_error().into(),
        };

    let kprint_crate = parsed.kprint_crate;
    let fmt_lit = LitByteStr::new(&fmt_bytes, Span::call_site());

    quote! {
        {
            #(#bindings)*
            const __FMT: &[u8] = #fmt_lit;
            unsafe {
                let _ = #kprint_crate::backend::printf(
                    __FMT.as_ptr() as *const core::ffi::c_char,
                    #(#emitted_args),*
                );
            }
        }
    }
    .into()
}

fn emit_format_parsed(parsed: ParsedCall) -> TokenStream {
    let GeneratedOutput { fmt_bytes, bindings, emitted_args } =
        match build_output(&parsed.kprint_crate, "", "", &parsed.fmt, &parsed.args) {
            Ok(res) => res,
            Err(e) => return e.to_compile_error().into(),
        };

    let kprint_crate = parsed.kprint_crate;
    let fmt_lit = LitByteStr::new(&fmt_bytes, Span::call_site());
    let buf_expr = parsed.buf.expect("buffer expression required for kformat");

    quote! {
        {
            #(#bindings)*
            const __FMT: &[u8] = #fmt_lit;
            let __len = unsafe {
                #kprint_crate::backend::snprintf(
                    (#buf_expr).as_mut_ptr() as *mut core::ffi::c_char,
                    (#buf_expr).len(),
                    __FMT.as_ptr() as *const core::ffi::c_char,
                    #(#emitted_args),*
                )
            };
            let __valid = if __len < 0 {
                0
            } else {
                (__len as usize).min((#buf_expr).len().saturating_sub(1))
            };
            &(#buf_expr)[..__valid]
        }
    }
    .into()
}

#[proc_macro]
pub fn __kprint_internal(tokens: TokenStream) -> TokenStream {
    let parsed = syn::parse_macro_input!(tokens with parse_internal_kprint);
    emit_print_parsed("", "", parsed)
}

#[proc_macro]
pub fn __kprintln_internal(tokens: TokenStream) -> TokenStream {
    let parsed = syn::parse_macro_input!(tokens with parse_internal_kprint);
    emit_print_parsed("", "\n", parsed)
}

#[proc_macro]
pub fn __kformat_internal(tokens: TokenStream) -> TokenStream {
    let parsed = syn::parse_macro_input!(tokens with parse_internal_kformat);
    emit_format_parsed(parsed)
}
