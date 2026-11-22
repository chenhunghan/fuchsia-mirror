// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Procedural macro for Zircon kernel syscall entry points.

use proc_macro::TokenStream;
use proc_macro2::{TokenStream as TokenStream2, TokenTree};
use quote::quote;
use syn::ext::IdentExt as _;
use syn::parse::{Nothing, Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    Abi, AttrStyle, Attribute, FnArg, Ident, ItemFn, Meta, Pat, Path, Safety, Signature, Token,
    Visibility, parse_quote,
};

fn is_cfg_path(path: &Path) -> bool {
    path.segments.last().is_some_and(|seg| seg.ident.unraw() == "cfg")
}

fn is_cfg_attr_path(path: &Path) -> bool {
    path.segments.last().is_some_and(|seg| seg.ident.unraw() == "cfg_attr")
}

fn validate_cfg_attr(meta: &Meta) -> syn::Result<()> {
    let list = meta.require_list()?;
    list.parse_args_with(|input: ParseStream<'_>| {
        // Skip the configuration predicate up to the first top-level comma.
        while !input.is_empty() && !input.peek(Token![,]) {
            input.parse::<TokenTree>()?;
        }
        if input.is_empty() {
            return Err(syn::Error::new_spanned(
                meta,
                "expected `,` in `#[cfg_attr(predicate, ...)]`",
            ));
        }
        input.parse::<Token![,]>()?;

        let nested = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        for nested_meta in &nested {
            if is_cfg_path(nested_meta.path()) {
                return Err(syn::Error::new_spanned(
                    nested_meta,
                    "#[syscall] functions cannot use `cfg` inside `#[cfg_attr(...)]`",
                ));
            }
            if is_cfg_attr_path(nested_meta.path()) {
                validate_cfg_attr(nested_meta)?;
            }
        }
        Ok(())
    })
}

/// Parsed and validated `sys_<name>` function annotated with [`syscall`].
struct Syscall {
    fn_item: ItemFn,
    base_name: Ident,
}

impl Parse for Syscall {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        // `ItemFn::parse` delegates to `Signature::parse`, which rejects `safe fn` with a generic
        // "expected `fn`" error instead of populating `Safety::Safe`. Check for an explicit `safe`
        // qualifier up front so callers get a clear diagnostic.
        let fork = input.fork();
        if fork.call(Attribute::parse_outer).is_ok()
            && fork.parse::<Visibility>().is_ok()
            && fork.parse::<Option<Token![const]>>().is_ok()
            && fork.parse::<Option<Token![async]>>().is_ok()
            && let Ok(Some(safe_token)) = fork.parse::<Option<Token![safe]>>()
            && fork.parse::<Option<Abi>>().is_ok()
            && fork.peek(Token![fn])
        {
            return Err(syn::Error::new_spanned(
                safe_token,
                "#[syscall] functions cannot have an explicit `safe` qualifier",
            ));
        }

        let fn_item: ItemFn = input.parse()?;
        fn_item.modifiers.require_empty()?;

        for attr in &fn_item.attrs {
            if matches!(attr.style, AttrStyle::Inner(_)) {
                return Err(syn::Error::new_spanned(
                    attr,
                    "#[syscall] functions cannot have inner attributes",
                ));
            }
            if is_cfg_attr_path(attr.path()) {
                validate_cfg_attr(&attr.meta)?;
            }
        }

        let Signature {
            constness,
            asyncness,
            safety,
            abi,
            fn_token: _,
            ident: fn_name,
            generics,
            paren_token: _,
            inputs,
            variadic,
            output: _,
        } = &fn_item.sig;

        if let Some(constness) = constness {
            return Err(syn::Error::new_spanned(
                constness,
                "#[syscall] functions cannot be `const`",
            ));
        }

        if let Some(asyncness) = asyncness {
            return Err(syn::Error::new_spanned(
                asyncness,
                "#[syscall] functions cannot be `async`",
            ));
        }

