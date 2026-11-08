// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![deny(
    dead_code,
    unused_parens,
    unused_results,
    unsafe_op_in_unsafe_fn,
    missing_unsafe_on_extern,
    unsafe_attr_outside_unsafe,
    non_local_definitions,
    mismatched_lifetime_syntaxes,
    clippy::all,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::new_without_default
)]

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Fields, Ident, ItemStruct, Type, TypePath};

struct MutexField {
    ident: Ident,
    class_ident: Ident,
    class_type: proc_macro2::TokenStream,
    mutex_type: proc_macro2::TokenStream,
    custom_class: Option<Type>,
    custom_flags: Option<syn::Expr>,
    is_phantom: bool,
    is_untracked: bool,
}

struct GuardedField {
    ident: Ident,
    mutex_idents: Vec<Ident>,
    ty: Type,
    vis: syn::Visibility,
    project_as_pin: bool,
    is_konce_cell: bool,
    konce_cell_inner_ty: Option<Type>,
}

struct FieldAttrAnalysis {
    is_mutex: bool,
    is_brwlock: bool,
    guarded_by: Vec<Ident>,
    is_pinned: bool,
    is_unpinned: bool,
    custom_class: Option<Type>,
    custom_flags: Option<syn::Expr>,
    is_untracked: bool,
}

fn parse_mutex_or_brwlock_args(
    meta_list: &syn::MetaList,
    custom_class: &mut Option<Type>,
    custom_flags: &mut Option<syn::Expr>,
    is_untracked: &mut bool,
    errors: &mut Vec<syn::Error>,
    attr_name: &str,
) {
    // `untracked` is only meaningful for `#[mutex]`: `BrwLockPi` has no untracked storage type
    // and the brwlock codegen always registers a lock class.
    let supports_untracked = attr_name == "mutex";
    let accepted_args = if supports_untracked {
        "`LockClass`, `untracked`, or `flags = <expr>`"
    } else {
        "`LockClass` or `flags = <expr>`"
    };
    let mut untracked_span = None;

    if let Err(e) = meta_list.parse_args_with(|input: syn::parse::ParseStream<'_>| {
        if input.is_empty() {
            return Err(syn::Error::new(
                meta_list.span(),
                format!("#[{attr_name}] attribute accepts {accepted_args}"),
            ));
        }

        while !input.is_empty() {
            if input.peek(Ident) && input.peek2(syn::Token![=]) {
                let key: Ident = input.parse()?;
                let _: syn::Token![=] = input.parse()?;
                // Consume the value regardless of the key so that parsing (and diagnostics for
                // the remaining arguments) can continue past an unknown key.
                let value: syn::Expr = input.parse()?;
                if key != "flags" {
                    errors.push(syn::Error::new(
                        key.span(),
                        format!("#[{attr_name}] attribute accepts {accepted_args}"),
                    ));
                } else if custom_flags.is_some() {
                    errors.push(syn::Error::new(
                        key.span(),
                        format!("Duplicate `flags` argument in #[{attr_name}] attribute"),
                    ));
                } else {
                    *custom_flags = Some(value);
                }
            } else {
                let ty: Type = input.parse()?;
                if let Type::Path(tp) = &ty
                    && tp.qself.is_none()
                    && tp.path.is_ident("untracked")
                {
                    if !supports_untracked {
                        errors.push(syn::Error::new(
                            ty.span(),
                            format!(
                                "#[{attr_name}] does not support `untracked`; brwlocks are \
                                 always tracked by lockdep"
                            ),
                        ));
                    } else if *is_untracked {
                        errors.push(syn::Error::new(
                            ty.span(),
                            format!("Duplicate `untracked` argument in #[{attr_name}] attribute"),
                        ));
                    } else {
                        *is_untracked = true;
                        untracked_span = Some(ty.span());
                    }
                } else if custom_class.is_some() {
                    errors.push(syn::Error::new(
                        ty.span(),
                        format!("Duplicate `LockClass` argument in #[{attr_name}] attribute"),
                    ));
                } else {
                    *custom_class = Some(ty);
                }
            }

            if !input.is_empty() {
                let _: syn::Token![,] = input.parse()?;
            }
        }

        Ok(())
    }) {
        errors.push(e);
    }

    if let (Some(class), Some(flags)) = (custom_class.as_ref(), custom_flags.as_ref()) {
        let mut error = syn::Error::new(
            flags.span(),
            format!(
                "#[{attr_name}] cannot combine `flags = ...` with an explicit `LockClass`; the \
                 flags of an explicit lock class are fixed where that class is registered"
            ),
        );
        error.combine(syn::Error::new(class.span(), "explicit `LockClass` specified here"));
        errors.push(error);
    }

    if let (Some(class), Some(untracked_span)) = (custom_class.as_ref(), untracked_span) {
        let mut error = syn::Error::new(
            untracked_span,
            format!(
                "#[{attr_name}] cannot combine `untracked` with an explicit `LockClass`; whether \
                 an explicit lock class is tracked is fixed where that class is registered"
            ),
        );
        error.combine(syn::Error::new(class.span(), "explicit `LockClass` specified here"));
        errors.push(error);
    }
}

fn parse_field_attributes(
    field: &mut syn::Field,
    errors: &mut Vec<syn::Error>,
) -> FieldAttrAnalysis {
    let mut analysis = FieldAttrAnalysis {
        is_mutex: false,
        is_brwlock: false,
        guarded_by: Vec::new(),
        is_pinned: false,
        is_unpinned: false,
        custom_class: None,
        custom_flags: None,
        is_untracked: false,
    };

    field.attrs.retain(|attr| {
        if attr.path().is_ident("mutex") {
            if analysis.is_brwlock {
                errors.push(syn::Error::new(
                    attr.span(),
                    "Field cannot have both #[mutex] and #[brwlock] attributes",
                ));
                return false;
            }
            if analysis.is_mutex {
                errors.push(syn::Error::new(attr.span(), "Duplicate #[mutex] attribute on field"));
                return false;
            }
            analysis.is_mutex = true;
            if let syn::Meta::List(meta_list) = &attr.meta {
                parse_mutex_or_brwlock_args(
                    meta_list,
                    &mut analysis.custom_class,
                    &mut analysis.custom_flags,
                    &mut analysis.is_untracked,
                    errors,
                    "mutex",
                );
            } else if !matches!(attr.meta, syn::Meta::Path(_)) {
                errors.push(syn::Error::new(
                    attr.meta.span(),
                    "#[mutex] attribute must be either #[mutex], #[mutex(LockClass)], \
                     #[mutex(untracked)], or #[mutex(flags = ...)].",
                ));
            }
            false
        } else if attr.path().is_ident("brwlock") {
            if analysis.is_mutex {
                errors.push(syn::Error::new(
                    attr.span(),
                    "Field cannot have both #[mutex] and #[brwlock] attributes",
                ));
                return false;
            }
            if analysis.is_brwlock {
                errors
                    .push(syn::Error::new(attr.span(), "Duplicate #[brwlock] attribute on field"));
                return false;
            }
            analysis.is_brwlock = true;
            if let syn::Meta::List(meta_list) = &attr.meta {
                parse_mutex_or_brwlock_args(
                    meta_list,
                    &mut analysis.custom_class,
                    &mut analysis.custom_flags,
                    &mut analysis.is_untracked,
                    errors,
                    "brwlock",
                );
            } else if !matches!(attr.meta, syn::Meta::Path(_)) {
                errors.push(syn::Error::new(
                    attr.meta.span(),
                    "#[brwlock] attribute must be either #[brwlock], #[brwlock(LockClass)], or \
                     #[brwlock(flags = ...)].",
                ));
            }
            false
        } else if attr.path().is_ident("guarded_by") {
            if let syn::Meta::List(meta_list) = &attr.meta {
                match meta_list
                    .parse_args_with(Punctuated::<Ident, syn::Token![,]>::parse_terminated)
                {
                    Ok(idents) if idents.is_empty() => {
                        errors.push(syn::Error::new(
                            attr.meta.span(),
                            "#[guarded_by(...)] requires at least one lock identifier",
                        ));
                    }
                    Ok(idents) => {
                        for ident in idents {
                            if analysis.guarded_by.contains(&ident) {
                                errors.push(syn::Error::new(
                                    ident.span(),
                                    format!("Duplicate lock '{ident}' in #[guarded_by] attribute"),
                                ));
                            } else {
                                analysis.guarded_by.push(ident);
                            }
                        }
                    }
                    Err(e) => errors.push(e),
                }
            } else {
                errors.push(syn::Error::new(
                    attr.meta.span(),
                    "#[guarded_by] attribute must be a list of lock identifiers, e.g., \
                     #[guarded_by(lock1, lock2)]",
                ));
            }
            false
        } else if attr.path().is_ident("pin") {
            analysis.is_pinned = true;
            true
        } else if attr.path().is_ident("ksync") {
            if let syn::Meta::List(meta_list) = &attr.meta
                && let Ok(ident) = meta_list.parse_args::<Ident>()
                && ident == "unpinned"
            {
                // TODO(https://fxbug.dev/536051491): Remove this workaround once
                // WavlTree/region-alloc support safe pin projection.
                analysis.is_unpinned = true;
                return false;
            }
            true
        } else {
            true
        }
    });

    analysis
}

fn check_unique_name(
    name: &str,
    span: proc_macro2::Span,
    field_ident: &Ident,
    kind: &str,
    generated_names: &mut std::collections::HashSet<String>,
    errors: &mut Vec<syn::Error>,
) {
    if !generated_names.insert(name.to_string()) {
        errors.push(syn::Error::new(
            span,
            format!(
                "The lock field '{field_ident}' generates the duplicate {kind} name '{name}'. \
                 Please use distinct field names."
            ),
        ));
    }
}

struct GenericsInfo<'a> {
    params_no_defaults: Punctuated<syn::GenericParam, syn::token::Comma>,
    ty_params: Vec<proc_macro2::TokenStream>,
    class_params_no_defaults: Punctuated<syn::GenericParam, syn::token::Comma>,
    class_ty_params: Vec<proc_macro2::TokenStream>,
    phantom_ty_params: Vec<&'a Ident>,
    phantom_lifetimes: Vec<&'a syn::Lifetime>,
    all_generic_idents: Vec<&'a Ident>,
}

fn extract_generics_info<'a>(generics: &'a syn::Generics) -> GenericsInfo<'a> {
    let mut ty_params = Vec::new();
    let mut class_ty_params = Vec::new();
    let mut class_params_no_defaults = Punctuated::new();
    let mut phantom_ty_params = Vec::new();
    let mut phantom_lifetimes = Vec::new();
    let mut all_generic_idents = Vec::new();

    for param in &generics.params {
        match param {
            syn::GenericParam::Type(type_param) => {
                let ident = &type_param.ident;
                ty_params.push(quote! { #ident });
                class_ty_params.push(quote! { #ident });
                phantom_ty_params.push(ident);
                all_generic_idents.push(ident);

                let mut p = type_param.clone();
                p.default = None;
                class_params_no_defaults.push(syn::GenericParam::Type(p));
            }
            syn::GenericParam::Const(const_param) => {
                let ident = &const_param.ident;
                ty_params.push(quote! { #ident });
                class_ty_params.push(quote! { #ident });
                all_generic_idents.push(ident);

                let mut p = const_param.clone();
                p.default = None;
                class_params_no_defaults.push(syn::GenericParam::Const(p));
            }
            syn::GenericParam::Lifetime(lifetime_param) => {
                let lifetime = &lifetime_param.lifetime;
                ty_params.push(quote! { #lifetime });
                class_ty_params.push(quote! { #lifetime });
                phantom_lifetimes.push(lifetime);

                class_params_no_defaults.push(syn::GenericParam::Lifetime(lifetime_param.clone()));
            }
        }
    }

    let mut params_no_defaults = generics.params.clone();
    for param in &mut params_no_defaults {
        match param {
            syn::GenericParam::Type(type_param) => type_param.default = None,
            syn::GenericParam::Const(const_param) => const_param.default = None,
            syn::GenericParam::Lifetime(_) => {}
        }
    }

    GenericsInfo {
        params_no_defaults,
        ty_params,
        class_params_no_defaults,
        class_ty_params,
        phantom_ty_params,
        phantom_lifetimes,
        all_generic_idents,
    }
}

/// Holds the generated accessor methods and field projection entries for a single lock (`mu`) on a
/// `#[guarded]` struct.
///
/// For each field `#[guarded_by(...)]` whose lock list includes `mu`:
/// - **Read access (`read_*`)**: Holding `mu` alone is sufficient to read both single-lock
///   (`#[guarded_by(mu)]`) and multi-lock (`#[guarded_by(mu, other)]`) fields, because mutating the
///   field requires holding *all* guarding locks simultaneously.
/// - **Write access (`write_*`)**:
///   - Single-lock fields (`#[guarded_by(mu)]`) are projected mutably (`&'b mut T`) in `FieldsMut`
///     and their `_mut(&mut self)` accessor requires no extra arguments.
///   - Multi-lock fields (`#[guarded_by(mu, other, ...)]`) are projected immutably (`&'b T`) in
///     `FieldsMut` (since `FieldsMut` only proves `mu` is held), while their `_mut` / `_cell`
///     accessors accept an `&'f mut impl ExclusiveLockToken<OtherClass>` parameter for each
///     remaining lock, assert that any guard arguments belong to the same parent struct instance,
///     and join all lock tokens before calling `KCell::get_mut` / `KOnceCell::guard`.
#[derive(Default)]
struct FieldCode {
    read_token_guard_accessors: proc_macro2::TokenStream,
    write_token_guard_accessors: proc_macro2::TokenStream,
    read_guard_accessors: proc_macro2::TokenStream,
    write_guard_accessors: proc_macro2::TokenStream,
    read_fields_decl: proc_macro2::TokenStream,
    write_fields_decl: proc_macro2::TokenStream,
    read_fields_init: proc_macro2::TokenStream,
    write_fields_init: proc_macro2::TokenStream,
}

fn generate_field_code(
    guarded_fields: &[&GuardedField],
    current_lock_ident: &Ident,
    class_type: &proc_macro2::TokenStream,
    all_locks: &[&MutexField],
    parent_ty: &proc_macro2::TokenStream,
) -> FieldCode {
    let mut code = FieldCode::default();

    for f in guarded_fields {
        let f_ident = &f.ident;
        let f_ty = &f.ty;
        let f_vis = &f.vis;
        let f_mut_ident = format_ident!("{}_mut", f_ident);
        let f_cell_ident = format_ident!("{}_cell", f_ident);
        let is_multi_lock = f.mutex_idents.len() > 1;

        // 1. Read accessors (`guard.field()`) and read-only field projections (`guard.fields()`).
        //    Identical for single-lock and multi-lock fields: holding `current_lock_ident` is
        //    sufficient for shared read access via `LockTokenRead`.
        if f.is_konce_cell {
            let inner_ty = f.konce_cell_inner_ty.as_ref().unwrap();

            code.read_token_guard_accessors.extend(quote! {
                #[inline]
                #f_vis fn #f_ident(&self) -> ::core::option::Option<&#inner_ty> {
                    // SAFETY: The lock token proves that a lock protecting this cell is held.
                    unsafe { self.parent.#f_ident.get(self.token) }
                }
            });

            code.read_guard_accessors.extend(quote! {
                #[inline]
                #f_vis fn #f_ident(&self) -> ::core::option::Option<&#inner_ty> {
                    // SAFETY: The lock token proves that a lock protecting this cell is held.
                    unsafe { self.parent.#f_ident.get(self.inner.token()) }
                }
            });

            code.read_fields_decl.extend(quote! {
                #[allow(dead_code)]
                #f_vis #f_ident: ::core::option::Option<&'b #inner_ty>,
            });

            code.read_fields_init.extend(quote! {
                // SAFETY: The guard token proves shared access to the cell.
                #f_ident: unsafe { me.parent.#f_ident.get(token) },
            });
        } else {
            code.read_token_guard_accessors.extend(quote! {
                #[inline]
                #f_vis fn #f_ident(&self) -> &#f_ty {
                    // SAFETY: The lock token proves that a lock protecting this cell is held.
                    unsafe { self.parent.#f_ident.get(self.token) }
                }
            });

            code.read_guard_accessors.extend(quote! {
                #[inline]
                #f_vis fn #f_ident(&self) -> &#f_ty {
                    // SAFETY: The lock token proves that a lock protecting this cell is held.
                    unsafe { self.parent.#f_ident.get(self.inner.token()) }
                }
            });

            code.read_fields_decl.extend(quote! {
                #[allow(dead_code)]
                #f_vis #f_ident: &'b #f_ty,
            });

            code.read_fields_init.extend(quote! {
                // SAFETY: The guard token proves shared access to the cell.
                #f_ident: unsafe { me.parent.#f_ident.get(token) },
            });
        }

        // 2. Write accessors (`guard.as_mut().field_mut(...)`) and `fields_mut()` projections.
        //    - For single-lock fields (`#[guarded_by(mu1)]`):
        //      * `other_token_params` is empty, so
        //        `field_mut(self: Pin<&'f mut Self>) -> &'f mut T`.
        //      * `joined_guard_expr` is `self_token`.
        //      * `fields_mut()` projects the field mutably (`&'b mut T`).
        //    - For multi-lock fields (`#[guarded_by(mu1, mu2)]`):
        //      * `other_token_params` contains
        //        `mu2_token: &'f mut impl ExclusiveLockToken<Mu2Class>`.
        //      * `joined_guard_expr` asserts `mu2_token` belongs to the same struct instance (if
        //        bound to a parent guard) and joins the tokens via
        //        `self_token.join(mu2_token.lock_token_mut())`.
        //      * `fields_mut()` projects the field immutably (`&'b T`).
        let cell_class_type = if is_multi_lock {
            let lock_class_types: Vec<&proc_macro2::TokenStream> = f
                .mutex_idents
                .iter()
                .map(|id| {
                    &all_locks
                        .iter()
                        .find(|m| m.ident == *id)
                        .expect("validated lock ident")
                        .class_type
                })
                .collect();
            quote! { (#(#lock_class_types),*) }
        } else {
            quote! { #class_type }
        };

        let other_token_params: Vec<proc_macro2::TokenStream> = f
            .mutex_idents
            .iter()
            .filter(|id| *id != current_lock_ident)
            .map(|id| {
                let param_ident = format_ident!("{}_token", id);
                let other_class_type = &all_locks
                    .iter()
                    .find(|m| m.ident == *id)
                    .expect("validated lock ident")
                    .class_type;
                quote! {
                    #param_ident: &'f mut impl ::ksync::ExclusiveLockToken<
                        #parent_ty,
                        #other_class_type,
                    >
                }
            })
            .collect();

        let token_guard_parent_asserts: Vec<proc_macro2::TokenStream> = f
            .mutex_idents
            .iter()
            .filter(|id| *id != current_lock_ident)
            .map(|id| {
                let param_ident = format_ident!("{}_token", id);
                quote! {
                    assert!(
                        ::core::ptr::eq(
                            ::core::ptr::from_ref(self.parent),
                            ::ksync::ExclusiveLockToken::parent(#param_ident),
                        ),
                        "Lock guard belongs to a different struct instance",
                    );
                }
            })
            .collect();

        let guard_parent_asserts: Vec<proc_macro2::TokenStream> = f
            .mutex_idents
            .iter()
            .filter(|id| *id != current_lock_ident)
            .map(|id| {
                let param_ident = format_ident!("{}_token", id);
                quote! {
                    assert!(
                        ::core::ptr::eq(
                            ::core::ptr::from_ref(me.parent),
                            ::ksync::ExclusiveLockToken::parent(#param_ident),
                        ),
                        "Lock guard belongs to a different struct instance",
                    );
                }
            })
            .collect();

        let token_guard_parts: Vec<proc_macro2::TokenStream> = f
            .mutex_idents
            .iter()
            .map(|id| {
                if id == current_lock_ident {
                    if is_multi_lock {
                        quote! { (&mut *self.token) }
                    } else {
                        quote! { &mut *self.token }
                    }
                } else {
                    let param_ident = format_ident!("{}_token", id);
                    quote! { ::ksync::ExclusiveLockToken::lock_token_mut(#param_ident) }
                }
            })
            .collect();
        let mut joined_token_guard_expr = token_guard_parts[0].clone();
        for part in &token_guard_parts[1..] {
            joined_token_guard_expr = quote! { #joined_token_guard_expr.join(#part) };
        }
        if !token_guard_parent_asserts.is_empty() {
            joined_token_guard_expr = quote! {
                {
                    #(#token_guard_parent_asserts)*
                    #joined_token_guard_expr
                }
            };
        }

        let guard_parts: Vec<proc_macro2::TokenStream> = f
            .mutex_idents
            .iter()
            .map(|id| {
                if id == current_lock_ident {
                    quote! { self_token }
                } else {
                    let param_ident = format_ident!("{}_token", id);
                    quote! { ::ksync::ExclusiveLockToken::lock_token_mut(#param_ident) }
                }
            })
            .collect();
        let mut joined_guard_expr = guard_parts[0].clone();
        for part in &guard_parts[1..] {
            joined_guard_expr = quote! { #joined_guard_expr.join(#part) };
        }
        if !guard_parent_asserts.is_empty() {
            joined_guard_expr = quote! {
                {
                    #(#guard_parent_asserts)*
                    #joined_guard_expr
                }
            };
        }

        if f.is_konce_cell {
            // Branch A: `KOnceCell<InnerTy>` fields (`field() -> Option<&InnerTy>`,
            // `field_mut(...) -> Option<&mut InnerTy>`, `field_cell(...) -> KOnceCellGuard<...>`).
            let inner_ty = f.konce_cell_inner_ty.as_ref().unwrap();

            code.write_token_guard_accessors.extend(quote! {
                #[inline]
                #f_vis fn #f_ident(&self) -> ::core::option::Option<&#inner_ty> {
                    // SAFETY: The lock token proves that a lock protecting this cell is held.
                    unsafe { self.parent.#f_ident.get(&*self.token) }
                }

                #[inline]
                #f_vis fn #f_mut_ident<'f>(
                    &'f mut self,
                    #(#other_token_params),*
                ) -> ::core::option::Option<&'f mut #inner_ty> {
                    let token = #joined_token_guard_expr;
                    // SAFETY: We hold exclusive mutable references to all lock tokens guarding
                    // this cell.
                    unsafe { self.parent.#f_ident.get_mut(token) }
                }

                #[inline]
                #f_vis fn #f_cell_ident<'f>(
                    &'f mut self,
                    #(#other_token_params),*
                ) -> ::ksync::KOnceCellGuard<'f, #inner_ty, #cell_class_type> {
                    let token = #joined_token_guard_expr;
                    // SAFETY: We hold exclusive mutable references to all lock tokens guarding
                    // this cell.
                    unsafe { self.parent.#f_ident.guard(token) }
                }
            });

            code.write_guard_accessors.extend(quote! {
                #[inline]
                #f_vis fn #f_ident(&self) -> ::core::option::Option<&#inner_ty> {
                    // SAFETY: The lock token proves that a lock protecting this cell is held.
                    unsafe { self.parent.#f_ident.get(self.inner.token()) }
                }

                #[inline]
                #f_vis fn #f_mut_ident<'f>(
                    self: ::core::pin::Pin<&'f mut Self>,
                    #(#other_token_params),*
                ) -> ::core::option::Option<&'f mut #inner_ty> {
                    // SAFETY: Projecting `inner` as pinned does not move any pinned fields.
                    let me = unsafe { self.get_unchecked_mut() };
                    // SAFETY: `inner` is structurally pinned.
                    let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
                    let self_token = inner_pin.token_mut();
                    let token = #joined_guard_expr;
                    // SAFETY: We hold exclusive mutable references to all lock tokens guarding
                    // this cell.
                    unsafe { me.parent.#f_ident.get_mut(token) }
                }

                #[inline]
                #f_vis fn #f_cell_ident<'f>(
                    self: ::core::pin::Pin<&'f mut Self>,
                    #(#other_token_params),*
                ) -> ::ksync::KOnceCellGuard<'f, #inner_ty, #cell_class_type> {
                    // SAFETY: Projecting `inner` as pinned does not move any pinned fields.
                    let me = unsafe { self.get_unchecked_mut() };
                    // SAFETY: `inner` is structurally pinned.
                    let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
                    let self_token = inner_pin.token_mut();
                    let token = #joined_guard_expr;
                    // SAFETY: We hold exclusive mutable references to all lock tokens guarding
                    // this cell.
                    unsafe { me.parent.#f_ident.guard(token) }
                }
            });

            if is_multi_lock {
                code.write_fields_decl.extend(quote! {
                    #[allow(dead_code)]
                    #f_vis #f_ident: ::core::option::Option<&'b #inner_ty>,
                });

                code.write_fields_init.extend(quote! {
                    // SAFETY: Holding one of the guarding locks guarantees shared access to the
                    // cell.
                    #f_ident: unsafe { me.parent.#f_ident.get_unchecked() },
                });
            } else {
                code.write_fields_decl.extend(quote! {
                    #[allow(dead_code)]
                    #f_vis #f_ident: ::core::option::Option<&'b mut #inner_ty>,
                });

                code.write_fields_init.extend(quote! {
                    // SAFETY: We hold exclusive access to the guard token and each field cell
                    // is disjoint.
                    #f_ident: unsafe { me.parent.#f_ident.get_mut_unchecked() },
                });
            }
        } else if f.project_as_pin {
            // Branch B: Structurally pinned `#[pin]` fields (`field() -> &T`,
            // `field_mut(...) -> Pin<&mut T>`).
            code.write_token_guard_accessors.extend(quote! {
                #[inline]
                #f_vis fn #f_ident(&self) -> &#f_ty {
                    // SAFETY: The lock token proves that a lock protecting this cell is held.
                    unsafe { self.parent.#f_ident.get(&*self.token) }
                }

                #[inline]
                #f_vis fn #f_mut_ident<'f>(
                    &'f mut self,
                    #(#other_token_params),*
                ) -> ::core::pin::Pin<&'f mut #f_ty> {
                    let token = #joined_token_guard_expr;
                    // SAFETY: We hold exclusive mutable references to all lock tokens guarding
                    // this cell, and the parent struct is pinned so structurally pinned fields
                    // remain pinned.
                    unsafe { ::core::pin::Pin::new_unchecked(self.parent.#f_ident.get_mut(token)) }
                }
            });

            code.write_guard_accessors.extend(quote! {
                #[inline]
                #f_vis fn #f_ident(&self) -> &#f_ty {
                    // SAFETY: The lock token proves that a lock protecting this cell is held.
                    unsafe { self.parent.#f_ident.get(self.inner.token()) }
                }

                #[inline]
                #f_vis fn #f_mut_ident<'f>(
                    self: ::core::pin::Pin<&'f mut Self>,
                    #(#other_token_params),*
                ) -> ::core::pin::Pin<&'f mut #f_ty> {
                    // SAFETY: Projecting `inner` as pinned does not move any pinned fields.
                    let me = unsafe { self.get_unchecked_mut() };
                    // SAFETY: `inner` is structurally pinned.
                    let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
                    let self_token = inner_pin.token_mut();
                    let token = #joined_guard_expr;
                    // SAFETY: We hold exclusive mutable references to all lock tokens guarding
                    // this cell, and the parent is pinned.
                    unsafe { ::core::pin::Pin::new_unchecked(me.parent.#f_ident.get_mut(token)) }
                }
            });

            if is_multi_lock {
                code.write_fields_decl.extend(quote! {
                    #[allow(dead_code)]
                    #f_vis #f_ident: &'b #f_ty,
                });

                code.write_fields_init.extend(quote! {
                    // SAFETY: Holding one of the guarding locks guarantees shared access to the
                    // cell.
                    #f_ident: unsafe { &*me.parent.#f_ident.as_ptr(&*token) },
                });
            } else {
                code.write_fields_decl.extend(quote! {
                    #[allow(dead_code)]
                    #f_vis #f_ident: ::core::pin::Pin<&'b mut #f_ty>,
                });

                code.write_fields_init.extend(quote! {
                    // SAFETY: We hold exclusive access to the guard token, each field cell
                    // is disjoint, and the parent struct is pinned so structurally pinned
                    // fields remain pinned.
                    #f_ident: unsafe {
                        ::core::pin::Pin::new_unchecked(&mut *me.parent.#f_ident.as_mut_ptr(token))
                    },
                });
            }
        } else {
            // Branch C: Standard `KCell<T>` fields (`field() -> &T`, `field_mut(...) -> &mut T`).
            code.write_token_guard_accessors.extend(quote! {
                #[inline]
                #f_vis fn #f_ident(&self) -> &#f_ty {
                    // SAFETY: The lock token proves that a lock protecting this cell is held.
                    unsafe { self.parent.#f_ident.get(&*self.token) }
                }

                #[inline]
                #f_vis fn #f_mut_ident<'f>(
                    &'f mut self,
                    #(#other_token_params),*
                ) -> &'f mut #f_ty {
                    let token = #joined_token_guard_expr;
                    // SAFETY: We hold exclusive mutable references to all lock tokens guarding
                    // this cell.
                    unsafe { self.parent.#f_ident.get_mut(token) }
                }
            });

            code.write_guard_accessors.extend(quote! {
                #[inline]
                #f_vis fn #f_ident(&self) -> &#f_ty {
                    // SAFETY: The lock token proves that a lock protecting this cell is held.
                    unsafe { self.parent.#f_ident.get(self.inner.token()) }
                }

                #[inline]
                #f_vis fn #f_mut_ident<'f>(
                    self: ::core::pin::Pin<&'f mut Self>,
                    #(#other_token_params),*
                ) -> &'f mut #f_ty {
                    // SAFETY: Projecting `inner` as pinned does not move any pinned fields.
                    let me = unsafe { self.get_unchecked_mut() };
                    // SAFETY: `inner` is structurally pinned.
                    let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
                    let self_token = inner_pin.token_mut();
                    let token = #joined_guard_expr;
                    // SAFETY: We hold exclusive mutable references to all lock tokens guarding
                    // this cell.
                    unsafe { me.parent.#f_ident.get_mut(token) }
                }
            });

            if is_multi_lock {
                code.write_fields_decl.extend(quote! {
                    #[allow(dead_code)]
                    #f_vis #f_ident: &'b #f_ty,
                });

                code.write_fields_init.extend(quote! {
                    // SAFETY: Holding one of the guarding locks guarantees shared access to the
                    // cell.
                    #f_ident: unsafe { &*me.parent.#f_ident.as_ptr(&*token) },
                });
            } else {
                code.write_fields_decl.extend(quote! {
                    #[allow(dead_code)]
                    #f_vis #f_ident: &'b mut #f_ty,
                });

                code.write_fields_init.extend(quote! {
                    // SAFETY: We hold exclusive access to the guard token and each field cell
                    // is disjoint.
                    #f_ident: unsafe { &mut *me.parent.#f_ident.as_mut_ptr(token) },
                });
            }
        }
    }

    code
}

/// Resolves the lock class marker type for a `#[mutex]` / `#[brwlock]` field: the explicit
/// `custom_class` if one was given, otherwise a `<Struct><Field>Class` marker to be generated.
fn resolve_lock_class(
    custom_class: Option<&Type>,
    struct_ident: &Ident,
    mu_camel: &str,
    field_ident: &Ident,
    generics_info: &GenericsInfo<'_>,
    generated_names: &mut std::collections::HashSet<String>,
    errors: &mut Vec<syn::Error>,
) -> (Ident, proc_macro2::TokenStream) {
    match custom_class {
        Some(ty) => {
            // This uniqueness check is purely syntactic (token spelling): a proc-macro has no
            // type information, so `Class` vs. `self::Class` or a type alias are not detected.
            let class_str = quote! { #ty }.to_string();
            if !generated_names.insert(class_str.clone()) {
                errors.push(syn::Error::new(
                    ty.span(),
                    format!(
                        "Lock class `{class_str}` is already used by another lock field of this \
                         struct; each lock field needs a distinct lock class"
                    ),
                ));
            }
            let ident = if let syn::Type::Path(syn::TypePath { path, .. }) = ty {
                path.segments
                    .last()
                    .map(|s| s.ident.clone())
                    .unwrap_or_else(|| format_ident!("CustomClass"))
            } else {
                format_ident!("CustomClass")
            };
            (ident, quote! { #ty })
        }
        None => {
            let class_name = format!("{struct_ident}{mu_camel}Class");
            check_unique_name(
                &class_name,
                field_ident.span(),
                field_ident,
                "class",
                generated_names,
                errors,
            );
            let ident = format_ident!("{class_name}");
            let ty = if generics_info.class_ty_params.is_empty() {
                quote! { #ident }
            } else {
                let class_ty_params = &generics_info.class_ty_params;
                quote! { #ident <#(#class_ty_params),*> }
            };
            (ident, ty)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn generate_lock_class_registration(
    struct_ident: &Ident,
    lock_ident: &Ident,
    mu_camel: &str,
    class_ident: &Ident,
    class_impl_generics: &proc_macro2::TokenStream,
    class_ty_generics: &proc_macro2::TokenStream,
    where_clause: Option<&syn::WhereClause>,
    flags_expr: proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    let struct_upper = struct_ident.to_string().to_ascii_uppercase();
    let mu_upper = mu_camel.to_ascii_uppercase();
    let reg_ident = format_ident!("{}_{}_REGISTRATION", struct_upper, mu_upper);
    let string_reg_ident = format_ident!("{}_{}_STRING_REG", struct_upper, mu_upper);
    let flags_ident = format_ident!("{}_{}_FLAGS", struct_upper, mu_upper);
    let path_name = format!("{}::{}", struct_ident, lock_ident);

    // `flags_expr` never references the struct's generics (callers use
    // `generate_generic_lock_class_registration` for lock types that mention them), so a single
    // scoped `const` can serve as the source of truth for both the registration and
    // `LockClass::FLAGS`.
    quote! {
        const _: () = {
            ::ksync::declare_interned_string!(#string_reg_ident, #path_name);

            const #flags_ident: ::ksync::LockFlags = #flags_expr;

            #[unsafe(link_section = "rust_lock_classes")]
            #[used]
            static #reg_ident: ::ksync::LockClassRegistration =
                ::ksync::LockClassRegistration::with_flags(#string_reg_ident, #flags_ident);

            impl #class_impl_generics ::ksync::LockClass
                for #class_ident #class_ty_generics #where_clause
            {
                const ID: *mut ::core::ffi::c_void = #reg_ident.get();
                const FLAGS: ::ksync::LockFlags = #flags_ident;
            }
        };
    }
}

#[allow(clippy::too_many_arguments)]
fn generate_generic_lock_class_registration(
    struct_ident: &Ident,
    lock_ident: &Ident,
    mu_camel: &str,
    class_ident: &Ident,
    class_impl_generics: &proc_macro2::TokenStream,
    class_ty_generics: &proc_macro2::TokenStream,
    where_clause: Option<&syn::WhereClause>,
    mutex_type: &proc_macro2::TokenStream,
    custom_flags: Option<&syn::Expr>,
) -> proc_macro2::TokenStream {
    let struct_upper = struct_ident.to_string().to_ascii_uppercase();
    let mu_upper = mu_camel.to_ascii_uppercase();
    let reg_ident = format_ident!("{}_{}_REGISTRATION", struct_upper, mu_upper);
    let reg_irq_safe_ident = format_ident!("{}_{}_IRQ_SAFE_REGISTRATION", struct_upper, mu_upper);
    let string_reg_ident = format_ident!("{}_{}_STRING_REG", struct_upper, mu_upper);
    let string_irq_safe_reg_ident =
        format_ident!("{}_{}_IRQ_SAFE_STRING_REG", struct_upper, mu_upper);
    let path_name = format!("{}::{}", struct_ident, lock_ident);
    let irq_safe_path_name = format!("{}::{} (irq_safe)", struct_ident, lock_ident);
    let base_flags_expr = match custom_flags {
        Some(flags) => quote! { (#flags) },
        None => quote! { ::ksync::LOCK_FLAGS_NONE },
    };

    quote! {
        const _: () = {
            ::ksync::declare_interned_string!(#string_reg_ident, #path_name);
            ::ksync::declare_interned_string!(#string_irq_safe_reg_ident, #irq_safe_path_name);

            // SAFETY: `LockClassRegistration` matches C++ section layout/alignment, uses
            // interior-mutable writable storage (`OpaqueBytes`), and holds a valid static
            // `InternedString` pointer.
            #[unsafe(link_section = "rust_lock_classes")]
            #[used]
            static #reg_ident: ::ksync::LockClassRegistration =
                ::ksync::LockClassRegistration::with_flags(#string_reg_ident, #base_flags_expr);

            // SAFETY: `LockClassRegistration` matches C++ section layout/alignment, uses
            // interior-mutable writable storage (`OpaqueBytes`), and holds a valid static
            // `InternedString` pointer.
            #[unsafe(link_section = "rust_lock_classes")]
            #[used]
            static #reg_irq_safe_ident: ::ksync::LockClassRegistration =
                ::ksync::LockClassRegistration::with_flags(
                    #string_irq_safe_reg_ident,
                    ::ksync::LOCK_FLAGS_IRQ_SAFE | (#base_flags_expr),
                );

            impl #class_impl_generics ::ksync::LockClass
                for #class_ident #class_ty_generics #where_clause
            {
                const ID: *mut ::core::ffi::c_void = {
                    const {
                        assert!(
                            (<#mutex_type as ::ksync::RawLock>::LOCK_FLAGS
                                & !(::ksync::LOCK_FLAGS_IRQ_SAFE
                                    | ::ksync::LOCK_FLAGS_TRACKING_DISABLED))
                                == 0,
                            "Generic lock types in #[guarded] may only set LOCK_FLAGS_IRQ_SAFE \
                             or LOCK_FLAGS_TRACKING_DISABLED"
                        );
                    }
                    if (<#mutex_type as ::ksync::RawLock>::LOCK_FLAGS
                        & ::ksync::LOCK_FLAGS_TRACKING_DISABLED)
                        != 0
                    {
                        ::core::ptr::null_mut()
                    } else if (<#mutex_type as ::ksync::RawLock>::LOCK_FLAGS
                        & ::ksync::LOCK_FLAGS_IRQ_SAFE)
                        != 0
                    {
                        #reg_irq_safe_ident.get()
                    } else {
                        #reg_ident.get()
                    }
                };
                const FLAGS: ::ksync::LockFlags =
                    <#mutex_type as ::ksync::RawLock>::LOCK_FLAGS | (#base_flags_expr);

            }
        };
    }
}

fn generate_fields_struct(
    struct_vis: &syn::Visibility,
    fields_ident: &Ident,
    fields_decl_generics: &proc_macro2::TokenStream,
    where_clause: Option<&syn::WhereClause>,
    fields_decl: &proc_macro2::TokenStream,
    phantom_lifetimes: &[&syn::Lifetime],
    phantom_ty_params: &[&Ident],
) -> proc_macro2::TokenStream {
    quote! {
        #[allow(dead_code)]
        #struct_vis struct #fields_ident #fields_decl_generics #where_clause {
            #fields_decl
            _marker: ::core::marker::PhantomData<(
                &'b (),
                #(& #phantom_lifetimes (),)*
                #(#phantom_ty_params),*
            )>,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn generate_token_guard(
    struct_vis: &syn::Visibility,
    struct_ident: &Ident,
    ty_generics: &impl quote::ToTokens,
    where_clause: Option<&syn::WhereClause>,
    class_type: &proc_macro2::TokenStream,
    token_guard_ident: &Ident,
    fields_ident: &Ident,
    token_guard_decl_generics: &proc_macro2::TokenStream,
    token_guard_impl_generics: &proc_macro2::TokenStream,
    token_guard_ty_generics: &proc_macro2::TokenStream,
    fields_ret_ty_generics: &proc_macro2::TokenStream,
    accessors: &proc_macro2::TokenStream,
    fields_init: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    quote! {
        #[allow(dead_code)]
        #struct_vis struct #token_guard_ident #token_guard_decl_generics #where_clause {
            parent: &'b #struct_ident #ty_generics,
            token: &'b ::ksync::LockToken<'a, #class_type>,
        }

        impl #token_guard_impl_generics #token_guard_ident #token_guard_ty_generics #where_clause {
            #accessors

            #[inline]
            #struct_vis fn parent(&self) -> &#struct_ident #ty_generics {
                self.parent
            }

            #[inline]
            #struct_vis fn token(&self) -> &::ksync::LockToken<'a, #class_type> {
                self.token
            }

            #[inline]
            #[allow(unused_variables)]
            #struct_vis fn fields<'f>(&'f self) -> #fields_ident #fields_ret_ty_generics {
                let me = self;
                let token = me.token;
                #fields_ident {
                    #fields_init
                    _marker: ::core::marker::PhantomData,
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn generate_token_guard_mut(
    struct_vis: &syn::Visibility,
    struct_ident: &Ident,
    ty_generics: &impl quote::ToTokens,
    where_clause: Option<&syn::WhereClause>,
    class_type: &proc_macro2::TokenStream,
    token_guard_ident: &Ident,
    token_guard_mut_ident: &Ident,
    read_fields_ident: &Ident,
    write_fields_ident: &Ident,
    token_guard_decl_generics: &proc_macro2::TokenStream,
    token_guard_impl_generics: &proc_macro2::TokenStream,
    token_guard_ty_generics: &proc_macro2::TokenStream,
    fields_ret_ty_generics: &proc_macro2::TokenStream,
    ty_params: &[proc_macro2::TokenStream],
    accessors: &proc_macro2::TokenStream,
    read_fields_init: &proc_macro2::TokenStream,
    write_fields_init: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    quote! {
        #[allow(dead_code)]
        #struct_vis struct #token_guard_mut_ident #token_guard_decl_generics #where_clause {
            parent: &'b #struct_ident #ty_generics,
            token: &'b mut ::ksync::LockToken<'a, #class_type>,
        }

        // SAFETY: `#token_guard_mut_ident` holds an exclusive mutable reference to the lock token
        // for `self.parent`.
        unsafe impl #token_guard_impl_generics
            ::ksync::ExclusiveLockToken<#struct_ident #ty_generics, #class_type>
            for #token_guard_mut_ident #token_guard_ty_generics #where_clause
        {
            #[inline]
            fn parent(&self) -> *const #struct_ident #ty_generics {
                ::core::ptr::from_ref(self.parent)
            }

            #[inline]
            fn lock_token_mut(&mut self) -> &mut ::ksync::LockToken<'_, #class_type> {
                self.token_mut().reborrow()
            }
        }

        impl #token_guard_impl_generics #token_guard_mut_ident #token_guard_ty_generics
            #where_clause
        {
            #accessors

            #[inline]
            #struct_vis fn parent(&self) -> &#struct_ident #ty_generics {
                self.parent
            }

            #[inline]
            #struct_vis fn as_token_guard<'c>(
                &'c self,
            ) -> #token_guard_ident <'c, 'a, #(#ty_params),*> {
                #token_guard_ident {
                    parent: self.parent,
                    token: &*self.token,
                }
            }

            #[inline]
            #struct_vis fn reborrow<'c>(
                &'c mut self,
            ) -> #token_guard_mut_ident <'c, 'a, #(#ty_params),*> {
                #token_guard_mut_ident {
                    parent: self.parent,
                    token: &mut *self.token,
                }
            }

            #[inline]
            #struct_vis fn token(&self) -> &::ksync::LockToken<'a, #class_type> {
                &*self.token
            }

            #[inline]
            #struct_vis fn token_mut(&mut self) -> &mut ::ksync::LockToken<'a, #class_type> {
                &mut *self.token
            }

            #[inline]
            #[allow(unused_variables)]
            #struct_vis fn fields<'f>(&'f self) -> #read_fields_ident #fields_ret_ty_generics {
                let me = self;
                let token = &*me.token;
                #read_fields_ident {
                    #read_fields_init
                    _marker: ::core::marker::PhantomData,
                }
            }

            #[inline]
            #[allow(unused_variables)]
            #struct_vis fn fields_mut<'f>(
                &'f mut self,
            ) -> #write_fields_ident #fields_ret_ty_generics {
                let me = self;
                let token = &mut *me.token;
                #write_fields_ident {
                    #write_fields_init
                    _marker: ::core::marker::PhantomData,
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn generate_pinned_guard_struct(
    struct_vis: &syn::Visibility,
    struct_ident: &Ident,
    guard_ident: &Ident,
    guard_decl_generics: &proc_macro2::TokenStream,
    guard_impl_generics: &proc_macro2::TokenStream,
    guard_ty_generics: &proc_macro2::TokenStream,
    ty_generics: &impl quote::ToTokens,
    where_clause: Option<&syn::WhereClause>,
    inner_type: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    quote! {
        #[pin_init::pin_data(PinnedDrop)]
        #struct_vis struct #guard_ident #guard_decl_generics #where_clause {
            parent: &'a #struct_ident #ty_generics,
            #[pin]
            inner: #inner_type,
        }

        #[pin_init::pinned_drop]
        impl #guard_impl_generics pin_init::PinnedDrop
            for #guard_ident #guard_ty_generics #where_clause
        {
            fn drop(self: ::core::pin::Pin<&mut Self>) {}
        }
    }
}

/// Instruments a struct with lock-class registration, `KCell`/`KOnceCell` field wrapping, and
/// type-safe lock guard accessors.
///
/// # Attributes
///
/// - `#[mutex]` / `#[mutex(flags = ...)]` / `#[mutex(CustomClass)]`: Marks a `KMutex<L>` field,
///   where `L` is the raw lock type (e.g. the kernel's `TrackedMutex`, `TrackedSpinlock`, or
///   `TrackedMonitoredSpinlock`; there is no default), and generates `lock_<name>()` /
///   `guard_<name>()` methods, plus `lock_<name>_policy[_with]()`,
///   `lock_<name>_aliased[_policy[_with]]()`, and `lock_<name>_multiple[_policy]()` variants for
///   custom lock policies (e.g. `CriticalMutexOrderedPolicy`), aliased locks, and address-ordered
///   multi-lock acquisition
///   (there is no `lock_<name>_multiple_policy_with()`; see `KMutex::lock_multiple_policy_with`).
///   (Monitored spinlock fields have no `*_with` variants: their `lock_<name>_policy(args)` and
///   `lock_<name>_aliased_policy(alias, args)` take the policy's arguments directly; `PhantomMutex`
///   fields only get `lock_<name>()`.)
///   Without `CustomClass`, a lock class is generated and registered with the raw lock's
///   flags plus `flags`; with `CustomClass`, the field uses that existing lock class (whose flags
///   are fixed where it is registered, so `flags` cannot be combined with it). `#[mutex]` also
///   supports generic lock types whose raw lock is a type parameter of the enclosing struct (e.g.
///   `KMutex<L>` where `L: RawLock`), emitting both non-IRQ-safe and IRQ-safe
///   `LockClassRegistration` statics and selecting between them at compile time based on
///   `<L as RawLock>::LOCK_FLAGS`.
///
///   Note that `lock_<name>()`, `lock_<name>_aliased()`, `lock_<name>_multiple()`,
///   `lock_<name>_policy::<P>()`, `lock_<name>_aliased_policy::<P>()`, and
///   `lock_<name>_multiple_policy::<P>()` require `<P as LockPolicy>::AcquireArgs = ()`.
///   For generic lock types (e.g., `KMutex<L>`), `#[guarded]` additionally generates
///   `lock_<name>_with(args)`, `lock_<name>_aliased_with(args)`,
///   `lock_<name>_multiple_with(args)`, `lock_<name>_policy_with::<P>(args)`,
///   `lock_<name>_aliased_policy_with::<P>(args)`, and
///   `lock_<name>_multiple_policy_with::<P>(items, args)` accepting `AcquireArgs` (useful for
///   generic lock types where `AcquireArgs` might not be `()`). Conversely, concrete
///   monitored spinlock fields do not generate `*_with` methods; instead, their base
///   `lock_<name>` methods are generated to directly accept a `SourceTag` argument.
/// - `#[mutex(untracked)]`: Marks a `KMutex` field whose generated lock class is not registered
///   with lockdep (`ID` is null); cannot be combined with `flags` or `CustomClass`. Fields of an
///   untracked storage type (e.g. the kernel's `KMutex<Mutex>`) are untracked implicitly.
/// - `#[brwlock]` / `#[brwlock(flags = ...)]` / `#[brwlock(CustomClass)]`: Marks a `BrwLockPi<L>`
///   field, where `L` is the raw reader-writer lock type (e.g. the kernel's `RawBrwLockPi`; there
///   is no default), and generates `read_<name>()` / `write_<name>()` / `guard_read_<name>()` /
///   `guard_write_<name>()` methods, with the same lock class rules as `#[mutex]` (except that
///   `untracked` is not supported).
/// - `#[guarded_by(lock1)]` or `#[guarded_by(lock1, lock2, ...)]`: Wraps a field in `KCell` (or
///   parameterizes a `KOnceCell<T>` field) with the lock class (or tuple of lock classes) of the
///   specified lock(s).
///   - **Single-lock (`#[guarded_by(mu1)]`)**: Holding `mu1` grants both shared (`&T`) and
///     exclusive (`&mut T`) access via `guard.field()`, `guard.as_mut().field_mut()`, and
///     `guard.as_mut().fields_mut().field`.
///   - **Multi-lock (`#[guarded_by(mu1, mu2, ...)]`)**: Holding *any* one of the guarding locks
///     grants shared read access (`&T`) via `guard.field()`, `guard.fields().field`, and
///     `guard.as_mut().fields_mut().field` (which projects the field as `&T` rather than `&mut T`).
///     Mutating the field requires holding *all* guarding locks simultaneously and passing the
///     remaining lock guards or tokens to `guard.as_mut().field_mut(&mut other_guard, ...)`.
///
/// # Generated Code Example (Multi-Lock Field)
///
/// Given the following input struct:
///
/// ```ignore
/// #[guarded]
/// struct Example {
///     #[mutex]
///     mu1: KMutex<RawMutex>,
///     #[mutex]
///     mu2: KMutex<RawMutex>,
///     #[guarded_by(mu1)]
///     single: u32,
///     #[guarded_by(mu1, mu2)]
///     shared: u64,
/// }
/// ```
///
/// `#[guarded]` expands to the following (simplified for `mu1`; `mu2` is generated symmetrically):
///
/// ```ignore
/// #[derive(Default, Debug, Clone, Copy)]
/// struct ExampleMu1Class;
/// #[derive(Default, Debug, Clone, Copy)]
/// struct ExampleMu2Class;
///
/// #[::pin_init::pin_data]
/// struct Example {
///     #[pin]
///     mu1: KMutex<ExampleMu1Class, RawMutex>,
///     #[pin]
///     mu2: KMutex<ExampleMu2Class, RawMutex>,
///     single: ::ksync::KCell<u32, ExampleMu1Class>,
///     shared: ::ksync::KCell<u64, (ExampleMu1Class, ExampleMu2Class)>,
/// }
///
/// // Read-only projection (`g1.fields()`): all fields guarded by `mu1` are `&'b T`.
/// struct ExampleMu1Fields<'b> {
///     single: &'b u32,
///     shared: &'b u64,
///     _marker: ::core::marker::PhantomData<(&'b (),)>,
/// }
///
/// // Mutable projection (`g1.as_mut().fields_mut()`):
/// // - Single-lock `single` is projected mutably (`&'b mut u32`).
/// // - Multi-lock `shared` is projected immutably (`&'b u64`) because holding only `mu1`
/// //   only suffices to read `shared`.
/// struct ExampleMu1FieldsMut<'b> {
///     single: &'b mut u32,
///     shared: &'b u64,
///     _marker: ::core::marker::PhantomData<(&'b (),)>,
/// }
///
/// impl<'a, P: ::ksync::ExclusiveLockPolicy<RawMutex>> ExampleMu1Guard<'a, P> {
///     fn single(&self) -> &u32 {
///         unsafe { self.parent.single.get(self.inner.token()) }
///     }
///
///     fn single_mut<'f>(self: ::core::pin::Pin<&'f mut Self>) -> &'f mut u32 {
///         let me = unsafe { self.get_unchecked_mut() };
///         let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
///         let self_token = inner_pin.token_mut();
///         let token = self_token;
///         unsafe { me.parent.single.get_mut(token) }
///     }
///
///     // Holding `mu1` alone is sufficient to read `shared`:
///     fn shared(&self) -> &u64 {
///         unsafe { self.parent.shared.get(self.inner.token()) }
///     }
///
///     // Mutating `shared` requires `self` (`mu1`) plus an exclusive token/guard for `mu2`:
///     fn shared_mut<'f>(
///         self: ::core::pin::Pin<&'f mut Self>,
///         mu2_token: &'f mut impl ::ksync::ExclusiveLockToken<Example, ExampleMu2Class>,
///     ) -> &'f mut u64 {
///         let me = unsafe { self.get_unchecked_mut() };
///         let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
///         let self_token = inner_pin.token_mut();
///         let token = {
///             // Runtime check that `mu2_token` belongs to the same `Example` instance
///             // (`me.parent`). When both guards are locked on the same `Example` reference in
///             // an inlined scope, the compiler constant-folds this pointer comparison away.
///             assert!(
///                 ::core::ptr::eq(
///                     ::core::ptr::from_ref(me.parent),
///                     ::ksync::ExclusiveLockToken::parent(mu2_token),
///                 ),
///                 "Lock guard belongs to a different struct instance",
///             );
///             self_token.join(::ksync::ExclusiveLockToken::lock_token_mut(mu2_token))
///         };
///         unsafe { me.parent.shared.get_mut(token) }
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn guarded(_args: TokenStream, input: TokenStream) -> TokenStream {
    guarded_impl(input.into()).into()
}

fn guarded_impl(input: proc_macro2::TokenStream) -> proc_macro2::TokenStream {
    let mut input_struct: ItemStruct = match syn::parse2(input) {
        Ok(s) => s,
        Err(e) => return e.to_compile_error(),
    };

    let mut mutex_fields = Vec::new();
    let mut brwlock_fields = Vec::new();
    let mut guarded_fields = Vec::new();
    let mut errors = Vec::new();

    if let Fields::Named(ref mut fields) = input_struct.fields {
        for field in fields.named.iter_mut() {
            let analysis = parse_field_attributes(field, &mut errors);

            if analysis.is_unpinned && !is_wavltree_type(&field.ty) {
                errors.push(syn::Error::new(
                    field.ty.span(),
                    "The #[ksync(unpinned)] attribute is only allowed on WavlTree fields",
                ));
            }

            if (analysis.is_mutex || analysis.is_brwlock) && !analysis.guarded_by.is_empty() {
                errors.push(syn::Error::new(
                    field.ident.as_ref().unwrap().span(),
                    "Lock fields cannot also have #[guarded_by] attributes",
                ));
            }

            if analysis.guarded_by.len() > 3 {
                errors.push(syn::Error::new(
                    field.ident.as_ref().unwrap().span(),
                    "#[guarded_by(...)] supports at most 3 locks",
                ));
            }

            if analysis.is_mutex {
                // There is no default raw lock type: `ksync` cannot name the kernel's
                // `TrackedMutex`, so the lock type must always be given explicitly (e.g.
                // `KMutex<TrackedMutex>` in the kernel, or `KMutex<RawMutex>` outside it).
                let mut mutex_type = quote! { () };
                if !is_kmutex_type(&field.ty) {
                    errors.push(syn::Error::new(
                        field.ty.span(),
                        "Mutex field must be of type KMutex (e.g. use \
                         `KMutex<TrackedSpinlock>` rather than `KSpinlock`)",
                    ));
                } else {
                    match extract_lock_type(&field.ty) {
                        Ok(Some(ty)) => mutex_type = ty,
                        Ok(None) => errors.push(syn::Error::new(
                            field.ty.span(),
                            "KMutex fields must specify the raw lock type explicitly, e.g. \
                             `KMutex<TrackedMutex>` (kernel) or `KMutex<RawMutex>` (host)",
                        )),
                        Err(e) => errors.push(e),
                    }
                }
                field.attrs.push(syn::parse_quote!(#[pin]));
                let is_untracked = analysis.is_untracked || is_untracked_storage_type(&mutex_type);
                if is_untracked && analysis.custom_flags.is_some() {
                    errors.push(syn::Error::new(
                        field.ident.as_ref().unwrap().span(),
                        "untracked mutexes cannot specify lockdep flags",
                    ));
                }
                mutex_fields.push((
                    field.clone(),
                    mutex_type,
                    analysis.custom_class,
                    analysis.custom_flags,
                    is_untracked,
                ));
            } else if analysis.is_brwlock {
                // As with `KMutex`, there is no default raw lock type: `ksync` cannot name the
                // kernel's `RawBrwLockPi`, so it must be given explicitly.
                let mut raw_lock_type = quote! { () };
                if !is_brwlock_type(&field.ty) {
                    errors.push(syn::Error::new(
                        field.ty.span(),
                        "Brwlock field must be of type BrwLockPi",
                    ));
                } else {
                    match extract_brwlock_type(&field.ty) {
                        Ok(ty) => raw_lock_type = ty,
                        Err(e) => errors.push(e),
                    }
                }
                field.attrs.push(syn::parse_quote!(#[pin]));
                brwlock_fields.push((
                    field.clone(),
                    raw_lock_type,
                    analysis.custom_class,
                    analysis.custom_flags,
                ));
            } else if !analysis.guarded_by.is_empty() {
                let konce_cell_inner_ty = if is_konce_cell_type(&field.ty) {
                    match extract_konce_cell_inner_type(&field.ty) {
                        Some(ty) => Some(ty),
                        None => {
                            errors.push(syn::Error::new(
                                field.ty.span(),
                                "KOnceCell field must specify its value type, e.g., KOnceCell<T>",
                            ));
                            None
                        }
                    }
                } else {
                    None
                };
                let is_konce_cell = konce_cell_inner_ty.is_some();

                if is_konce_cell && analysis.is_pinned {
                    errors.push(syn::Error::new(
                        field.ident.as_ref().unwrap().span(),
                        "KOnceCell fields cannot be structurally pinned with #[pin]",
                    ));
                }

                guarded_fields.push(GuardedField {
                    ident: field.ident.clone().unwrap(),
                    mutex_idents: analysis.guarded_by,
                    ty: field.ty.clone(),
                    vis: field.vis.clone(),
                    project_as_pin: analysis.is_pinned && !analysis.is_unpinned,
                    is_konce_cell,
                    konce_cell_inner_ty,
                });
            }
        }
    }

    if !errors.is_empty() {
        let compile_errors = errors.iter().map(|e| e.to_compile_error());
        return quote! { #(#compile_errors)* };
    }

    // Automatically apply #[::pin_init::pin_data] if not already present
    let has_pin_data = input_struct
        .attrs
        .iter()
        .any(|attr| attr.path().segments.iter().any(|seg| seg.ident == "pin_data"));
    if !has_pin_data {
        input_struct.attrs.push(syn::parse_quote!(#[::pin_init::pin_data]));
    }

    let struct_ident = &input_struct.ident;
    let struct_vis = &input_struct.vis;

    let generics_info = extract_generics_info(&input_struct.generics);

    let mut mutex_fields_processed = Vec::new();
    let mut brwlock_fields_processed = Vec::new();
    let mut generated_names = std::collections::HashSet::new();

    for (field, mutex_type, custom_class, custom_flags, is_untracked) in mutex_fields {
        let field_ident = field.ident.clone().unwrap();
        let mu_camel = to_camel_case(&field_ident.to_string());
        let guard_name = format!("{struct_ident}{mu_camel}Guard");

        check_unique_name(
            &guard_name,
            field_ident.span(),
            &field_ident,
            "guard",
            &mut generated_names,
            &mut errors,
        );

        let (class_ident, class_type) = resolve_lock_class(
            custom_class.as_ref(),
            struct_ident,
            &mu_camel,
            &field_ident,
            &generics_info,
            &mut generated_names,
            &mut errors,
        );

        let is_phantom = is_phantom_mutex_type(&field.ty);
        if is_phantom && custom_class.is_none() {
            errors.push(syn::Error::new(
                field_ident.span(),
                "PhantomMutex fields must specify an external lock class via #[mutex(LockClass)]",
            ));
        }

        mutex_fields_processed.push(MutexField {
            ident: field_ident,
            class_ident,
            class_type,
            mutex_type,
            custom_class,
            custom_flags,
            is_phantom,
            is_untracked,
        });
    }

    for (field, raw_lock_type, custom_class, custom_flags) in brwlock_fields {
        let field_ident = field.ident.unwrap();
        let mu_camel = to_camel_case(&field_ident.to_string());

        let read_guard_name = format!("{struct_ident}{mu_camel}ReadGuard");
        let write_guard_name = format!("{struct_ident}{mu_camel}WriteGuard");

        check_unique_name(
            &read_guard_name,
            field_ident.span(),
            &field_ident,
            "guard",
            &mut generated_names,
            &mut errors,
        );
        check_unique_name(
            &write_guard_name,
            field_ident.span(),
            &field_ident,
            "guard",
            &mut generated_names,
            &mut errors,
        );

        let (class_ident, class_type) = resolve_lock_class(
            custom_class.as_ref(),
            struct_ident,
            &mu_camel,
            &field_ident,
            &generics_info,
            &mut generated_names,
            &mut errors,
        );

        brwlock_fields_processed.push(MutexField {
            ident: field_ident,
            class_ident,
            class_type,
            mutex_type: raw_lock_type,
            custom_class,
            custom_flags,
            is_phantom: false,
            // `untracked` is rejected on `#[brwlock]`; brwlock classes are always registered.
            is_untracked: false,
        });
    }

    if !errors.is_empty() {
        let compile_errors = errors.iter().map(|e| e.to_compile_error());
        return quote! { #(#compile_errors)* };
    }

    // Rewrite fields in struct to KMutex / BrwLockPi and KCell
    if let Fields::Named(ref mut fields) = input_struct.fields {
        for field in fields.named.iter_mut() {
            let field_ident = field.ident.as_ref().unwrap();

            if let Some(mutex_field) =
                mutex_fields_processed.iter().find(|m| m.ident == *field_ident)
            {
                let class_type = &mutex_field.class_type;
                let mutex_type = &mutex_field.mutex_type;
                if let Type::Path(ref mut type_path) = field.ty
                    && let Some(last_segment) = type_path.path.segments.last_mut()
                {
                    last_segment.ident = format_ident!("KMutex");
                    last_segment.arguments = syn::PathArguments::AngleBracketed(
                        syn::parse2(quote! { <#class_type, #mutex_type> }).unwrap(),
                    );
                }
            } else if let Some(brwlock_field) =
                brwlock_fields_processed.iter().find(|m| m.ident == *field_ident)
            {
                let class_type = &brwlock_field.class_type;
                let raw_lock_type = &brwlock_field.mutex_type;
                if let Type::Path(ref mut type_path) = field.ty
                    && let Some(last_segment) = type_path.path.segments.last_mut()
                {
                    last_segment.ident = format_ident!("BrwLockPi");
                    last_segment.arguments = syn::PathArguments::AngleBracketed(
                        syn::parse2(quote! { <#class_type, #raw_lock_type> }).unwrap(),
                    );
                }
            } else if let Some(guarded_field) =
                guarded_fields.iter().find(|f| f.ident == *field_ident)
            {
                let mut class_types = Vec::new();
                for mutex_ident in &guarded_field.mutex_idents {
                    let class_type_opt = mutex_fields_processed
                        .iter()
                        .chain(brwlock_fields_processed.iter())
                        .find(|m| m.ident == *mutex_ident)
                        .map(|m| &m.class_type);
                    if let Some(ct) = class_type_opt {
                        class_types.push(ct);
                    } else {
                        errors.push(syn::Error::new(
                            mutex_ident.span(),
                            format!(
                                "Guarded field '{}' references non-existent lock '{}'",
                                field_ident, mutex_ident
                            ),
                        ));
                    }
                }

                if class_types.len() == guarded_field.mutex_idents.len() {
                    let cell_class_type = if class_types.len() == 1 {
                        let ct = class_types[0];
                        quote! { #ct }
                    } else {
                        quote! { (#(#class_types),*) }
                    };

                    if guarded_field.is_konce_cell {
                        let inner_ty = guarded_field.konce_cell_inner_ty.as_ref().unwrap();
                        field.ty =
                            syn::parse2(quote! { ::ksync::KOnceCell<#inner_ty, #cell_class_type> })
                                .unwrap();
                    } else {
                        let original_ty = &field.ty;
                        field.ty =
                            syn::parse2(quote! { ::ksync::KCell<#original_ty, #cell_class_type> })
                                .unwrap();
                    }
                    field.attrs.push(syn::parse_quote!(#[allow(dead_code)]));
                }
            }
        }
    }

    if !errors.is_empty() {
        let compile_errors = errors.iter().map(|e| e.to_compile_error());
        return quote! { #(#compile_errors)* };
    }

    let all_locks: Vec<&MutexField> =
        mutex_fields_processed.iter().chain(brwlock_fields_processed.iter()).collect();

    let (impl_generics, ty_generics, where_clause) = input_struct.generics.split_for_impl();

    let params_with_bounds = &generics_info.params_no_defaults;
    let ty_params = &generics_info.ty_params;
    let phantom_ty_params = &generics_info.phantom_ty_params;
    let phantom_lifetimes = &generics_info.phantom_lifetimes;

    let fields_decl_generics = quote! { <'b, #params_with_bounds> };
    let fields_ty_generics = quote! { <'b, #(#ty_params),*> };
    let fields_ret_ty_generics = quote! { <'f, #(#ty_params),*> };
    let token_guard_decl_generics = quote! { <'b, 'a, #params_with_bounds> };
    let token_guard_impl_generics = quote! { <'b, 'a, #params_with_bounds> };
    let token_guard_ty_generics = quote! { <'b, 'a, #(#ty_params),*> };

    let (class_decl_generics, class_impl_generics, class_ty_generics) =
        if generics_info.class_ty_params.is_empty() {
            (quote! {}, quote! {}, quote! {})
        } else {
            let class_params_no_defaults = &generics_info.class_params_no_defaults;
            let class_ty_params = &generics_info.class_ty_params;
            (
                quote! { <#class_params_no_defaults> },
                quote! { <#class_params_no_defaults> },
                quote! { <#(#class_ty_params),*> },
            )
        };

    let marker_phantom = quote! {
        ::core::marker::PhantomData<(
            #(& #phantom_lifetimes (),)*
            fn() -> *const (#(#phantom_ty_params),*),
        )>
    };

    // Marker structs for custom classes
    let mut marker_structs = quote! {};
    for lock in mutex_fields_processed.iter().chain(brwlock_fields_processed.iter()) {
        if lock.custom_class.is_none() {
            let class_ident = &lock.class_ident;
            if input_struct.generics.params.is_empty() {
                marker_structs.extend(quote! {
                    #[allow(non_camel_case_types)]
                    #[derive(Default, Debug, Clone, Copy)]
                    #struct_vis struct #class_ident;
                });
            } else {
                marker_structs.extend(quote! {
                    #[allow(non_camel_case_types)]
                    #[derive(Default, Debug, Clone, Copy)]
                    #struct_vis struct #class_ident #class_decl_generics (
                        pub #marker_phantom
                    );
                });
            }
        }
    }

    let mut generated_code = quote! {};

    for mutex in &mutex_fields_processed {
        let mu_ident = &mutex.ident;
        let class_ident = &mutex.class_ident;
        let class_type = &mutex.class_type;
        let mutex_type = &mutex.mutex_type;

        let mu_camel = to_camel_case(&mu_ident.to_string());
        let guard_ident = format_ident!("{struct_ident}{mu_camel}Guard");
        let fields_ident = format_ident!("{struct_ident}{mu_camel}Fields");
        let fields_mut_ident = format_ident!("{struct_ident}{mu_camel}FieldsMut");
        let token_guard_ident = format_ident!("{struct_ident}{mu_camel}TokenGuard");
        let token_guard_mut_ident = format_ident!("{struct_ident}{mu_camel}TokenGuardMut");

        let lock_method_ident = format_ident!("lock_{mu_ident}", span = mu_ident.span());
        let lock_policy_method_ident =
            format_ident!("lock_{mu_ident}_policy", span = mu_ident.span());
        let guard_method_ident = format_ident!("guard_{mu_ident}", span = mu_ident.span());
        let guard_mut_method_ident = format_ident!("guard_{mu_ident}_mut", span = mu_ident.span());

        let this_guarded_fields: Vec<&GuardedField> =
            guarded_fields.iter().filter(|f| f.mutex_idents.contains(mu_ident)).collect();

        let parent_ty = quote! { #struct_ident #ty_generics };
        let field_code =
            generate_field_code(&this_guarded_fields, mu_ident, class_type, &all_locks, &parent_ty);

        let fields_struct = generate_fields_struct(
            struct_vis,
            &fields_ident,
            &fields_decl_generics,
            where_clause,
            &field_code.read_fields_decl,
            phantom_lifetimes,
            phantom_ty_params,
        );

        let fields_mut_struct = generate_fields_struct(
            struct_vis,
            &fields_mut_ident,
            &fields_decl_generics,
            where_clause,
            &field_code.write_fields_decl,
            phantom_lifetimes,
            phantom_ty_params,
        );

        let is_generic_mutex_type =
            mentions_any_ident(mutex_type, &generics_info.all_generic_idents);

        let class_registration_code = if mutex.custom_class.is_none() {
            if mutex.is_untracked {
                quote! {
                    impl #class_impl_generics ::ksync::LockClass
                        for #class_ident #class_ty_generics #where_clause
                    {
                        const ID: *mut ::core::ffi::c_void = ::core::ptr::null_mut();
                        const FLAGS: ::ksync::LockFlags = ::ksync::LOCK_FLAGS_NONE;
                    }
                }
            } else if is_generic_mutex_type {
                generate_generic_lock_class_registration(
                    struct_ident,
                    mu_ident,
                    &mu_camel,
                    class_ident,
                    &class_impl_generics,
                    &class_ty_generics,
                    where_clause,
                    mutex_type,
                    mutex.custom_flags.as_ref(),
                )
            } else {
                let flags_expr = match mutex.custom_flags.as_ref() {
                    Some(flags) => {
                        quote! { (<#mutex_type as ::ksync::RawLock>::LOCK_FLAGS | (#flags)) }
                    }
                    None => quote! { <#mutex_type as ::ksync::RawLock>::LOCK_FLAGS },
                };
                generate_lock_class_registration(
                    struct_ident,
                    mu_ident,
                    &mu_camel,
                    class_ident,
                    &class_impl_generics,
                    &class_ty_generics,
                    where_clause,
                    flags_expr,
                )
            }
        } else {
            quote! {}
        };

        let guard_struct_generics = if mutex.is_phantom {
            let mut params = params_with_bounds.clone();
            params.push(syn::parse_quote!(M: ::ksync::RawLock));
            params.push(syn::parse_quote!(
                P: ::ksync::ExclusiveLockPolicy<M> = <M as ::ksync::RawLock>::DefaultPolicy
            ));
            quote! { <'a, #params> }
        } else {
            let mut params = params_with_bounds.clone();
            params.push(syn::parse_quote!(
                P: ::ksync::ExclusiveLockPolicy<#mutex_type> =
                    <#mutex_type as ::ksync::RawLock>::DefaultPolicy
            ));

            quote! { <'a, #params>  }
        };
        let guard_impl_generics = if mutex.is_phantom {
            let mut params = params_with_bounds.clone();
            params.push(syn::parse_quote!(M: ::ksync::RawLock));
            params.push(syn::parse_quote!(P: ::ksync::ExclusiveLockPolicy<M>));
            quote! { <'a, #params> }
        } else {
            let mut params = params_with_bounds.clone();
            params.push(syn::parse_quote!(P: ::ksync::ExclusiveLockPolicy<#mutex_type>));
            quote! { <'a, #params> }
        };
        let guard_ty_generics = if mutex.is_phantom {
            let mut args = ty_params.clone();
            args.push(quote! { M });
            args.push(quote! { P });
            quote! { <'a, #(#args),*> }
        } else {
            let mut args = ty_params.clone();
            args.push(quote! { P });
            quote! { <'a, #(#args),*> }
        };
        let guard_inner_type = if mutex.is_phantom {
            quote! { ::ksync::KMutexGuard<'a, #class_type, M, P> }
        } else {
            quote! { ::ksync::KMutexGuard<'a, #class_type, #mutex_type, P> }
        };

        let lock_with_method_ident = format_ident!("lock_{mu_ident}_with", span = mu_ident.span());
        let lock_policy_with_method_ident =
            format_ident!("lock_{mu_ident}_policy_with", span = mu_ident.span());
        let lock_aliased_method_ident =
            format_ident!("lock_{mu_ident}_aliased", span = mu_ident.span());
        let lock_aliased_with_method_ident =
            format_ident!("lock_{mu_ident}_aliased_with", span = mu_ident.span());
        let lock_aliased_policy_method_ident =
            format_ident!("lock_{mu_ident}_aliased_policy", span = mu_ident.span());
        let _lock_aliased_policy_with_method_ident =
            format_ident!("lock_{mu_ident}_aliased_policy_with", span = mu_ident.span());
        let lock_multiple_method_ident =
            format_ident!("lock_{mu_ident}_multiple", span = mu_ident.span());
        let lock_multiple_with_method_ident =
            format_ident!("lock_{mu_ident}_multiple_with", span = mu_ident.span());
        let lock_multiple_policy_method_ident =
            format_ident!("lock_{mu_ident}_multiple_policy", span = mu_ident.span());
        let lock_multiple_policy_with_method_ident =
            format_ident!("lock_{mu_ident}_multiple_policy_with", span = mu_ident.span());

        let lock_method_def = if mutex.is_phantom {
            let mut return_args = ty_params.clone();
            return_args.push(quote! { M });
            let return_ty_generics = quote! { <'a, #(#return_args),*> };
            let mut policy_args = ty_params.clone();
            policy_args.push(quote! { M });
            policy_args.push(quote! { P });
            let policy_return_ty_generics = quote! { <'a, #(#policy_args),*> };
            quote! {
                /// Acquires `real_mutex` and returns a guard for accessing fields of `self`
                /// protected by this phantom
                /// mutex.
                ///
                /// # Safety
                ///
                /// The caller must ensure that `real_mutex` is the specific physical lock instance
                /// that synchronizes `self`.
                #[inline]
                #struct_vis unsafe fn #lock_method_ident<'a, M: ::ksync::RawLock>(
                    &'a self,
                    real_mutex: &'a ::ksync::KMutex<#class_type, M>,
                ) -> impl pin_init::PinInit<
                    #guard_ident #return_ty_generics,
                    ::core::convert::Infallible,
                >
                where
                    <M as ::ksync::RawLock>::DefaultPolicy:
                        ::ksync::ExclusiveLockPolicy<M, AcquireArgs = ()>,

                {
                    pin_init::pin_init!(#guard_ident {
                        parent: self,
                        inner <- ::ksync::KMutexGuard::new(real_mutex),
                    })
                }

                /// Acquires `real_mutex` using the default policy and provided arguments,
                /// returning a guard for accessing fields of `self` protected by this phantom
                /// mutex.
                ///
                /// # Safety
                ///
                /// The caller must ensure that `real_mutex` is the specific physical lock instance
                /// that synchronizes `self`.
                #[inline]
                #struct_vis unsafe fn #lock_with_method_ident<'a, M: ::ksync::RawLock>(
                    &'a self,
                    real_mutex: &'a ::ksync::KMutex<#class_type, M>,
                    args: <<M as ::ksync::RawLock>::DefaultPolicy as ::ksync::LockPolicy<M>>::
                        AcquireArgs,
                ) -> impl pin_init::PinInit<
                    #guard_ident #return_ty_generics,
                    ::core::convert::Infallible,
                >
                where
                    <M as ::ksync::RawLock>::DefaultPolicy:
                        ::ksync::ExclusiveLockPolicy<M>,
                {
                    pin_init::pin_init!(#guard_ident {
                        parent: self,
                        inner <- ::ksync::KMutexGuard::new_with_args(real_mutex, args),
                    })
                }

                /// Acquires `real_mutex` using policy `P` and provided arguments,
                /// returning a guard for accessing fields of `self` protected by this phantom
                /// mutex.
                ///
                /// # Safety
                ///
                /// The caller must ensure that `real_mutex` is the specific physical lock instance
                /// that synchronizes `self`.
                #[inline]
                #struct_vis unsafe fn #lock_policy_with_method_ident<
                    'a,
                    M: ::ksync::RawLock,
                    P: ::ksync::ExclusiveLockPolicy<M>,
                >(
                    &'a self,
                    real_mutex: &'a ::ksync::KMutex<#class_type, M>,
                    args: <P as ::ksync::LockPolicy<M>>::AcquireArgs,
                ) -> impl pin_init::PinInit<
                    #guard_ident #policy_return_ty_generics,
                    ::core::convert::Infallible,
                >
                {
                    pin_init::pin_init!(#guard_ident {
                        parent: self,
                        inner <- ::ksync::KMutexGuard::new_with_args(real_mutex, args),
                    })
                }
            }
        } else if is_monitored_spinlock_type(mutex_type) {
            let return_ty_generics = quote! { <'_, #(#ty_params),*> };
            let mut policy_args = ty_params.clone();
            policy_args.push(quote! { P });
            let policy_return_ty_generics = quote! { <'_, #(#policy_args),*> };
            quote! {
                #[inline]
                #struct_vis fn #lock_method_ident(
                    &self,
                    tag: ::ksync::SourceTag,
                ) -> impl pin_init::PinInit<
                    #guard_ident #return_ty_generics,
                    ::core::convert::Infallible,
                > {
                    pin_init::pin_init!(#guard_ident {
                        parent: self,
                        inner <- ::ksync::KMutexGuard::new_with_args(&self.#mu_ident, tag),
                    })
                }
                #[inline]
                #struct_vis fn #lock_policy_method_ident<
                    P: ::ksync::ExclusiveLockPolicy<#mutex_type>,
                >(
                    &self,
                    args: P::AcquireArgs,
                ) -> impl pin_init::PinInit<
                    #guard_ident #policy_return_ty_generics,
                    ::core::convert::Infallible,
                > {
                    pin_init::pin_init!(#guard_ident {
                        parent: self,
                        inner <- ::ksync::KMutexGuard::new_with_args(&self.#mu_ident, args),
                    })
                }
                #[inline]
                #struct_vis fn #lock_aliased_method_ident<
                    'a,
                    AliasClass: ::ksync::LockClass,
                    M2: ::ksync::RawLock,
                >(
                    &'a self,
                    alias: &'a ::ksync::KMutex<AliasClass, M2>,
                    tag: ::ksync::SourceTag,
                ) -> impl pin_init::PinInit<
                    ::ksync::KMutexAliasedGuard<'a, #class_type, AliasClass, #mutex_type, M2>,
                    ::core::convert::Infallible,
                > {
                    ::ksync::KMutexAliasedGuard::new_with_args(&self.#mu_ident, alias, tag)
                }
                #[inline]
                #struct_vis fn #lock_aliased_policy_method_ident<
                    'a,
                    AliasClass: ::ksync::LockClass,
                    M2: ::ksync::RawLock,
                    P: ::ksync::ExclusiveLockPolicy<#mutex_type>,
                >(
                    &'a self,
                    alias: &'a ::ksync::KMutex<AliasClass, M2>,
                    args: P::AcquireArgs,
                ) -> impl pin_init::PinInit<
                    ::ksync::KMutexAliasedGuard<'a, #class_type, AliasClass, #mutex_type, M2, P>,
                    ::core::convert::Infallible,
                > {
                    ::ksync::KMutexAliasedGuard::new_with_args(&self.#mu_ident, alias, args)
                }
                #[inline]
                #struct_vis fn #lock_multiple_method_ident<'a, const __KSYNC_N: usize>(
                    items: [&'a Self; __KSYNC_N],
                    tag: ::ksync::SourceTag,
                ) -> impl pin_init::PinInit<
                    ::ksync::KMutexGuardMultiple<'a, __KSYNC_N, #class_type, #mutex_type>,
                    ::core::convert::Infallible,
                > {
                    ::ksync::KMutexGuardMultiple::new_with_args(
                        ::core::array::from_fn(|i| &items[i].#mu_ident),
                        tag,
                    )
                }
                #[inline]
                #struct_vis fn #lock_multiple_policy_method_ident<
                    'a,
                    const __KSYNC_N: usize,
                    P: ::ksync::ExclusiveLockPolicy<#mutex_type>,
                >(
                    items: [&'a Self; __KSYNC_N],
                    args: P::AcquireArgs,
                ) -> impl pin_init::PinInit<
                    ::ksync::KMutexGuardMultiple<'a, __KSYNC_N, #class_type, #mutex_type, P>,
                    ::core::convert::Infallible,
                > {
                    ::ksync::KMutexGuardMultiple::new_with_args(
                        ::core::array::from_fn(|i| &items[i].#mu_ident),
                        args,
                    )
                }
            }
        } else {
            let lock_policy_with_method_ident =
                format_ident!("lock_{mu_ident}_policy_with", span = mu_ident.span());
            let lock_aliased_policy_with_method_ident =
                format_ident!("lock_{mu_ident}_aliased_policy_with", span = mu_ident.span());
            let return_ty_generics = quote! { <'_, #(#ty_params),*> };
            let mut policy_args = ty_params.clone();
            policy_args.push(quote! { P });
            let policy_return_ty_generics = quote! { <'_, #(#policy_args),*> };
            quote! {
                #[inline]
                #struct_vis fn #lock_method_ident(
                    &self,
                ) -> impl pin_init::PinInit<
                    #guard_ident #return_ty_generics,
                    ::core::convert::Infallible,
                >
                where
                    <#mutex_type as ::ksync::RawLock>::DefaultPolicy:
                        ::ksync::ExclusiveLockPolicy<#mutex_type, AcquireArgs = ()>,
                {
                    pin_init::pin_init!(#guard_ident {
                        parent: self,
                        inner <- ::ksync::KMutexGuard::new(&self.#mu_ident),
                    })
                }
                #[inline]
                #struct_vis fn #lock_with_method_ident(
                    &self,
                    args: <<#mutex_type as ::ksync::RawLock>::DefaultPolicy as ::ksync::LockPolicy<
                        #mutex_type,
                    >>::AcquireArgs,
                ) -> impl pin_init::PinInit<
                    #guard_ident #return_ty_generics,
                    ::core::convert::Infallible,
                > {
                    pin_init::pin_init!(#guard_ident {
                        parent: self,
                        inner <- ::ksync::KMutexGuard::new_with_args(&self.#mu_ident, args),
                    })
                }
                #[inline]
                #struct_vis fn #lock_policy_method_ident<
                    P: ::ksync::ExclusiveLockPolicy<#mutex_type, AcquireArgs = ()>,
                >(
                    &self,
                ) -> impl pin_init::PinInit<
                    #guard_ident #policy_return_ty_generics,
                    ::core::convert::Infallible,
                > {

                    pin_init::pin_init!(#guard_ident {
                        parent: self,
                        inner <- ::ksync::KMutexGuard::new(&self.#mu_ident),
                    })
                }
                #[inline]
                #struct_vis fn #lock_policy_with_method_ident<
                    P: ::ksync::ExclusiveLockPolicy<#mutex_type>,
                >(
                    &self,
                    args: P::AcquireArgs,
                ) -> impl pin_init::PinInit<
                    #guard_ident #policy_return_ty_generics,
                    ::core::convert::Infallible,
                > {
                    pin_init::pin_init!(#guard_ident {
                        parent: self,
                        inner <- ::ksync::KMutexGuard::new_with_args(&self.#mu_ident, args),
                    })
                }
                #[inline]
                #struct_vis fn #lock_aliased_method_ident<
                    'a,
                    AliasClass: ::ksync::LockClass,
                    M2: ::ksync::RawLock,
                >(
                    &'a self,
                    alias: &'a ::ksync::KMutex<AliasClass, M2>,
                ) -> impl pin_init::PinInit<
                    ::ksync::KMutexAliasedGuard<'a, #class_type, AliasClass, #mutex_type, M2>,
                    ::core::convert::Infallible,
                >
                where
                    <#mutex_type as ::ksync::RawLock>::DefaultPolicy:
                        ::ksync::ExclusiveLockPolicy<#mutex_type, AcquireArgs = ()>,
                {
                    ::ksync::KMutexAliasedGuard::new(&self.#mu_ident, alias)
                }
                #[inline]
                #struct_vis fn #lock_aliased_with_method_ident<
                    'a,
                    AliasClass: ::ksync::LockClass,
                    M2: ::ksync::RawLock,
                >(
                    &'a self,
                    alias: &'a ::ksync::KMutex<AliasClass, M2>,
                    args: <<#mutex_type as ::ksync::RawLock>::DefaultPolicy as ::ksync::LockPolicy<
                        #mutex_type,
                    >>::AcquireArgs,
                ) -> impl pin_init::PinInit<
                    ::ksync::KMutexAliasedGuard<'a, #class_type, AliasClass, #mutex_type, M2>,
                    ::core::convert::Infallible,
                > {
                    ::ksync::KMutexAliasedGuard::new_with_args(&self.#mu_ident, alias, args)
                }
                #[inline]
                #struct_vis fn #lock_aliased_policy_method_ident<
                    'a,
                    AliasClass: ::ksync::LockClass,
                    M2: ::ksync::RawLock,
                    P: ::ksync::ExclusiveLockPolicy<#mutex_type, AcquireArgs = ()>,
                >(
                    &'a self,
                    alias: &'a ::ksync::KMutex<AliasClass, M2>,
                ) -> impl pin_init::PinInit<
                    ::ksync::KMutexAliasedGuard<'a, #class_type, AliasClass, #mutex_type, M2, P>,
                    ::core::convert::Infallible,
                > {
                    ::ksync::KMutexAliasedGuard::new(&self.#mu_ident, alias)
                }
                #[inline]
                #struct_vis fn #lock_aliased_policy_with_method_ident<
                    'a,
                    AliasClass: ::ksync::LockClass,
                    M2: ::ksync::RawLock,
                    P: ::ksync::ExclusiveLockPolicy<#mutex_type>,
                >(
                    &'a self,
                    alias: &'a ::ksync::KMutex<AliasClass, M2>,
                    args: P::AcquireArgs,
                ) -> impl pin_init::PinInit<
                    ::ksync::KMutexAliasedGuard<'a, #class_type, AliasClass, #mutex_type, M2, P>,
                    ::core::convert::Infallible,
                > {
                    ::ksync::KMutexAliasedGuard::new_with_args(&self.#mu_ident, alias, args)
                }
                #[inline]
                #struct_vis fn #lock_multiple_method_ident<'a, const __KSYNC_N: usize>(
                    items: [&'a Self; __KSYNC_N],
                ) -> impl pin_init::PinInit<
                    ::ksync::KMutexGuardMultiple<'a, __KSYNC_N, #class_type, #mutex_type>,
                    ::core::convert::Infallible,
                >
                where
                    <#mutex_type as ::ksync::RawLock>::DefaultPolicy:
                        ::ksync::ExclusiveLockPolicy<#mutex_type, AcquireArgs = ()>,
                {
                    ::ksync::KMutexGuardMultiple::new(
                        ::core::array::from_fn(|i| &items[i].#mu_ident),
                    )
                }
                #[inline]
                #struct_vis fn #lock_multiple_with_method_ident<'a, const __KSYNC_N: usize>(
                    items: [&'a Self; __KSYNC_N],
                    args: <<#mutex_type as ::ksync::RawLock>::DefaultPolicy as ::ksync::LockPolicy<
                        #mutex_type,
                    >>::AcquireArgs,
                ) -> impl pin_init::PinInit<
                    ::ksync::KMutexGuardMultiple<'a, __KSYNC_N, #class_type, #mutex_type>,
                    ::core::convert::Infallible,
                > {
                    ::ksync::KMutexGuardMultiple::new_with_args(
                        ::core::array::from_fn(|i| &items[i].#mu_ident),
                        args,
                    )
                }
                #[inline]
                #struct_vis fn #lock_multiple_policy_method_ident<
                    'a,
                    const __KSYNC_N: usize,
                    P: ::ksync::ExclusiveLockPolicy<#mutex_type, AcquireArgs = ()>,
                >(
                    items: [&'a Self; __KSYNC_N],
                ) -> impl pin_init::PinInit<
                    ::ksync::KMutexGuardMultiple<'a, __KSYNC_N, #class_type, #mutex_type, P>,
                    ::core::convert::Infallible,
                > {
                    ::ksync::KMutexGuardMultiple::new(
                        ::core::array::from_fn(|i| &items[i].#mu_ident),
                    )
                }
                #[inline]
                #struct_vis fn #lock_multiple_policy_with_method_ident<
                    'a,
                    const __KSYNC_N: usize,
                    P: ::ksync::ExclusiveLockPolicy<#mutex_type>,
                >(
                    items: [&'a Self; __KSYNC_N],
                    args: P::AcquireArgs,
                ) -> impl pin_init::PinInit<
                    ::ksync::KMutexGuardMultiple<'a, __KSYNC_N, #class_type, #mutex_type, P>,
                    ::core::convert::Infallible,
                > {
                    ::ksync::KMutexGuardMultiple::new_with_args(
                        ::core::array::from_fn(|i| &items[i].#mu_ident),
                        args,
                    )
                }
            }
        };

        let guard_from_guard_method_ident =
            format_ident!("guard_{mu_ident}_from_guard", span = mu_ident.span());
        let guard_mut_from_guard_method_ident =
            format_ident!("guard_{mu_ident}_mut_from_guard", span = mu_ident.span());

        let guard_from_guard_methods = if mutex.is_phantom {
            quote! {}
        } else {
            quote! {
                #[inline]
                #struct_vis fn #guard_from_guard_method_ident<
                    'b,
                    'a,
                    P: ::ksync::ExclusiveLockPolicy<#mutex_type>,
                >(
                    &'b self,
                    guard: &'b ::ksync::KMutexGuard<'a, #class_type, #mutex_type, P>,
                ) -> #token_guard_ident #token_guard_ty_generics {
                    assert!(
                        guard.is_locked_by(&self.#mu_ident),
                        "KMutexGuard does not belong to this struct's mutex"
                    );
                    #token_guard_ident { parent: self, token: guard.token() }
                }

                #[inline]
                #struct_vis fn #guard_mut_from_guard_method_ident<
                    'b,
                    'a,
                    P: ::ksync::ExclusiveLockPolicy<#mutex_type>,
                >(
                    &'b self,
                    guard: ::core::pin::Pin<
                        &'b mut ::ksync::KMutexGuard<'a, #class_type, #mutex_type, P>,
                    >,
                ) -> #token_guard_mut_ident #token_guard_ty_generics {
                    assert!(
                        guard.is_locked_by(&self.#mu_ident),
                        "KMutexGuard does not belong to this struct's mutex"
                    );
                    #token_guard_mut_ident { parent: self, token: guard.token_mut() }
                }
            }
        };

        let guard_method_def = quote! {
            /// Creates a read-only token guard for accessing fields protected by this mutex.
            ///
            /// # Safety
            ///
            /// The caller must ensure that `token` belongs to the specific lock instance that
            /// protects `self`.
            #[inline]
            #struct_vis unsafe fn #guard_method_ident<'b, 'a>(
                &'b self,
                token: &'b ::ksync::LockToken<'a, #class_type>,
            ) -> #token_guard_ident #token_guard_ty_generics {
                #token_guard_ident { parent: self, token }
            }

            /// Creates a mutable token guard for accessing fields protected by this mutex.
            ///
            /// # Safety
            ///
            /// The caller must ensure that `token` belongs to the specific lock instance that
            /// protects `self` and is not used to create aliasing references to `self`'s fields.
            #[inline]
            #struct_vis unsafe fn #guard_mut_method_ident<'b, 'a>(
                &'b self,
                token: &'b mut ::ksync::LockToken<'a, #class_type>,
            ) -> #token_guard_mut_ident #token_guard_ty_generics {
                #token_guard_mut_ident { parent: self, token }
            }

            #guard_from_guard_methods
        };

        let guard_accessors = &field_code.write_guard_accessors;
        let fields_init = &field_code.read_fields_init;
        let fields_mut_init = &field_code.write_fields_init;

        let token_guard = generate_token_guard(
            struct_vis,
            struct_ident,
            &ty_generics,
            where_clause,
            class_type,
            &token_guard_ident,
            &fields_ident,
            &token_guard_decl_generics,
            &token_guard_impl_generics,
            &token_guard_ty_generics,
            &fields_ret_ty_generics,
            &field_code.read_token_guard_accessors,
            &field_code.read_fields_init,
        );

        let token_guard_mut = generate_token_guard_mut(
            struct_vis,
            struct_ident,
            &ty_generics,
            where_clause,
            class_type,
            &token_guard_ident,
            &token_guard_mut_ident,
            &fields_ident,
            &fields_mut_ident,
            &token_guard_decl_generics,
            &token_guard_impl_generics,
            &token_guard_ty_generics,
            &fields_ret_ty_generics,
            ty_params,
            &field_code.write_token_guard_accessors,
            &field_code.read_fields_init,
            &field_code.write_fields_init,
        );

        let guard_struct = generate_pinned_guard_struct(
            struct_vis,
            struct_ident,
            &guard_ident,
            &guard_struct_generics,
            &guard_impl_generics,
            &guard_ty_generics,
            &ty_generics,
            where_clause,
            &guard_inner_type,
        );

        generated_code.extend(quote! {
            #fields_struct
            #fields_mut_struct
            #class_registration_code
            #guard_struct

            impl #guard_impl_generics #guard_ident #guard_ty_generics #where_clause {
                #guard_accessors

                #[inline]
                #struct_vis fn parent(&self) -> &#struct_ident #ty_generics {
                    self.parent
                }

                #[inline]
                #struct_vis fn as_token_guard<'b>(
                    &'b self,
                ) -> #token_guard_ident #token_guard_ty_generics {
                    #token_guard_ident {
                        parent: self.parent,
                        token: self.inner.token(),
                    }
                }

                #[inline]
                #struct_vis fn as_token_guard_mut<'b>(
                    self: ::core::pin::Pin<&'b mut Self>,
                ) -> #token_guard_mut_ident #token_guard_ty_generics {
                    // SAFETY: Projecting `inner` as pinned does not move any pinned fields.
                    let me = unsafe { self.get_unchecked_mut() };
                    // SAFETY: `inner` is structurally pinned.
                    let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
                    #token_guard_mut_ident {
                        parent: me.parent,
                        token: inner_pin.token_mut(),
                    }
                }

                #[inline]
                #struct_vis fn token(&self) -> &::ksync::LockToken<'a, #class_type> {
                    self.inner.token()
                }

                #[inline]
                #struct_vis fn token_mut(
                    self: ::core::pin::Pin<&mut Self>,
                ) -> &mut ::ksync::LockToken<'a, #class_type> {
                    // SAFETY: Projecting `inner` as pinned does not move any pinned fields.
                    let me = unsafe { self.get_unchecked_mut() };
                    // SAFETY: `inner` is structurally pinned.
                    let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
                    inner_pin.token_mut()
                }

                #[inline]
                #struct_vis fn release(self: ::core::pin::Pin<&mut Self>) {
                    // SAFETY: Projecting `inner` as pinned does not move any pinned fields.
                    let me = unsafe { self.get_unchecked_mut() };
                    // SAFETY: `inner` is structurally pinned.
                    let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
                    inner_pin.release();
                }

                #[inline]
                #struct_vis fn call_unlocked<R, F: FnOnce() -> R>(
                    self: ::core::pin::Pin<&mut Self>,
                    f: F,
                ) -> R {
                    // SAFETY: Projecting `inner` as pinned does not move any pinned fields.
                    let me = unsafe { self.get_unchecked_mut() };
                    // SAFETY: `inner` is structurally pinned.
                    let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
                    inner_pin.call_unlocked(f)
                }

                #[inline]
                #struct_vis fn call_untracked<
                    R,
                    F: FnOnce(&mut ::ksync::LockToken<'a, #class_type>) -> R,
                >(
                    self: ::core::pin::Pin<&mut Self>,
                    f: F,
                ) -> R {
                    // SAFETY: Projecting `inner` as pinned does not move any pinned fields.
                    let me = unsafe { self.get_unchecked_mut() };
                    // SAFETY: `inner` is structurally pinned.
                    let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
                    inner_pin.call_untracked(f)
                }

                #[inline]
                #[allow(unused_variables)]
                #struct_vis fn fields<'b>(&'b self) -> #fields_ident #fields_ty_generics {
                    let me = self;
                    let token = me.inner.token();
                    #fields_ident {
                        #fields_init
                        _marker: ::core::marker::PhantomData,
                    }
                }

                #[inline]
                #[allow(unused_variables)]
                #struct_vis fn fields_mut<'b>(
                    self: ::core::pin::Pin<&'b mut Self>,
                ) -> #fields_mut_ident #fields_ty_generics {
                    // SAFETY: Projecting `inner` as pinned does not move any pinned fields.
                    let me = unsafe { self.get_unchecked_mut() };
                    // SAFETY: `inner` is structurally pinned.
                    let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
                    let token = inner_pin.token_mut();
                    #fields_mut_ident {
                        #fields_mut_init
                        _marker: ::core::marker::PhantomData,
                    }
                }
            }

            // SAFETY: `#guard_ident` holds an exclusive lock guard for `self.parent`.
            unsafe impl #guard_impl_generics
                ::ksync::ExclusiveLockToken<#struct_ident #ty_generics, #class_type>
                for ::core::pin::Pin<&mut #guard_ident #guard_ty_generics> #where_clause
            {
                #[inline]
                fn parent(&self) -> *const #struct_ident #ty_generics {
                    ::core::ptr::from_ref(self.parent)
                }

                #[inline]
                fn lock_token_mut(&mut self) -> &mut ::ksync::LockToken<'_, #class_type> {
                    self.as_mut().token_mut().reborrow()
                }
            }

            #token_guard
            #token_guard_mut

            impl #impl_generics #struct_ident #ty_generics #where_clause {
                #lock_method_def
                #guard_method_def
            }
        });
    }

    for brwlock in &brwlock_fields_processed {
        let lock_ident = &brwlock.ident;
        let class_ident = &brwlock.class_ident;
        let class_type = &brwlock.class_type;
        let raw_lock_type = &brwlock.mutex_type;

        let mu_camel = to_camel_case(&lock_ident.to_string());
        let read_guard_ident = format_ident!("{struct_ident}{mu_camel}ReadGuard");
        let write_guard_ident = format_ident!("{struct_ident}{mu_camel}WriteGuard");
        let read_fields_ident = format_ident!("{struct_ident}{mu_camel}ReadFields");
        let write_fields_ident = format_ident!("{struct_ident}{mu_camel}WriteFields");

        let read_token_guard_ident = format_ident!("{struct_ident}{mu_camel}ReadTokenGuard");
        let write_token_guard_ident = format_ident!("{struct_ident}{mu_camel}WriteTokenGuard");

        let read_lock_method_ident = format_ident!("read_{lock_ident}", span = lock_ident.span());
        let write_lock_method_ident = format_ident!("write_{lock_ident}", span = lock_ident.span());
        let guard_read_method_ident =
            format_ident!("guard_read_{lock_ident}", span = lock_ident.span());
        let guard_write_method_ident =
            format_ident!("guard_write_{lock_ident}", span = lock_ident.span());
        let guard_read_from_guard_method_ident =
            format_ident!("guard_read_{lock_ident}_from_guard", span = lock_ident.span());
        let guard_write_from_guard_method_ident =
            format_ident!("guard_write_{lock_ident}_from_guard", span = lock_ident.span());

        let this_guarded_fields: Vec<&GuardedField> =
            guarded_fields.iter().filter(|f| f.mutex_idents.contains(lock_ident)).collect();

        let parent_ty = quote! { #struct_ident #ty_generics };
        let field_code = generate_field_code(
            &this_guarded_fields,
            lock_ident,
            class_type,
            &all_locks,
            &parent_ty,
        );

        let read_fields_struct = generate_fields_struct(
            struct_vis,
            &read_fields_ident,
            &fields_decl_generics,
            where_clause,
            &field_code.read_fields_decl,
            phantom_lifetimes,
            phantom_ty_params,
        );

        let write_fields_struct = generate_fields_struct(
            struct_vis,
            &write_fields_ident,
            &fields_decl_generics,
            where_clause,
            &field_code.write_fields_decl,
            phantom_lifetimes,
            phantom_ty_params,
        );

        let is_generic_raw_lock_type =
            mentions_any_ident(raw_lock_type, &generics_info.all_generic_idents);

        let class_registration_code = if brwlock.custom_class.is_none() {
            if is_generic_raw_lock_type {
                generate_generic_lock_class_registration(
                    struct_ident,
                    lock_ident,
                    &mu_camel,
                    class_ident,
                    &class_impl_generics,
                    &class_ty_generics,
                    where_clause,
                    raw_lock_type,
                    brwlock.custom_flags.as_ref(),
                )
            } else {
                let flags_expr = match brwlock.custom_flags.as_ref() {
                    Some(flags) => {
                        quote! { (<#raw_lock_type as ::ksync::RawLock>::LOCK_FLAGS | (#flags)) }
                    }
                    None => quote! { <#raw_lock_type as ::ksync::RawLock>::LOCK_FLAGS },
                };
                generate_lock_class_registration(
                    struct_ident,
                    lock_ident,
                    &mu_camel,
                    class_ident,
                    &class_impl_generics,
                    &class_ty_generics,
                    where_clause,
                    flags_expr,
                )
            }
        } else {
            quote! {}
        };

        let read_guard_accessors = &field_code.read_guard_accessors;
        let write_guard_accessors = &field_code.write_guard_accessors;
        let read_fields_init = &field_code.read_fields_init;
        let write_fields_init = &field_code.write_fields_init;

        let guard_decl_generics = quote! { <'a, #params_with_bounds> };
        let guard_ty_generics = quote! { <'a, #(#ty_params),*> };
        let return_ty_generics = quote! { <'_, #(#ty_params),*> };

        let read_token_guard = generate_token_guard(
            struct_vis,
            struct_ident,
            &ty_generics,
            where_clause,
            class_type,
            &read_token_guard_ident,
            &read_fields_ident,
            &token_guard_decl_generics,
            &token_guard_impl_generics,
            &token_guard_ty_generics,
            &fields_ret_ty_generics,
            &field_code.read_token_guard_accessors,
            &field_code.read_fields_init,
        );

        let write_token_guard = generate_token_guard_mut(
            struct_vis,
            struct_ident,
            &ty_generics,
            where_clause,
            class_type,
            &read_token_guard_ident,
            &write_token_guard_ident,
            &read_fields_ident,
            &write_fields_ident,
            &token_guard_decl_generics,
            &token_guard_impl_generics,
            &token_guard_ty_generics,
            &fields_ret_ty_generics,
            ty_params,
            &field_code.write_token_guard_accessors,
            &field_code.read_fields_init,
            &field_code.write_fields_init,
        );

        let read_guard_inner =
            quote! { ::ksync::BrwLockPiReadGuard<'a, #class_type, #raw_lock_type> };
        let read_guard_struct = generate_pinned_guard_struct(
            struct_vis,
            struct_ident,
            &read_guard_ident,
            &guard_decl_generics,
            &guard_decl_generics,
            &guard_ty_generics,
            &ty_generics,
            where_clause,
            &read_guard_inner,
        );

        let write_guard_inner =
            quote! { ::ksync::BrwLockPiWriteGuard<'a, #class_type, #raw_lock_type> };
        let write_guard_struct = generate_pinned_guard_struct(
            struct_vis,
            struct_ident,
            &write_guard_ident,
            &guard_decl_generics,
            &guard_decl_generics,
            &guard_ty_generics,
            &ty_generics,
            where_clause,
            &write_guard_inner,
        );

        generated_code.extend(quote! {
            #read_fields_struct
            #write_fields_struct
            #class_registration_code
            #read_guard_struct

            impl #guard_decl_generics #read_guard_ident #guard_ty_generics #where_clause {
                #read_guard_accessors

                #[inline]
                #struct_vis fn parent(&self) -> &#struct_ident #ty_generics {
                    self.parent
                }

                #[inline]
                #struct_vis fn as_token_guard<'b>(
                    &'b self,
                ) -> #read_token_guard_ident #token_guard_ty_generics {
                    #read_token_guard_ident {
                        parent: self.parent,
                        token: self.inner.token(),
                    }
                }

                #[inline]
                #struct_vis fn token(&self) -> &::ksync::LockToken<'a, #class_type> {
                    self.inner.token()
                }

                #[inline]
                #struct_vis fn call_unlocked<R, F: FnOnce() -> R>(
                    self: ::core::pin::Pin<&mut Self>,
                    f: F,
                ) -> R {
                    // SAFETY: Projecting `inner` as pinned does not move any pinned fields.
                    let me = unsafe { self.get_unchecked_mut() };
                    // SAFETY: `inner` is structurally pinned.
                    let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
                    inner_pin.call_unlocked(f)
                }

                #[inline]
                #[allow(unused_variables)]
                #struct_vis fn fields<'b>(&'b self) -> #read_fields_ident #fields_ty_generics {
                    let me = self;
                    let token = me.inner.token();
                    #read_fields_ident {
                        #read_fields_init
                        _marker: ::core::marker::PhantomData,
                    }
                }
            }

            #write_guard_struct

            impl #guard_decl_generics #write_guard_ident #guard_ty_generics #where_clause {
                #write_guard_accessors

                #[inline]
                #struct_vis fn parent(&self) -> &#struct_ident #ty_generics {
                    self.parent
                }

                #[inline]
                #struct_vis fn as_token_guard<'b>(
                    &'b self,
                ) -> #read_token_guard_ident #token_guard_ty_generics {
                    #read_token_guard_ident {
                        parent: self.parent,
                        token: self.inner.token(),
                    }
                }

                #[inline]
                #struct_vis fn as_token_guard_mut<'b>(
                    self: ::core::pin::Pin<&'b mut Self>,
                ) -> #write_token_guard_ident #token_guard_ty_generics {
                    // SAFETY: Projecting `inner` as pinned does not move any pinned fields.
                    let me = unsafe { self.get_unchecked_mut() };
                    // SAFETY: `inner` is structurally pinned.
                    let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
                    #write_token_guard_ident {
                        parent: me.parent,
                        token: inner_pin.token_mut(),
                    }
                }

                #[inline]
                #struct_vis fn token(&self) -> &::ksync::LockToken<'a, #class_type> {
                    self.inner.token()
                }

                #[inline]
                #struct_vis fn token_mut(
                    self: ::core::pin::Pin<&mut Self>,
                ) -> &mut ::ksync::LockToken<'a, #class_type> {
                    // SAFETY: Projecting `inner` as pinned does not move any pinned fields.
                    let me = unsafe { self.get_unchecked_mut() };
                    // SAFETY: `inner` is structurally pinned.
                    let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
                    inner_pin.token_mut()
                }

                #[inline]
                #struct_vis fn call_unlocked<R, F: FnOnce() -> R>(
                    self: ::core::pin::Pin<&mut Self>,
                    f: F,
                ) -> R {
                    // SAFETY: Projecting `inner` as pinned does not move any pinned fields.
                    let me = unsafe { self.get_unchecked_mut() };
                    // SAFETY: `inner` is structurally pinned.
                    let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
                    inner_pin.call_unlocked(f)
                }

                #[inline]
                #[allow(unused_variables)]
                #struct_vis fn fields<'b>(&'b self) -> #read_fields_ident #fields_ty_generics {
                    let me = self;
                    let token = me.inner.token();
                    #read_fields_ident {
                        #read_fields_init
                        _marker: ::core::marker::PhantomData,
                    }
                }

                #[inline]
                #[allow(unused_variables)]
                #struct_vis fn fields_mut<'b>(
                    self: ::core::pin::Pin<&'b mut Self>,
                ) -> #write_fields_ident #fields_ty_generics {
                    // SAFETY: Projecting `inner` as pinned does not move any pinned fields.
                    let me = unsafe { self.get_unchecked_mut() };
                    // SAFETY: `inner` is structurally pinned.
                    let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
                    let token = inner_pin.token_mut();
                    #write_fields_ident {
                        #write_fields_init
                        _marker: ::core::marker::PhantomData,
                    }
                }
            }

            // SAFETY: `#write_guard_ident` holds an exclusive write lock guard for `self.parent`.
            unsafe impl #guard_decl_generics
                ::ksync::ExclusiveLockToken<#struct_ident #ty_generics, #class_type>
                for ::core::pin::Pin<&mut #write_guard_ident #guard_ty_generics> #where_clause
            {
                #[inline]
                fn parent(&self) -> *const #struct_ident #ty_generics {
                    ::core::ptr::from_ref(self.parent)
                }

                #[inline]
                fn lock_token_mut(&mut self) -> &mut ::ksync::LockToken<'_, #class_type> {
                    self.as_mut().token_mut().reborrow()
                }
            }

            #read_token_guard
            #write_token_guard

            impl #impl_generics #struct_ident #ty_generics #where_clause {
                #[inline]
                #struct_vis fn #read_lock_method_ident(
                    &self,
                ) -> impl pin_init::PinInit<
                    #read_guard_ident #return_ty_generics,
                    ::core::convert::Infallible,
                > {
                    pin_init::pin_init!(#read_guard_ident {
                        parent: self,
                        inner <- ::ksync::BrwLockPiReadGuard::new(&self.#lock_ident),
                    })
                }

                #[inline]
                #struct_vis fn #write_lock_method_ident(
                    &self,
                ) -> impl pin_init::PinInit<
                    #write_guard_ident #return_ty_generics,
                    ::core::convert::Infallible,
                > {
                    pin_init::pin_init!(#write_guard_ident {
                        parent: self,
                        inner <- ::ksync::BrwLockPiWriteGuard::new(&self.#lock_ident),
                    })
                }

                /// Creates a read-only token guard for accessing fields protected by this lock.
                ///
                /// # Safety
                ///
                /// The caller must ensure that `token` belongs to the specific lock instance that
                /// protects `self`.
                #[inline]
                #struct_vis unsafe fn #guard_read_method_ident<'b, 'a>(
                    &'b self,
                    token: &'b ::ksync::LockToken<'a, #class_type>,
                ) -> #read_token_guard_ident #token_guard_ty_generics {
                    #read_token_guard_ident { parent: self, token }
                }

                /// Creates a mutable token guard for accessing fields protected by this lock.
                ///
                /// # Safety
                ///
                /// The caller must ensure that `token` belongs to the specific lock instance that
                /// protects `self` and is not used to create aliasing references to `self`'s
                /// fields.
                #[inline]
                #struct_vis unsafe fn #guard_write_method_ident<'b, 'a>(
                    &'b self,
                    token: &'b mut ::ksync::LockToken<'a, #class_type>,
                ) -> #write_token_guard_ident #token_guard_ty_generics {
                    #write_token_guard_ident { parent: self, token }
                }

                #[inline]
                #struct_vis fn #guard_read_from_guard_method_ident<'b, 'a>(
                    &'b self,
                    guard: &'b ::ksync::BrwLockPiReadGuard<'a, #class_type, #raw_lock_type>,
                ) -> #read_token_guard_ident #token_guard_ty_generics {
                    assert!(
                        guard.is_locked_by(&self.#lock_ident),
                        "BrwLockPiReadGuard does not belong to this struct's lock"
                    );
                    #read_token_guard_ident { parent: self, token: guard.token() }
                }

                #[inline]
                #struct_vis fn #guard_write_from_guard_method_ident<'b, 'a>(
                    &'b self,
                    guard: ::core::pin::Pin<
                        &'b mut ::ksync::BrwLockPiWriteGuard<'a, #class_type, #raw_lock_type>,
                    >,
                ) -> #write_token_guard_ident #token_guard_ty_generics {
                    assert!(
                        guard.is_locked_by(&self.#lock_ident),
                        "BrwLockPiWriteGuard does not belong to this struct's lock"
                    );
                    #write_token_guard_ident { parent: self, token: guard.token_mut() }
                }
            }
        });
    }

    quote! {
        #marker_structs
        #input_struct
        #generated_code
    }
}

fn to_camel_case(s: &str) -> String {
    let mut camel = String::new();
    let mut capitalize = true;
    for c in s.chars() {
        if c == '_' {
            capitalize = true;
        } else if capitalize {
            camel.push(c.to_ascii_uppercase());
            capitalize = false;
        } else {
            camel.push(c);
        }
    }
    camel
}

fn is_type_named(ty: &Type, name: &str) -> bool {
    matches!(
        ty,
        Type::Path(TypePath { path, .. }) if path.segments.iter().any(|seg| seg.ident == name)
    )
}

fn mentions_any_ident(tokens: &proc_macro2::TokenStream, idents: &[&Ident]) -> bool {
    tokens.clone().into_iter().any(|tt| match tt {
        proc_macro2::TokenTree::Ident(ref id) => idents.contains(&id),
        proc_macro2::TokenTree::Group(ref g) => mentions_any_ident(&g.stream(), idents),
        _ => false,
    })
}

fn is_wavltree_type(ty: &Type) -> bool {
    is_type_named(ty, "WavlTree")
}

fn is_kmutex_type(ty: &Type) -> bool {
    is_type_named(ty, "KMutex")
}

fn is_monitored_spinlock_type(tokens: &proc_macro2::TokenStream) -> bool {
    if let Ok(ty) = syn::parse2::<Type>(tokens.clone()) {
        // As for the kernel's untracked `Mutex` (see `is_untracked_storage_type`), only match the
        // kernel's untracked `MonitoredSpinLock` as the final path segment.
        is_type_named(&ty, "TrackedMonitoredSpinlock")
            || is_type_last_named(&ty, "MonitoredSpinLock")
    } else {
        false
    }
}

fn is_untracked_storage_type(tokens: &proc_macro2::TokenStream) -> bool {
    if let Ok(ty) = syn::parse2::<Type>(tokens.clone()) {
        // The kernel's untracked `Mutex`, `SpinLock` and `MonitoredSpinLock` have generic names, so
        // only match them as the final path segment (e.g. `Mutex` or
        // `crate::kernel::mutex::Mutex`).
        // Matching is by name only: an alias of one under another name must be marked
        // `#[mutex(untracked)]` explicitly.
        is_type_last_named(&ty, "Mutex")
            || is_type_last_named(&ty, "SpinLock")
            || is_type_last_named(&ty, "MonitoredSpinLock")
            || is_type_named(&ty, "RawMutexStorage")
            || is_type_named(&ty, "RawBrwLockPiStorage")
    } else {
        false
    }
}

fn is_type_last_named(ty: &Type, name: &str) -> bool {
    let Type::Path(TypePath { path, .. }) = ty else {
        return false;
    };
    path.segments.last().is_some_and(|seg| seg.ident == name)
}

fn is_brwlock_type(ty: &Type) -> bool {
    is_type_named(ty, "BrwLockPi")
}

fn is_konce_cell_type(ty: &Type) -> bool {
    is_type_named(ty, "KOnceCell")
}

fn extract_konce_cell_inner_type(ty: &Type) -> Option<Type> {
    let Type::Path(TypePath { path, .. }) = ty else { return None };
    let seg = path.segments.iter().find(|s| s.ident == "KOnceCell")?;
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else { return None };
    let syn::GenericArgument::Type(inner_ty) = args.args.first()? else { return None };
    Some(inner_ty.clone())
}

fn is_phantom_mutex_type(ty: &Type) -> bool {
    let Type::Path(TypePath { path, .. }) = ty else { return false };
    let Some(seg) = path.segments.last() else { return false };
    if seg.ident != "KMutex" {
        return false;
    }
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else { return false };
    let Some(syn::GenericArgument::Type(arg_ty)) = args.args.first() else { return false };
    is_type_named(arg_ty, "PhantomMutex")
}

fn extract_lock_type(ty: &Type) -> Result<Option<proc_macro2::TokenStream>, syn::Error> {
    let Type::Path(type_path) = ty else { return Ok(None) };
    let Some(last_segment) = type_path.path.segments.last() else { return Ok(None) };
    if last_segment.ident != "KMutex" {
        return Ok(None);
    }

    match &last_segment.arguments {
        syn::PathArguments::None => Ok(None),
        syn::PathArguments::AngleBracketed(args) => match args.args.len() {
            0 => Ok(None),
            1 => {
                let first_arg = &args.args[0];
                Ok(Some(quote! { #first_arg }))
            }
            _ => Err(syn::Error::new(
                args.span(),
                "KMutex expects at most 1 generic argument for the lock type in struct definition",
            )),
        },
        syn::PathArguments::Parenthesized(args) => Err(syn::Error::new(
            args.span(),
            "KMutex does not support parenthesized generic arguments",
        )),
    }
}

/// Extracts the raw lock type `L` from a `BrwLockPi<L>` field type in a `#[guarded]` struct.
fn extract_brwlock_type(ty: &Type) -> Result<proc_macro2::TokenStream, syn::Error> {
    let missing = || {
        syn::Error::new(
            ty.span(),
            "BrwLockPi fields must specify the raw lock type explicitly, e.g. \
             `BrwLockPi<RawBrwLockPi>` (specify a custom lock class with #[brwlock(LockClass)])",
        )
    };
    let Type::Path(type_path) = ty else { return Err(missing()) };
    let Some(last_segment) = type_path.path.segments.last() else { return Err(missing()) };
    match &last_segment.arguments {
        syn::PathArguments::AngleBracketed(args) if args.args.len() == 1 => {
            let first_arg = &args.args[0];
            Ok(quote! { #first_arg })
        }
        syn::PathArguments::AngleBracketed(args) if args.args.len() > 1 => Err(syn::Error::new(
            args.span(),
            "BrwLockPi expects exactly 1 generic argument for the raw lock type in struct \
             definition; specify a custom lock class with #[brwlock(LockClass)]",
        )),
        _ => Err(missing()),
    }
}

/// Placement of a singleton lock's static storage, mirroring the C++ kernel's `<align.h>`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SingletonPlacement {
    /// Like C++ `__CPU_ALIGN`: aligned to (and padded out to) a cache line.
    CpuAlign,
    /// Like C++ `__CPU_ALIGN_EXCLUSIVE`: `CpuAlign`, and placed in `.data.cpu_align_exclusive`.
    CpuAlignExclusive,
}

impl SingletonPlacement {
    /// Returns the placement named by `ident`, if any.
    fn from_ident(ident: &Ident) -> Option<Self> {
        if ident == "cpu_align" {
            Some(Self::CpuAlign)
        } else if ident == "cpu_align_exclusive" {
            Some(Self::CpuAlignExclusive)
        } else {
            None
        }
    }
}

struct SingletonLockInput {
    attrs: Vec<syn::Attribute>,
    vis: syn::Visibility,
    _struct_token: Option<syn::Token![struct]>,
    name: Ident,
    lock_type: Option<Type>,
    /// Optional placement of the singleton's static storage.
    placement: Option<SingletonPlacement>,
    /// Optional extra lockdep flags OR'd into `LockClassRegistration::with_flags`.
    flags: Option<syn::Expr>,
}

impl syn::parse::Parse for SingletonLockInput {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let attrs = input.call(syn::Attribute::parse_outer)?;
        let vis = input.parse::<syn::Visibility>()?;
        let _struct_token = if input.peek(syn::Token![struct]) {
            Some(input.parse::<syn::Token![struct]>()?)
        } else {
            None
        };
        let name = input.parse::<Ident>()?;
        let mut lock_type = None;
        let mut placement = None;
        let mut flags = None;
        while input.peek(syn::Token![,]) {
            let _: syn::Token![,] = input.parse()?;
            if input.is_empty() {
                break;
            }
            if input.peek(Ident) && input.peek2(syn::Token![=]) {
                let key = input.parse::<Ident>()?;
                if key != "flags" {
                    return Err(syn::Error::new(key.span(), "expected `flags = <expr>`"));
                }
                if flags.is_some() {
                    return Err(syn::Error::new(
                        key.span(),
                        "duplicate `flags` argument in `declare_singleton_lock!`",
                    ));
                }
                let _: syn::Token![=] = input.parse()?;
                flags = Some(input.parse::<syn::Expr>()?);
                continue;
            }
            // A placement keyword is a bare identifier followed by a comma or the end of the
            // input. Anything else must be the lock type.
            let fork = input.fork();
            let keyword = fork
                .parse::<Ident>()
                .ok()
                .filter(|_| fork.is_empty() || fork.peek(syn::Token![,]))
                .and_then(|ident| SingletonPlacement::from_ident(&ident).map(|p| (ident, p)));
            if let Some((ident, p)) = keyword {
                let _: Ident = input.parse()?;
                if placement.is_some() {
                    return Err(syn::Error::new(
                        ident.span(),
                        "only one of `cpu_align` or `cpu_align_exclusive` may be given",
                    ));
                }
                placement = Some(p);
                continue;
            }
            if lock_type.is_some() || placement.is_some() || flags.is_some() {
                return Err(input.error(
                    "expected `cpu_align`, `cpu_align_exclusive`, or `flags = <expr>` \
                     (the lock type must come right after the name)",
                ));
            }
            lock_type = Some(input.parse::<Type>()?);
        }
        Ok(SingletonLockInput { attrs, vis, _struct_token, name, lock_type, placement, flags })
    }
}

/// Declares a singleton lock class and its statically initialized lock.
///
/// Syntax:
/// `declare_singleton_lock!([#[attrs]] [vis] Name, RawLockType [, placement]`
/// `[, flags = <expr>] [,])`.
///
/// `RawLockType` is required: `ksync` has no default raw lock type, since it cannot name the
/// kernel's `TrackedMutex`. Passing an untracked storage type (such as `SpinLock`) omits
/// lockdep class registration and per-acquisition tracking state. The optional `placement`
/// controls the singleton's static storage, mirroring the C++ kernel's `<align.h>`:
///
/// - `cpu_align`: like C++ `__CPU_ALIGN`, aligns the static to a cache line (64 bytes, the kernel's
///   `MAX_CACHE_LINE` on every supported architecture) and pads its size to a multiple of it.
/// - `cpu_align_exclusive`: like C++ `__CPU_ALIGN_EXCLUSIVE`, `cpu_align` plus placing the static
///   in `.data.cpu_align_exclusive`, so that the lock has exclusive use of its cache line(s).
///
/// The optional `flags = <expr>` argument specifies additional lockdep flags to OR into
/// `LockClassRegistration::with_flags`.
#[proc_macro]
pub fn declare_singleton_lock(input: TokenStream) -> TokenStream {
    declare_singleton_lock_impl(input.into()).into()
}

fn declare_singleton_lock_impl(input: proc_macro2::TokenStream) -> proc_macro2::TokenStream {
    let SingletonLockInput { attrs, vis, name, lock_type, placement, flags, .. } =
        match syn::parse2::<SingletonLockInput>(input) {
            Ok(parsed) => parsed,
            Err(err) => return err.to_compile_error(),
        };

    let Some(raw_lock) = lock_type else {
        return syn::Error::new(
            name.span(),
            "declare_singleton_lock! requires an explicit raw lock type, e.g. \
             `declare_singleton_lock!(Name, TrackedMutex)` (kernel) or \
             `declare_singleton_lock!(Name, RawMutex)` (host)",
        )
        .to_compile_error();
    };
    let raw_lock_tokens = quote! { #raw_lock };
    let is_untracked = is_untracked_storage_type(&raw_lock_tokens);
    if is_untracked && flags.is_some() {
        return syn::Error::new(
            name.span(),
            "untracked singleton locks cannot specify lockdep flags",
        )
        .to_compile_error();
    }

    let name_str = name.to_string();
    let name_upper = name_str.to_ascii_uppercase();
    let string_reg_ident = format_ident!("{}_STRING_REG", name_upper);
    let reg_ident = format_ident!("{}_REGISTRATION", name_upper);
    let extra_flags = match flags {
        Some(flags) => quote! { | (#flags) },
        None => quote! {},
    };

    let lock_methods = if is_monitored_spinlock_type(&raw_lock_tokens) {
        quote! {
            /// Acquires the singleton monitored spinlock using the default policy and source tag.
            #[inline]
            pub fn lock(
                tag: ::ksync::SourceTag,
            ) -> impl ::ksync::pin_init::PinInit<
                ::ksync::KMutexGuard<'static, #name, #raw_lock>,
                ::core::convert::Infallible,
            > {
                Self::get().lock_with(tag)
            }

            /// Acquires the singleton monitored spinlock using the specified policy and arguments.
            #[inline]
            pub fn lock_policy<P: ::ksync::ExclusiveLockPolicy<#raw_lock>>(
                args: P::AcquireArgs,
            ) -> impl ::ksync::pin_init::PinInit<
                ::ksync::KMutexGuard<'static, #name, #raw_lock, P>,
                ::core::convert::Infallible,
            > {
                Self::get().lock_policy_with::<P>(args)
            }
        }
    } else {
        quote! {
            /// Acquires the singleton mutex using the default policy.
            #[inline]
            pub fn lock() -> impl ::ksync::pin_init::PinInit<
                ::ksync::KMutexGuard<'static, #name, #raw_lock>,
                ::core::convert::Infallible,
            > {
                Self::get().lock()
            }

            /// Acquires the singleton mutex using the specified lock policy.
            #[inline]
            pub fn lock_policy<
                P: ::ksync::ExclusiveLockPolicy<#raw_lock, AcquireArgs = ()>,
            >() -> impl ::ksync::pin_init::PinInit<
                ::ksync::KMutexGuard<'static, #name, #raw_lock, P>,
                ::core::convert::Infallible,
            > {
                Self::get().lock_policy::<P>()
            }
        }
    };

    let singleton_init = if is_untracked {
        quote! {
            ::ksync::KMutex::new(<#raw_lock as ::ksync::ConstRawLock>::INIT)
        }
    } else {
        quote! {
            ::ksync::KMutex::new(<#raw_lock>::const_init(<#name as ::ksync::LockClass>::ID))
        }
    };
    let section_attr = (placement == Some(SingletonPlacement::CpuAlignExclusive))
        .then(|| quote! { #[unsafe(link_section = ".data.cpu_align_exclusive")] });
    let singleton_storage = if placement.is_some() {
        // Wrap the lock so that the static is aligned to (and padded out to a multiple of) a cache
        // line, like C++ `__CPU_ALIGN`.
        quote! {
            #[repr(C, align(64))]
            struct __KsyncCpuAligned(::ksync::KMutex<#name, #raw_lock>);
            #section_attr
            static SINGLETON: __KsyncCpuAligned = __KsyncCpuAligned(#singleton_init);
            &SINGLETON.0
        }
    } else {
        quote! {
            static SINGLETON: ::ksync::KMutex<#name, #raw_lock> = #singleton_init;
            &SINGLETON
        }
    };

    let class_registration = if is_untracked {
        quote! {
            impl ::ksync::LockClass for #name {
                const ID: *mut ::core::ffi::c_void = ::core::ptr::null_mut();
                const FLAGS: ::ksync::LockFlags = ::ksync::LOCK_FLAGS_NONE;
            }
        }
    } else {
        quote! {
            const _: () = {
                ::ksync::declare_interned_string!(#string_reg_ident, #name_str);

                #[unsafe(link_section = "rust_lock_classes")]
                #[used]
                static #reg_ident: ::ksync::LockClassRegistration =
                    ::ksync::LockClassRegistration::with_flags(
                        &#string_reg_ident,
                        <#name as ::ksync::LockClass>::FLAGS,
                    );

                impl ::ksync::LockClass for #name {
                    const ID: *mut ::core::ffi::c_void = #reg_ident.get();
                    const FLAGS: ::ksync::LockFlags =
                        <#raw_lock>::LOCK_FLAGS
                            | ::ksync::LOCK_FLAGS_SINGLETON_LOCK
                            #extra_flags;
                }
            };
        }
    };

    quote! {
        #(#attrs)*
        #[derive(Debug, Copy, Clone)]
        #vis struct #name;

        #class_registration

        impl #name {
            /// Returns a reference to the global singleton mutex.
            #[allow(non_snake_case)]
            #[inline]
            pub fn Get() -> &'static ::ksync::KMutex<#name, #raw_lock> {
                Self::get()
            }

            /// Returns a reference to the global singleton mutex.
            #[inline]
            pub fn get() -> &'static ::ksync::KMutex<#name, #raw_lock> {
                #singleton_storage
            }

            #lock_methods
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guarded_by_compile_errors() {
        // 1. Empty `#[guarded_by()]` list.
        let err_empty = guarded_impl(quote! {
            struct BadEmpty {
                #[mutex]
                mu: KMutex<RawMutex>,
                #[guarded_by()]
                val: u32,
            }
        })
        .to_string();
        assert!(
            err_empty.contains("compile_error")
                && err_empty.contains("requires at least one lock identifier"),
            "unexpected output: {err_empty}"
        );

        // 2. Duplicate lock in `#[guarded_by(mu, mu)]`.
        let err_dup = guarded_impl(quote! {
            struct BadDup {
                #[mutex]
                mu: KMutex<RawMutex>,
                #[guarded_by(mu, mu)]
                val: u32,
            }
        })
        .to_string();
        assert!(
            err_dup.contains("compile_error")
                && err_dup.contains("Duplicate lock 'mu' in #[guarded_by] attribute"),
            "unexpected output: {err_dup}"
        );

        // 3. Non-existent lock in `#[guarded_by(mu, missing)]`.
        let err_missing = guarded_impl(quote! {
            struct BadMissing {
                #[mutex]
                mu: KMutex<RawMutex>,
                #[guarded_by(mu, missing)]
                val: u32,
            }
        })
        .to_string();
        assert!(
            err_missing.contains("compile_error")
                && err_missing
                    .contains("Guarded field 'val' references non-existent lock 'missing'"),
            "unexpected output: {err_missing}"
        );

        // 4. Lock field itself annotated with `#[guarded_by(...)]`.
        let err_lock_guarded = guarded_impl(quote! {
            struct BadLockGuarded {
                #[mutex]
                #[guarded_by(mu2)]
                mu1: KMutex<RawMutex>,
                #[mutex]
                mu2: KMutex<RawMutex>,
            }
        })
        .to_string();
        assert!(
            err_lock_guarded.contains("compile_error")
                && err_lock_guarded.contains("Lock fields cannot also have #[guarded_by]"),
            "unexpected output: {err_lock_guarded}"
        );

        // 5. Two lock fields on the same struct sharing a duplicate custom lock class.
        let err_dup_class = guarded_impl(quote! {
            struct BadDupClass {
                #[mutex(SharedCustomClass)]
                mu1: KMutex<RawMutex>,
                #[mutex(SharedCustomClass)]
                mu2: KMutex<RawMutex>,
            }
        })
        .to_string();
        assert!(
            err_dup_class.contains("compile_error")
                && err_dup_class.contains(
                    "Lock class `SharedCustomClass` is already used by another lock field"
                ),
            "unexpected output: {err_dup_class}"
        );

        // 6. Bare `#[guarded_by]` without a parenthesized list.
        let err_bare = guarded_impl(quote! {
            struct BadBare {
                #[mutex]
                mu: KMutex<RawMutex>,
                #[guarded_by]
                val: u32,
            }
        })
        .to_string();
        assert!(
            err_bare.contains("compile_error")
                && err_bare.contains("#[guarded_by] attribute must be a list of lock identifiers"),
            "unexpected output: {err_bare}"
        );

        // 7. Invalid token inside `#[guarded_by(123)]`.
        let err_invalid_tok = guarded_impl(quote! {
            struct BadInvalidTok {
                #[mutex]
                mu: KMutex<RawMutex>,
                #[guarded_by(123)]
                val: u32,
            }
        })
        .to_string();
        assert!(
            err_invalid_tok.contains("compile_error")
                && err_invalid_tok.contains("expected identifier"),
            "unexpected output: {err_invalid_tok}"
        );

        // 8. More than 3 locks in `#[guarded_by(mu1, mu2, mu3, mu4)]`.
        let err_too_many = guarded_impl(quote! {
            struct BadTooMany {
                #[mutex]
                mu1: KMutex<RawMutex>,
                #[mutex]
                mu2: KMutex<RawMutex>,
                #[mutex]
                mu3: KMutex<RawMutex>,
                #[mutex]
                mu4: KMutex<RawMutex>,
                #[guarded_by(mu1, mu2, mu3, mu4)]
                val: u32,
            }
        })
        .to_string();
        assert!(
            err_too_many.contains("compile_error")
                && err_too_many.contains("#[guarded_by(...)] supports at most 3 locks"),
            "unexpected output: {err_too_many}"
        );

        // 9. Combining `untracked` and `flags = ...` on `#[mutex(...)]`.
        let err_untracked_flags = guarded_impl(quote! {
            struct BadUntrackedFlags {
                #[mutex(untracked, flags = 1)]
                mu: KMutex<RawMutexStorage>,
                #[guarded_by(mu)]
                val: u32,
            }
        })
        .to_string();
        assert!(
            err_untracked_flags.contains("compile_error")
                && err_untracked_flags.contains("untracked mutexes cannot specify lockdep flags"),
            "unexpected output: {err_untracked_flags}"
        );
    }

    #[test]
    fn test_generic_lock_class_generation() {
        let out = guarded_impl(quote! {
            struct GenericLockObj<L: ::ksync::RawLock, const C: usize> {
                #[mutex(flags = ::ksync::LOCK_FLAGS_NESTABLE)]
                mu: ::ksync::KMutex<L>,
                #[guarded_by(mu)]
                val: u32,
            }
        })
        .to_string();
        assert!(!out.contains("compile_error"), "unexpected error: {out}");

        assert!(out.contains("GENERICLOCKOBJ_MU_REGISTRATION"), "missing registration: {out}");
        assert!(
            out.contains("GENERICLOCKOBJ_MU_IRQ_SAFE_REGISTRATION"),
            "missing irq safe registration: {out}"
        );
        assert!(out.contains("\"GenericLockObj::mu\""), "missing string: {out}");
        assert!(
            out.contains("\"GenericLockObj::mu (irq_safe)\""),
            "missing irq safe string: {out}"
        );
        assert!(
            out.contains("LOCK_FLAGS_TRACKING_DISABLED"),
            "missing tracking disabled check: {out}"
        );
        assert!(out.contains("LOCK_FLAGS_IRQ_SAFE"), "missing irq safe flag check: {out}");
        assert!(
            out.contains(concat!(
                "impl < L : :: ksync :: RawLock , const C : usize > :: ksync :: LockClass",
                " for GenericLockObjMuClass < L , C >"
            )),
            "missing impl: {out}"
        );

        assert!(
            out.contains(concat!(
                "fn lock_mu_with (& self , args : << L as :: ksync :: RawLock > ::",
                " DefaultPolicy as :: ksync :: LockPolicy < L , >> :: AcquireArgs ,) ->",
                " impl pin_init :: PinInit < GenericLockObjMuGuard < '_ , L , C > , :: core",
                " :: convert :: Infallible , >"
            )),
            "missing lock_mu_with: {out}"
        );
        assert!(
            out.contains(concat!(
                "fn lock_mu_policy_with < P : :: ksync :: ExclusiveLockPolicy < L > , > (&",
                " self , args : P :: AcquireArgs ,) -> impl pin_init :: PinInit <",
                " GenericLockObjMuGuard < '_ , L , C , P > , :: core :: convert ::",
                " Infallible , >"
            )),
            "missing lock_mu_policy_with: {out}"
        );
        assert!(
            out.contains(concat!(
                "fn lock_mu_aliased_with < 'a , AliasClass : :: ksync :: LockClass , M2 :",
                " :: ksync :: RawLock , > (& 'a self , alias : & 'a :: ksync :: KMutex <",
                " AliasClass , M2 > , args : << L as :: ksync :: RawLock > :: DefaultPolicy",
                " as :: ksync :: LockPolicy < L , >> :: AcquireArgs ,) -> impl pin_init ::",
                " PinInit < :: ksync :: KMutexAliasedGuard < 'a , GenericLockObjMuClass < L",
                " , C > , AliasClass , L , M2 > , :: core :: convert :: Infallible , >"
            )),
            "missing lock_mu_aliased_with: {out}"
        );
        assert!(
            out.contains(concat!(
                "fn lock_mu_aliased_policy_with < 'a , AliasClass : :: ksync :: LockClass ,",
                " M2 : :: ksync :: RawLock , P : :: ksync :: ExclusiveLockPolicy < L > , >",
                " (& 'a self , alias : & 'a :: ksync :: KMutex < AliasClass , M2 > , args :",
                " P :: AcquireArgs ,) -> impl pin_init :: PinInit < :: ksync ::",
                " KMutexAliasedGuard < 'a , GenericLockObjMuClass < L , C > , AliasClass ,",
                " L , M2 , P > , :: core :: convert :: Infallible , >"
            )),
            "missing lock_mu_aliased_policy_with: {out}"
        );
        assert!(
            out.contains(concat!(
                "fn lock_mu_multiple_with < 'a , const __KSYNC_N : usize > (items : [& 'a",
                " Self ; __KSYNC_N] , args : << L as :: ksync :: RawLock > :: DefaultPolicy",
                " as :: ksync :: LockPolicy < L , >> :: AcquireArgs ,) -> impl pin_init ::",
                " PinInit < :: ksync :: KMutexGuardMultiple < 'a , __KSYNC_N ,",
                " GenericLockObjMuClass < L , C > , L > , :: core :: convert :: Infallible",
                " , >"
            )),
            "missing lock_mu_multiple_with: {out}"
        );
        assert!(
            out.contains(concat!(
                "fn lock_mu_multiple_policy_with < 'a , const __KSYNC_N : usize , P : ::",
                " ksync :: ExclusiveLockPolicy < L > , > (items : [& 'a Self ; __KSYNC_N] ,",
                " args : P :: AcquireArgs ,) -> impl pin_init :: PinInit < :: ksync ::",
                " KMutexGuardMultiple < 'a , __KSYNC_N , GenericLockObjMuClass < L , C > ,",
                " L , P > , :: core :: convert :: Infallible , >"
            )),
            "missing lock_mu_multiple_policy_with: {out}"
        );

        let out_phantom = guarded_impl(quote! {
            struct PhantomObj<L: ::ksync::RawLock> {
                #[mutex(CustomClass)]
                mu: ::ksync::KMutex<::ksync::PhantomMutex>,
                #[guarded_by(mu)]
                val: u32,
            }
        })
        .to_string();
        assert!(!out_phantom.contains("compile_error"), "unexpected error: {out_phantom}");
        assert!(
            out_phantom.contains(concat!(
                "unsafe fn lock_mu_with < 'a , M : :: ksync :: RawLock > (& 'a self ,",
                " real_mutex : & 'a :: ksync :: KMutex < CustomClass , M > , args : << M as",
                " :: ksync :: RawLock > :: DefaultPolicy as :: ksync :: LockPolicy < M >>",
                " :: AcquireArgs ,) -> impl pin_init :: PinInit < PhantomObjMuGuard < 'a ,",
                " L , M > , :: core :: convert :: Infallible , >"
            )),
            "missing phantom lock_mu_with: {out_phantom}"
        );
        assert!(
            out_phantom.contains(concat!(
                "unsafe fn lock_mu_policy_with < 'a , M : :: ksync :: RawLock , P : ::",
                " ksync :: ExclusiveLockPolicy < M > , > (& 'a self , real_mutex : & 'a ::",
                " ksync :: KMutex < CustomClass , M > , args : < P as :: ksync ::",
                " LockPolicy < M >> :: AcquireArgs ,) -> impl pin_init :: PinInit <",
                " PhantomObjMuGuard < 'a , L , M , P > , :: core :: convert :: Infallible ,",
                " >"
            )),
            "missing phantom lock_mu_policy_with: {out_phantom}"
        );
    }
    #[test]
    fn test_untracked_lock_class_generation() {
        let out_storage = guarded_impl(quote! {
            struct UntrackedStorageObj {
                #[mutex]
                mu: KMutex<RawMutexStorage>,
                #[guarded_by(mu)]
                val: u32,
            }
        })
        .to_string();
        assert!(!out_storage.contains("compile_error"), "unexpected error: {out_storage}");
        assert!(!out_storage.contains("rust_lock_classes"), "unexpected section: {out_storage}");
        assert!(out_storage.contains("null_mut"), "expected null_mut ID: {out_storage}");

        // The kernel's untracked `Mutex`, `SpinLock` and `MonitoredSpinLock`, named directly or by
        // path.
        for kernel_mutex in [
            quote! { Mutex },
            quote! { crate::kernel::mutex::Mutex },
            quote! { SpinLock },
            quote! { crate::kernel::spinlock::SpinLock },
            quote! { MonitoredSpinLock },
            quote! { crate::kernel::spinlock::MonitoredSpinLock },
        ] {
            let out_mutex = guarded_impl(quote! {
                struct UntrackedKernelMutexObj {
                    #[mutex]
                    mu: KMutex<#kernel_mutex>,
                    #[guarded_by(mu)]
                    val: u32,
                }
            })
            .to_string();
            assert!(!out_mutex.contains("compile_error"), "unexpected error: {out_mutex}");
            assert!(!out_mutex.contains("rust_lock_classes"), "unexpected section: {out_mutex}");
            assert!(out_mutex.contains("null_mut"), "expected null_mut ID: {out_mutex}");
        }

        // A type merely nested in a `Mutex` path segment is not the kernel's untracked `Mutex`.
        let out_nested = guarded_impl(quote! {
            struct NestedMutexPathObj {
                #[mutex]
                mu: KMutex<Mutex::Tracked>,
                #[guarded_by(mu)]
                val: u32,
            }
        })
        .to_string();
        assert!(!out_nested.contains("compile_error"), "unexpected error: {out_nested}");
        assert!(out_nested.contains("rust_lock_classes"), "expected section: {out_nested}");

        let out_explicit = guarded_impl(quote! {
            struct ExplicitUntrackedObj {
                #[mutex(untracked)]
                mu: KMutex<CustomUntrackedLock>,
                #[guarded_by(mu)]
                val: u32,
            }
        })
        .to_string();
        assert!(!out_explicit.contains("compile_error"), "unexpected error: {out_explicit}");
        assert!(!out_explicit.contains("rust_lock_classes"), "unexpected section: {out_explicit}");
        assert!(out_explicit.contains("null_mut"), "expected null_mut ID: {out_explicit}");

        // Untracked classes report `LOCK_FLAGS_NONE`; tracked classes share one `const` between
        // the registration and `LockClass::FLAGS`.
        for out in [&out_storage, &out_explicit] {
            assert!(out.contains("const FLAGS"), "expected LockClass::FLAGS: {out}");
            assert!(out.contains("LOCK_FLAGS_NONE"), "expected LOCK_FLAGS_NONE: {out}");
        }
        let out_tracked = guarded_impl(quote! {
            struct TrackedObj {
                #[mutex(flags = lockdep::LOCK_FLAGS_NESTABLE)]
                mu: KMutex<RawMutex>,
                #[guarded_by(mu)]
                val: u32,
            }
        })
        .to_string();
        assert!(!out_tracked.contains("compile_error"), "unexpected error: {out_tracked}");
        assert!(out_tracked.contains("rust_lock_classes"), "expected section: {out_tracked}");
        assert!(
            out_tracked.contains("const TRACKEDOBJ_MU_FLAGS")
                && out_tracked.contains("LOCK_FLAGS_NESTABLE")
                && out_tracked
                    .contains("const FLAGS : :: ksync :: LockFlags = TRACKEDOBJ_MU_FLAGS")
                && out_tracked.contains("TRACKEDOBJ_MU_STRING_REG , TRACKEDOBJ_MU_FLAGS")
                && !out_tracked.contains("LOCK_FLAGS_NONE"),
            "unexpected output: {out_tracked}"
        );
    }

    #[test]
    fn test_explicit_lock_class_with_flags_compile_errors() {
        // An explicit lock class's flags are fixed where it is registered, so `flags = ...` must
        // be rejected rather than silently ignored, including for generic `CustomClass<T>` types.
        for (attr, lock_ty) in [
            ("mutex", quote! { KMutex<RawMutex> }),
            ("brwlock", quote! { BrwLockPi<RawBrwLockPi> }),
        ] {
            let attr_ident = format_ident!("{attr}");
            for args in [
                quote! { CustomClass, flags = lockdep::LOCK_FLAGS_NESTABLE },
                quote! { CustomClass<T>, flags = lockdep::LOCK_FLAGS_NESTABLE },
                quote! { flags = lockdep::LOCK_FLAGS_NESTABLE, CustomClass<T> },
            ] {
                let err = guarded_impl(quote! {
                    struct BadExplicitClassFlags<T> {
                        #[#attr_ident(#args)]
                        lock: #lock_ty,
                        #[guarded_by(lock)]
                        val: T,
                    }
                })
                .to_string();
                assert!(
                    err.contains("compile_error")
                        && err.contains(&format!(
                            "#[{attr}] cannot combine `flags = ...` with an explicit `LockClass`"
                        ))
                        && err.contains("explicit `LockClass` specified here"),
                    "unexpected output for #[{attr}({args})]: {err}"
                );
            }
        }
    }

    #[test]
    fn test_explicit_lock_class_with_untracked_compile_errors() {
        // Whether an explicit lock class is tracked is fixed where it is registered, so
        // `untracked` must be rejected rather than silently ignored.
        for args in [
            quote! { CustomClass, untracked },
            quote! { untracked, CustomClass<T> },
            quote! { CustomClass<T>, untracked },
        ] {
            let err = guarded_impl(quote! {
                struct BadExplicitClassUntracked<T> {
                    #[mutex(#args)]
                    lock: KMutex<RawMutex>,
                    #[guarded_by(lock)]
                    val: T,
                }
            })
            .to_string();
            assert!(
                err.contains("compile_error")
                    && err.contains(
                        "#[mutex] cannot combine `untracked` with an explicit `LockClass`"
                    )
                    && err.contains("explicit `LockClass` specified here"),
                "unexpected output for #[mutex({args})]: {err}"
            );
        }

        // An explicit class on an untracked *storage* type is fine: the storage type, not the
        // attribute, decides that the field is untracked.
        let ok = guarded_impl(quote! {
            struct ExplicitClassUntrackedStorage {
                #[mutex(CustomClass)]
                lock: KMutex<RawMutexStorage>,
                #[guarded_by(lock)]
                val: u32,
            }
        })
        .to_string();
        assert!(!ok.contains("compile_error"), "unexpected error: {ok}");
    }

    #[test]
    fn test_brwlock_untracked_compile_errors() {
        // `BrwLockPi` has no untracked storage type and brwlock classes are always registered,
        // so `untracked` is rejected on `#[brwlock]` instead of being parsed and ignored.
        for args in [quote! { untracked }, quote! { untracked, flags = lockdep::LOCK_FLAGS_LEAF }] {
            let err = guarded_impl(quote! {
                struct BadBrwLockUntracked {
                    #[brwlock(#args)]
                    lock: BrwLockPi<RawBrwLockPi>,
                    #[guarded_by(lock)]
                    val: u32,
                }
            })
            .to_string();
            assert!(
                err.contains("compile_error")
                    && err.contains("#[brwlock] does not support `untracked`"),
                "unexpected output for #[brwlock({args})]: {err}"
            );
        }
    }

    #[test]
    fn test_unknown_lock_attribute_arg_does_not_abort_parsing() {
        // An unknown `key = value` argument is reported, but the remaining arguments are still
        // parsed so that their diagnostics are not lost.
        let err = guarded_impl(quote! {
            struct BadUnknownKey {
                #[mutex(bogus = 1, CustomClass, flags = lockdep::LOCK_FLAGS_NESTABLE)]
                lock: KMutex<RawMutex>,
                #[guarded_by(lock)]
                val: u32,
            }
        })
        .to_string();
        assert!(
            err.contains("compile_error")
                && err.contains("#[mutex] attribute accepts `LockClass`, `untracked`, or")
                && err
                    .contains("#[mutex] cannot combine `flags = ...` with an explicit `LockClass`"),
            "unexpected output: {err}"
        );

        let err_brwlock = guarded_impl(quote! {
            struct BadUnknownKeyBrwLock {
                #[brwlock(bogus = 1)]
                lock: BrwLockPi<RawBrwLockPi>,
                #[guarded_by(lock)]
                val: u32,
            }
        })
        .to_string();
        assert!(
            err_brwlock.contains("compile_error")
                && err_brwlock.contains("#[brwlock] attribute accepts `LockClass` or"),
            "unexpected output: {err_brwlock}"
        );
    }

    #[test]
    fn test_duplicate_lock_attribute_args_compile_errors() {
        for (attr, lock_ty) in [
            ("mutex", quote! { KMutex<RawMutex> }),
            ("brwlock", quote! { BrwLockPi<RawBrwLockPi> }),
        ] {
            let attr_ident = format_ident!("{attr}");

            // 1. Duplicate `flags = ...` in a single attribute.
            let err_dup_flags = guarded_impl(quote! {
                struct BadDupFlags {
                    #[#attr_ident(
                        flags = lockdep::LOCK_FLAGS_NESTABLE,
                        flags = lockdep::LOCK_FLAGS_LEAF,
                    )]
                    lock: #lock_ty,
                    #[guarded_by(lock)]
                    val: u32,
                }
            })
            .to_string();
            assert!(
                err_dup_flags.contains("compile_error")
                    && err_dup_flags
                        .contains(&format!("Duplicate `flags` argument in #[{attr}] attribute")),
                "unexpected output for duplicate flags on #[{attr}]: {err_dup_flags}"
            );

            // 2. Duplicate explicit `LockClass` in a single attribute.
            let err_dup_class = guarded_impl(quote! {
                struct BadDupClass {
                    #[#attr_ident(ClassA, ClassB)]
                    lock: #lock_ty,
                    #[guarded_by(lock)]
                    val: u32,
                }
            })
            .to_string();
            assert!(
                err_dup_class.contains("compile_error")
                    && err_dup_class.contains(&format!(
                        "Duplicate `LockClass` argument in #[{attr}] attribute"
                    )),
                "unexpected output for duplicate class on #[{attr}]: {err_dup_class}"
            );

            // 3. Repeated `#[mutex]` / `#[brwlock]` attributes on the same field.
            let err_dup_attr = guarded_impl(quote! {
                struct BadDupAttr {
                    #[#attr_ident(ClassA)]
                    #[#attr_ident(flags = lockdep::LOCK_FLAGS_NESTABLE)]
                    lock: #lock_ty,
                    #[guarded_by(lock)]
                    val: u32,
                }
            })
            .to_string();
            assert!(
                err_dup_attr.contains("compile_error")
                    && err_dup_attr.contains(&format!("Duplicate #[{attr}] attribute on field")),
                "unexpected output for duplicate #[{attr}] attribute: {err_dup_attr}"
            );
        }

        // 4. Combining `#[mutex]` and `#[brwlock]` on the same field.
        let err_mixed = guarded_impl(quote! {
            struct BadMixedAttr {
                #[mutex]
                #[brwlock]
                lock: KMutex<RawMutex>,
                #[guarded_by(lock)]
                val: u32,
            }
        })
        .to_string();
        assert!(
            err_mixed.contains("compile_error")
                && err_mixed.contains("Field cannot have both #[mutex] and #[brwlock] attributes"),
            "unexpected output for mixed #[mutex]/#[brwlock]: {err_mixed}"
        );
    }

    #[test]
    fn test_explicit_brwlock_class_is_used() {
        let output = guarded_impl(quote! {
            struct ExplicitBrwLockClass {
                #[brwlock(CustomBrwLockClass)]
                lock: BrwLockPi<RawBrwLockPi>,
                #[guarded_by(lock)]
                val: u32,
            }
        })
        .to_string();
        assert!(!output.contains("compile_error"), "unexpected output: {output}");
        // The explicit class is used for the field and its guarded KCell, and no class is
        // generated or registered.
        assert!(
            output.contains("BrwLockPi < CustomBrwLockClass , RawBrwLockPi >"),
            "unexpected output: {output}"
        );
        assert!(
            output.contains("KCell < u32 , CustomBrwLockClass >"),
            "unexpected output: {output}"
        );
        assert!(
            !output.contains("ExplicitBrwLockClassLockClass")
                && !output.contains("EXPLICITBRWLOCKCLASS_LOCK_REGISTRATION")
                && !output.contains("rust_lock_classes"),
            "unexpected output: {output}"
        );

        // `BrwLockPi` fields must name their raw lock type (there is no default), and take no more
        // than one generic argument (the lock class is specified with `#[brwlock(LockClass)]`).
        for (attr, lock_ty, msg) in [
            (
                quote! { #[brwlock] },
                quote! { BrwLockPi },
                "BrwLockPi fields must specify the raw lock type explicitly",
            ),
            (
                quote! { #[brwlock(CustomBrwLockClass)] },
                quote! { BrwLockPi<CustomBrwLockClass, RawBrwLockPi> },
                "BrwLockPi expects exactly 1 generic argument for the raw lock type",
            ),
        ] {
            let err = guarded_impl(quote! {
                struct BadBrwLockGenericArgs {
                    #attr
                    lock: #lock_ty,
                    #[guarded_by(lock)]
                    val: u32,
                }
            })
            .to_string();
            assert!(
                err.contains("compile_error") && err.contains(msg),
                "unexpected output for {attr} {lock_ty}: {err}"
            );
        }
    }

    #[test]
    fn test_missing_raw_lock_type_compile_errors() {
        // `ksync` has no default raw lock type, so a bare `KMutex` field is rejected.
        let err = guarded_impl(quote! {
            struct BadBareKMutex {
                #[mutex]
                mu: KMutex,
                #[guarded_by(mu)]
                val: u32,
            }
        })
        .to_string();
        assert!(
            err.contains("compile_error")
                && err.contains("KMutex fields must specify the raw lock type explicitly"),
            "unexpected output: {err}"
        );

        // A non-`KMutex` field should report only the wrong-wrapper diagnostic, not the missing
        // raw lock type diagnostic on top of it.
        let err_non_kmutex = guarded_impl(quote! {
            struct BadNonKMutex {
                #[mutex]
                mu: KSpinlock,
                #[guarded_by(mu)]
                val: u32,
            }
        })
        .to_string();
        assert!(
            err_non_kmutex.contains("compile_error")
                && err_non_kmutex.contains("Mutex field must be of type KMutex")
                && !err_non_kmutex
                    .contains("KMutex fields must specify the raw lock type explicitly"),
            "unexpected output: {err_non_kmutex}"
        );

        let ok = guarded_impl(quote! {
            struct ExplicitKMutex {
                #[mutex]
                mu: KMutex<RawMutex>,
                #[guarded_by(mu)]
                val: u32,
            }
        })
        .to_string();
        assert!(!ok.contains("compile_error"), "unexpected error: {ok}");
        assert!(ok.contains("KMutex < ExplicitKMutexMuClass , RawMutex >"), "unexpected: {ok}");
    }

    #[test]
    fn test_declare_singleton_lock_flags_and_untracked() {
        let with_flags = declare_singleton_lock_impl(quote! {
            pub MyTrackedLock, RawSpinlock, cpu_align, flags = ::ksync::LOCK_FLAGS_NESTABLE
        })
        .to_string();
        assert!(!with_flags.contains("compile_error"), "unexpected error: {with_flags}");
        assert!(with_flags.contains("MYTRACKEDLOCK_REGISTRATION"), "unexpected: {with_flags}");
        assert!(
            with_flags.contains("| (:: ksync :: LOCK_FLAGS_NESTABLE)"),
            "missing extra flags: {with_flags}"
        );
        assert!(with_flags.contains("__KsyncCpuAligned"), "missing cpu_align: {with_flags}");

        let dup_flags = declare_singleton_lock_impl(quote! {
            MyDupFlagsLock,
            RawSpinlock,
            flags = ::ksync::LOCK_FLAGS_NESTABLE,
            flags = ::ksync::LOCK_FLAGS_MULTI_ACQUIRE,
        })
        .to_string();
        assert!(
            dup_flags.contains("compile_error")
                && dup_flags.contains("duplicate `flags` argument in `declare_singleton_lock!`"),
            "unexpected output: {dup_flags}"
        );

        let untracked = declare_singleton_lock_impl(quote! {
            IpiTaskLock, crate::kernel::spinlock::SpinLock, cpu_align_exclusive,
        })
        .to_string();
        assert!(!untracked.contains("compile_error"), "unexpected error: {untracked}");
        assert!(!untracked.contains("rust_lock_classes"), "unexpected registration: {untracked}");
        assert!(
            untracked.contains("ConstRawLock > :: INIT")
                && untracked.contains(".data.cpu_align_exclusive")
                && untracked.contains("null_mut ()"),
            "unexpected untracked expansion: {untracked}"
        );

        let untracked_with_flags = declare_singleton_lock_impl(quote! {
            BadUntrackedFlags,
            SpinLock,
            flags = ::ksync::LOCK_FLAGS_TRACKING_DISABLED,
        })
        .to_string();
        assert!(
            untracked_with_flags.contains("compile_error")
                && untracked_with_flags
                    .contains("untracked singleton locks cannot specify lockdep flags"),
            "unexpected output: {untracked_with_flags}"
        );
    }
}