        match safety {
            Safety::Default => {}
            Safety::Unsafe(unsafe_token) => {
                return Err(syn::Error::new_spanned(
                    unsafe_token,
                    "#[syscall] functions cannot be `unsafe`",
                ));
            }
            Safety::Safe(_) => {
                unreachable!("`ItemFn::parse` never constructs `Safety::Safe`")
            }
        }

        if let Some(abi) = abi {
            return Err(syn::Error::new_spanned(
                abi,
                "#[syscall] functions cannot specify an explicit ABI",
            ));
        }

        if generics.lt_token.is_some() {
            return Err(syn::Error::new_spanned(
                generics,
                "#[syscall] functions cannot be generic",
            ));
        }

        if let Some(where_clause) = &generics.where_clause {
            return Err(syn::Error::new_spanned(
                where_clause,
                "#[syscall] functions cannot have a `where` clause",
            ));
        }

        if let Some(variadic) = variadic {
            return Err(syn::Error::new_spanned(
                variadic,
                "#[syscall] functions cannot be variadic",
            ));
        }

        let fn_name_string = fn_name.to_string();
        let Some(syscall_name) = fn_name_string.strip_prefix("sys_") else {
            return Err(syn::Error::new_spanned(
                fn_name,
                "#[syscall] function names must start with 'sys_' prefix",
            ));
        };

        let Ok(mut base_name) = syn::parse_str::<Ident>(syscall_name) else {
            return Err(syn::Error::new_spanned(
                fn_name,
                "#[syscall] function name after 'sys_' prefix must be a valid identifier",
            ));
        };
        base_name.set_span(fn_name.span());

        for arg in inputs {
            match arg {
                FnArg::Typed(pat_type) => match &*pat_type.pat {
                    Pat::Ident(_) => {}
                    _ => {
                        return Err(syn::Error::new_spanned(
                            &pat_type.pat,
                            "#[syscall] only supports identifier patterns for arguments",
                        ));
                    }
                },
                FnArg::Receiver(receiver) => {
                    return Err(syn::Error::new_spanned(
                        receiver,
                        "#[syscall] functions cannot take `self`",
                    ));
                }
            }
        }

        Ok(Syscall { fn_item, base_name })
    }
}

fn syscall_impl(attr: TokenStream2, item: TokenStream2) -> syn::Result<TokenStream2> {
    let _: Nothing = syn::parse2(attr)?;
    let Syscall { mut fn_item, base_name } = syn::parse2(item)?;

    let cfg_attrs: Vec<Attribute> =
        fn_item.attrs.iter().filter(|attr| is_cfg_path(attr.path())).cloned().collect();
    fn_item.attrs.push(parse_quote!(#[allow(improper_ctypes_definitions)]));
    fn_item.attrs.push(parse_quote!(#[unsafe(no_mangle)]));
    fn_item.sig.abi = Some(parse_quote!(extern "C"));
    let fn_name = &fn_item.sig.ident;

    Ok(quote! {
        #fn_item

        #(#cfg_attrs)*
        const _: crate::syscalls_rs::syscall_signatures::#base_name = #fn_name;
    })
}

/// Marks a Zircon kernel syscall implementation `sys_<name>` as an unmangled `extern "C"` entry
/// point and verifies its signature against `syscall_signatures::<name>`.
#[proc_macro_attribute]
pub fn syscall(attr: TokenStream, item: TokenStream) -> TokenStream {
    syscall_impl(attr.into(), item.into()).unwrap_or_else(syn::Error::into_compile_error).into()
}

#[cfg(test)]
mod tests {
    use super::{TokenStream2, syscall_impl};
    use quote::quote;
    use syn::{File, parse_quote};

    #[test]
    fn preserves_attributes_and_expands_valid_syscall() {
        let expanded = syscall_impl(
            quote!(),
            quote! {
                /// Syscall doc comment.
                #[cfg(target_arch = "x86_64")]
                #[cfg_attr(target_arch = "riscv64", allow(unused_mut))]
                #[cfg_attr(target_arch = "x86_64", cfg_attr(debug_assertions, inline))]
                #[cfg_attr(unix,)]
                #[cold]
                #[inline]
                #[allow(clippy::too_many_arguments)]
                pub fn sys_example_call(
                    mut options: u32,
                    #[allow(unused_variables)] extra: u64,
                    out: &mut u64,
                ) -> Result<(), Status> {
                    options += 1;
                    *out = u64::from(options);
                    Ok(())
                }
            },
        )
        .expect("valid #[syscall] item should expand");

        let actual: File = syn::parse2(expanded).expect("expanded tokens should parse as a file");
        let expected: File = parse_quote! {
            /// Syscall doc comment.
            #[cfg(target_arch = "x86_64")]
            #[cfg_attr(target_arch = "riscv64", allow(unused_mut))]
            #[cfg_attr(target_arch = "x86_64", cfg_attr(debug_assertions, inline))]
            #[cfg_attr(unix,)]
            #[cold]
            #[inline]
            #[allow(clippy::too_many_arguments)]
            #[allow(improper_ctypes_definitions)]
            #[unsafe(no_mangle)]
            pub extern "C" fn sys_example_call(
                mut options: u32,
                #[allow(unused_variables)] extra: u64,
                out: &mut u64,
            ) -> Result<(), Status> {
                options += 1;
                *out = u64::from(options);
                Ok(())
            }

            #[cfg(target_arch = "x86_64")]
            const _: crate::syscalls_rs::syscall_signatures::example_call = sys_example_call;
        };
        assert_eq!(actual, expected);
    }

    #[test]
    fn rejects_unexpected_attribute_arguments() {
        let err = syscall_impl(
            quote!(vdsocall),
            quote! {
                pub fn sys_foo() {}
            },
        )
        .expect_err("non-empty #[syscall(...)] arguments must be rejected");
        assert!(
            err.to_string().contains("unexpected token"),
            "expected `unexpected token`, got: {err}"
        );
    }

    #[test]
    fn rejects_inner_attributes_and_conditional_cfg_attr() {
        let cases = [
            (
                quote! {
                    pub fn sys_foo() {
                        #![cfg(target_arch = "x86_64")]
                    }
                },
                "#[syscall] functions cannot have inner attributes",
            ),
            (
                quote! {
                    pub fn sys_foo() {
                        #![allow(unused_variables)]
                    }
                },
                "#[syscall] functions cannot have inner attributes",
            ),
            (
                quote! {
                    #[cfg_attr(target_arch = "x86_64", cfg(foo))]
                    pub fn sys_foo() {}
                },
                "#[syscall] functions cannot use `cfg` inside `#[cfg_attr(...)]`",
            ),
            (
                quote! {
                    #[cfg_attr(target_arch = "x86_64", allow(unused_variables), cfg(foo))]
                    pub fn sys_foo() {}
                },
                "#[syscall] functions cannot use `cfg` inside `#[cfg_attr(...)]`",
            ),
            (
                quote! {
                    #[cfg_attr(a, cfg_attr(b, cfg(c)))]
                    pub fn sys_foo() {}
                },
                "#[syscall] functions cannot use `cfg` inside `#[cfg_attr(...)]`",
            ),
            (
                quote! {
                    #[cfg_attr(true, cfg(false))]
                    pub fn sys_foo() {}
                },
                "#[syscall] functions cannot use `cfg` inside `#[cfg_attr(...)]`",
            ),
            (
                quote! {
                    #[core::cfg_attr(a, core::cfg(b))]
                    pub fn sys_foo() {}
                },
                "#[syscall] functions cannot use `cfg` inside `#[cfg_attr(...)]`",
            ),
            (
                quote! {
                    #[cfg_attr(target_arch = "x86_64")]
                    pub fn sys_foo() {}
                },
                "expected `,` in `#[cfg_attr(predicate, ...)]`",
            ),
        ];

        for (item, expected_msg) in cases {
            let src = item.to_string();
            let err = syscall_impl(quote!(), item)
                .expect_err("unsupported attribute form must be rejected");
            assert!(
                err.to_string().contains(expected_msg),
                "expected `{expected_msg}` for `{src}`, got `{err}`",
            );
        }
    }

    #[test]
    fn rejects_invalid_signature_qualifiers_and_generics() {
        // Use string inputs so `rustfmt` cannot normalize `extern fn` or strip empty `<>`.
        let cases = [
            ("pub unsafe fn sys_foo() {}", "#[syscall] functions cannot be `unsafe`"),
            (
                "pub safe fn sys_foo() {}",
                "#[syscall] functions cannot have an explicit `safe` qualifier",
            ),
            (
                "pub safe extern \"C\" fn sys_foo() {}",
                "#[syscall] functions cannot have an explicit `safe` qualifier",
            ),
            ("pub const fn sys_foo() {}", "#[syscall] functions cannot be `const`"),
            ("pub async fn sys_foo() {}", "#[syscall] functions cannot be `async`"),
            (
                "pub extern \"C\" fn sys_foo() {}",
                "#[syscall] functions cannot specify an explicit ABI",
            ),
            ("pub extern fn sys_foo() {}", "#[syscall] functions cannot specify an explicit ABI"),
            ("pub fn sys_foo<T>() {}", "#[syscall] functions cannot be generic"),
            ("pub fn sys_foo<'a>(arg: &'a u32) {}", "#[syscall] functions cannot be generic"),
            ("pub fn sys_foo<const N: usize>() {}", "#[syscall] functions cannot be generic"),
            ("pub fn sys_foo<>() {}", "#[syscall] functions cannot be generic"),
            (
                "pub fn sys_foo() where (): Sized {}",
                "#[syscall] functions cannot have a `where` clause",
            ),
            ("pub fn sys_foo(arg: u32, ...) {}", "#[syscall] functions cannot be variadic"),
            ("pub fn sys_foo(...) {}", "#[syscall] functions cannot be variadic"),
        ];

        for (src, expected_msg) in cases {
            let item: TokenStream2 = src.parse().expect("valid token stream");
            let err = syscall_impl(quote!(), item)
                .expect_err("invalid #[syscall] signature must be rejected");
            assert!(
                err.to_string().contains(expected_msg),
                "expected `{expected_msg}` for `{src}`, got `{err}`",
            );
        }
    }

    #[test]
    fn rejects_invalid_names_and_parameter_forms() {
        let cases = [
            ("pub fn foo() {}", "#[syscall] function names must start with 'sys_' prefix"),
            (
                "pub fn sys_() {}",
                "#[syscall] function name after 'sys_' prefix must be a valid identifier",
            ),
            (
                "pub fn sys_0() {}",
                "#[syscall] function name after 'sys_' prefix must be a valid identifier",
            ),
            (
                "pub fn sys_type() {}",
                "#[syscall] function name after 'sys_' prefix must be a valid identifier",
            ),
            ("pub fn sys_foo(&self) {}", "#[syscall] functions cannot take `self`"),
            (
                "pub fn sys_foo((a, b): (u32, u32)) {}",
                "#[syscall] only supports identifier patterns for arguments",
            ),
            (
                "pub fn sys_foo(_: u32) {}",
                "#[syscall] only supports identifier patterns for arguments",
            ),
        ];

        for (src, expected_msg) in cases {
            let item: TokenStream2 = src.parse().expect("valid token stream");
            let err = syscall_impl(quote!(), item)
                .expect_err("invalid #[syscall] name or parameter must be rejected");
            assert!(
                err.to_string().contains(expected_msg),
                "expected `{expected_msg}` for `{src}`, got `{err}`",
            );
        }
    }
}
