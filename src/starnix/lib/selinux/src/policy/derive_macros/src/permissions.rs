// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Procedural macros for declaring SELinux kernel object classes, class subsets, and permissions.

use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{Attribute, Ident, LitStr, Result, parse_macro_input};

/// Variant entry parsed from a macro body (`$(#[$attrs])* $ident $( ($arg) )?`).
///
/// Used by:
/// - `declare_kernel_classes!` and `class_permission_enum!` with `T = LitStr` for optional policy
///   name overrides (`Variant("custom_name")`).
/// - `subset_enum!` with `T = Ident` for optional sub-class types (`File(FileClass)`).
struct Variant<T> {
    attrs: Vec<Attribute>,
    ident: Ident,
    arg: Option<T>,
}

impl<T: Parse> Parse for Variant<T> {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        let ident: Ident = input.parse()?;
        let arg = if input.peek(syn::token::Paren) {
            let content;
            syn::parenthesized!(content in input);
            let value: T = content.parse()?;
            if !content.is_empty() {
                return Err(content.error("unexpected trailing tokens in variant argument"));
            }
            Some(value)
        } else {
            None
        };
        Ok(Self { attrs, ident, arg })
    }
}

impl Variant<LitStr> {
    /// Returns the explicit policy string literal if provided, or the `snake_case` form of `ident`.
    ///
    /// Rejects explicit overrides that are empty or match the default `snake_case` name.
    fn policy_name(&self) -> Result<LitStr> {
        let default_name = camel_to_snake(&self.ident.to_string());
        let Some(ref lit) = self.arg else {
            return Ok(LitStr::new(&default_name, self.ident.span()));
        };
        let value = lit.value();
        if value.is_empty() {
            return Err(syn::Error::new_spanned(lit, "policy name override must not be empty"));
        }
        if value == default_name {
            return Err(syn::Error::new_spanned(
                lit,
                format!("redundant policy name override `{value}` matches default snake_case name"),
            ));
        }
        Ok(lit.clone())
    }
}

/// Parses a braced, comma-separated list of [`Variant`] entries (`{ Variant, ... }`).
fn parse_braced_variants<T: Parse>(input: ParseStream<'_>) -> Result<Vec<Variant<T>>> {
    let content;
    syn::braced!(content in input);
    Ok(content.parse_terminated(Variant::<T>::parse, syn::Token![,])?.into_iter().collect())
}

/// Unpacks a slice of [`Variant`]s into parallel vectors of attributes and identifiers for `quote!`
/// repetition.
fn unpack_variants<T>(variants: &[Variant<T>]) -> (Vec<&[Attribute]>, Vec<&Ident>) {
    let attrs = variants.iter().map(|v| v.attrs.as_slice()).collect();
    let idents = variants.iter().map(|v| &v.ident).collect();
    (attrs, idents)
}

/// Returns the `<Class>Permission` type identifier corresponding to a kernel class `ident`.
fn permission_type_ident(class_ident: &Ident) -> Ident {
    Ident::new(&format!("{class_ident}Permission"), class_ident.span())
}

/// Derives the `Common<Prefix>Permission` type identifier for a `<Prefix>Class` subset type.
fn common_permission_ident(class_ty: &Ident) -> Result<Ident> {
    let class_str = class_ty.to_string();
    let Some(prefix) = class_str.strip_suffix("Class") else {
        return Err(syn::Error::new_spanned(
            class_ty,
            "subset class type name must end with `Class`",
        ));
    };
    Ok(Ident::new(&format!("Common{prefix}Permission"), class_ty.span()))
}

/// Returns the internal helper macro identifier used to propagate inherited permissions from
/// `perm_ty`.
fn inherit_macro_ident(perm_ty: &Ident) -> Ident {
    Ident::new(
        &format!("__selinux_inherit_{}", camel_to_snake(&perm_ty.to_string())),
        perm_ty.span(),
    )
}

/// Parsed input for the `declare_kernel_classes!` procedural macro.
struct DeclareKernelClassesInput {
    attrs: Vec<Attribute>,
    name: Ident,
    variants: Vec<Variant<LitStr>>,
}

impl Parse for DeclareKernelClassesInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        let name: Ident = input.parse()?;
        let variants = parse_braced_variants(input)?;
        Ok(Self { attrs, name, variants })
    }
}

/// Entry point for the `declare_kernel_classes!` procedural macro.
pub(crate) fn declare_kernel_classes_impl(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeclareKernelClassesInput);
    match gen_declare_kernel_classes(input) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// Generates the `KernelClass` enum and its policy name / permission table lookup methods.
fn gen_declare_kernel_classes(
    input: DeclareKernelClassesInput,
) -> Result<proc_macro2::TokenStream> {
    let attrs = &input.attrs;
    let name = &input.name;
    let variant_count = input.variants.len();
    let (variant_attrs, variant_idents) = unpack_variants(&input.variants);
    let variant_names =
        input.variants.iter().map(Variant::policy_name).collect::<Result<Vec<_>>>()?;
    let perm_tys: Vec<Ident> =
        input.variants.iter().map(|v| permission_type_ident(&v.ident)).collect();

    Ok(quote! {
        #(#attrs)*
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub enum #name {
            #(
                #(#variant_attrs)*
                #variant_idents,
            )*
        }

        impl #name {
            pub(super) const VARIANTS: &'static [Self; #variant_count] = &[
                #(Self::#variant_idents),*
            ];
            const NAMES: &'static [&'static str; #variant_count] = &[ #(#variant_names),* ];
            const PERMISSION_COUNTS: &'static [u8; #variant_count] = &[
                #(#perm_tys::COUNT),*
            ];
            const PERMISSION_NAMES: &'static [&'static [&'static str]; #variant_count] = &[
                #(#perm_tys::NAMES),*
            ];

            #[inline]
            pub const fn name(&self) -> &'static str {
                Self::NAMES[*self as usize]
            }

            /// Returns the [`KernelClass`] corresponding to the supplied policy class `name`,
            /// if any.
            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    #(#variant_names => Some(Self::#variant_idents),)*
                    _ => None,
                }
            }

            #[inline]
            pub(super) const fn permission_count(&self) -> u8 {
                Self::PERMISSION_COUNTS[*self as usize]
            }

            #[inline]
            pub(super) const fn permission_name(&self, id: u8) -> &'static str {
                Self::PERMISSION_NAMES[*self as usize][id as usize]
            }

            pub(super) fn permissions(
                &self,
            ) -> impl ExactSizeIterator<Item = KernelPermission> + use<> {
                let class = *self;
                (0..self.permission_count()).map(move |id| KernelPermission::new(class, id))
            }
        }
    })
}

/// Parsed input for the `subset_enum!` procedural macro.
struct SubsetEnumInput {
    attrs: Vec<Attribute>,
    name: Ident,
    common_permission: Ident,
    variants: Vec<Variant<Ident>>,
}

impl Parse for SubsetEnumInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        let name: Ident = input.parse()?;
        let common_permission = common_permission_ident(&name)?;
        let variants = parse_braced_variants(input)?;
        Ok(Self { attrs, name, common_permission, variants })
    }
}

/// Entry point for the `subset_enum!` procedural macro.
pub(crate) fn subset_enum_impl(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as SubsetEnumInput);
    match gen_subset_enum(input) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// Generates a subset class type (either a composite enum over sub-classes or a leaf wrapper
/// struct around `KernelClass`), compile-time permission inheritance assertions, and `ForClass`
/// implementations for its `Common<Prefix>Permission` type.
fn gen_subset_enum(input: SubsetEnumInput) -> Result<proc_macro2::TokenStream> {
    let name = &input.name;
    let common_permission = &input.common_permission;
    let is_composite = input.variants.iter().any(|v| v.arg.is_some());
    let (type_def, target_perm_tys) = if is_composite {
        gen_composite_subset_enum(&input)?
    } else {
        gen_leaf_subset_enum(&input)
    };

    Ok(quote! {
        #type_def

        const _: () = {
            #(
                let _ = #target_perm_tys::from as fn(#common_permission) -> _;
                let _ = PermissionSet::<#target_perm_tys>::from as fn(PermissionSet<#common_permission>) -> _;
            )*
        };

        impl<T: Into<#name>> ForClass<T> for #common_permission {
            type Output = KernelPermission;

            #[inline]
            fn for_class(&self, class: T) -> KernelPermission {
                KernelPermission::new(class.into().into(), self.0)
            }
        }

        impl<T: Into<#name>> ForClass<T> for PermissionSet<#common_permission> {
            type Output = KernelPermissionSet;

            #[inline]
            fn for_class(&self, class: T) -> KernelPermissionSet {
                KernelPermissionSet::new(class.into().into(), self.access_vector)
            }
        }
    })
}

/// Generates a composite subset enum whose variants wrap smaller subset class types
/// (e.g., `FsNodeClass { File(FileClass), Socket(SocketClass) }`).
fn gen_composite_subset_enum(
    input: &SubsetEnumInput,
) -> Result<(proc_macro2::TokenStream, Vec<Ident>)> {
    let attrs = &input.attrs;
    let name = &input.name;
    let (variant_attrs, variant_idents) = unpack_variants(&input.variants);

    let mut sub_class_tys = Vec::with_capacity(input.variants.len());
    let mut sub_common_perm_tys = Vec::with_capacity(input.variants.len());
    for v in &input.variants {
        let Some(ref sub_ty) = v.arg else {
            return Err(syn::Error::new_spanned(
                &v.ident,
                "cannot mix tuple and unit variants in `subset_enum!`",
            ));
        };
        sub_common_perm_tys.push(common_permission_ident(sub_ty)?);
        sub_class_tys.push(sub_ty);
    }

    let def = quote! {
        #(#attrs)*
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub enum #name {
            #(
                #(#variant_attrs)*
                #variant_idents(#sub_class_tys),
            )*
        }

        #(
            impl From<#sub_class_tys> for #name {
                #[inline]
                fn from(sub_class: #sub_class_tys) -> Self {
                    Self::#variant_idents(sub_class)
                }
            }
        )*

        impl From<#name> for KernelClass {
            #[inline]
            fn from(class: #name) -> Self {
                match class {
                    #(#name::#variant_idents(sub_class) => sub_class.into(),)*
                }
            }
        }
    };
    Ok((def, sub_common_perm_tys))
}

/// Generates a leaf subset class wrapper around `KernelClass` with associated constants for each
/// allowed variant and default `<Variant>Permission` type aliases to `Common<Prefix>Permission`.
fn gen_leaf_subset_enum(input: &SubsetEnumInput) -> (proc_macro2::TokenStream, Vec<Ident>) {
    let attrs = &input.attrs;
    let name = &input.name;
    let common_permission = &input.common_permission;
    let (variant_attrs, variant_idents) = unpack_variants(&input.variants);
    let perm_tys: Vec<Ident> =
        input.variants.iter().map(|v| permission_type_ident(&v.ident)).collect();
    let default_perms_mod = Ident::new(
        &format!("__default_{}_permissions", camel_to_snake(&name.to_string())),
        name.span(),
    );

    let def = quote! {
        // Provide fallback `<Variant>Permission = Common<Prefix>Permission` aliases in a child
        // module imported via wildcard; explicit `class_permission_enum!` definitions in the
        // enclosing module shadow these aliases for classes that add class-specific permissions.
        #[allow(dead_code)]
        mod #default_perms_mod {
            #(
                pub(super) type #perm_tys = super::#common_permission;
            )*
        }
        #[allow(unused_imports)]
        use #default_perms_mod::*;

        #(#attrs)*
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub struct #name(KernelClass);

        #[allow(non_upper_case_globals)]
        impl #name {
            #(
                #(#variant_attrs)*
                pub const #variant_idents: Self = Self(KernelClass::#variant_idents);
            )*
        }

        impl From<#name> for KernelClass {
            #[inline]
            fn from(other: #name) -> Self {
                other.0
            }
        }
    };
    (def, perm_tys)
}

/// Parsed input for the `class_permission_enum!` procedural macro.
struct ClassPermissionEnumInput {
    attrs: Vec<Attribute>,
    name: Ident,
    extends: Option<Ident>,
    kernel_class: Option<Ident>,
    variants: Vec<Variant<LitStr>>,
}

impl Parse for ClassPermissionEnumInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        let name: Ident = input.parse()?;
        let kernel_class = if input.peek(syn::Token![for]) {
            input.parse::<syn::Token![for]>()?;
            let kc: Ident = input.parse()?;
            let expected_name = permission_type_ident(&kc);
            if name != expected_name {
                return Err(syn::Error::new_spanned(
                    &name,
                    format!("expected permission type name `{expected_name}` for class `{kc}`"),
                ));
            }
            Some(kc)
        } else {
            None
        };
        let extends = if !input.peek(syn::token::Brace) {
            let kw: Ident = input.parse()?;
            if kw != "extends" {
                let expected = if kernel_class.is_some() {
                    "expected `extends <BasePermission>` or `{`"
                } else {
                    "expected `for <KernelClass>`, `extends <BasePermission>`, or `{`"
                };
                return Err(syn::Error::new_spanned(kw, expected));
            }
            Some(input.parse()?)
        } else {
            None
        };

        let variants = parse_braced_variants(input)?;
        if variants.is_empty() {
            return Err(syn::Error::new_spanned(
                &name,
                "permission definition must declare at least one permission",
            ));
        }

        Ok(Self { attrs, name, extends, kernel_class, variants })
    }
}

/// Entry point for the `class_permission_enum!` procedural macro.
pub(crate) fn class_permission_enum_impl(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as ClassPermissionEnumInput);
    match gen_class_permission_enum(input) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// Generates an opaque permission struct, `<Name>Set` alias, permission ID constants, `NAMES`
/// table, and conversions into [`PermissionSet`] / [`KernelPermission`].
fn gen_class_permission_enum(input: ClassPermissionEnumInput) -> Result<proc_macro2::TokenStream> {
    let attrs = &input.attrs;
    let name = &input.name;
    let own_count_usize = input.variants.len();
    if own_count_usize > 32 {
        return Err(syn::Error::new_spanned(name, "Too many permissions in definition"));
    }
    let own_count_u8 = own_count_usize as u8;

    let (variant_attrs, variant_idents) = unpack_variants(&input.variants);
    let variant_names =
        input.variants.iter().map(Variant::policy_name).collect::<Result<Vec<_>>>()?;
    let variant_offsets = 0..own_count_u8;
    let set_name = Ident::new(&format!("{name}Set"), name.span());

    let names_and_count_consts =
        gen_permission_names_and_counts(input.extends.as_ref(), &variant_names);
    let parent_inherit_invocation = input.extends.as_ref().map(|base| {
        let base_macro_name = inherit_macro_ident(base);
        quote! { #base_macro_name!(#name); }
    });
    let class_or_common_items =
        gen_class_or_common_permission_items(&input, &variant_attrs, &variant_idents);

    Ok(quote! {
        #(#attrs)*
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub struct #name(u8);

        pub type #set_name = PermissionSet<#name>;

        #[allow(non_upper_case_globals)]
        impl #name {
            #names_and_count_consts

            #(
                #(#variant_attrs)*
                pub const #variant_idents: Self = Self(Self::BASE_ID + #variant_offsets);
            )*

            #[inline]
            pub fn as_access_vector(&self) -> AccessVector {
                AccessVector::from(1u32 << self.0)
            }
        }

        const _: () = {
            assert!((#name::COUNT as usize) <= 32);
        };

        impl From<#name> for PermissionSet<#name> {
            #[inline]
            fn from(permission: #name) -> Self {
                Self {
                    access_vector: permission.as_access_vector(),
                    _marker: std::marker::PhantomData,
                }
            }
        }

        impl<T: Into<PermissionSet<#name>>> BitOr<T> for #name {
            type Output = PermissionSet<#name>;

            #[inline]
            fn bitor(self, other: T) -> PermissionSet<#name> {
                PermissionSet::from(self) | other
            }
        }

        #parent_inherit_invocation
        #class_or_common_items
    })
}

/// Generates `BASE_ID`, `COUNT`, and `NAMES` associated constants for a permission type.
fn gen_permission_names_and_counts(
    extends: Option<&Ident>,
    variant_names: &[LitStr],
) -> proc_macro2::TokenStream {
    let own_count_usize = variant_names.len();
    let own_count_u8 = own_count_usize as u8;
    let Some(base) = extends else {
        return quote! {
            const BASE_ID: u8 = 0;
            const COUNT: u8 = #own_count_u8;
            const NAMES: &'static [&'static str] = &[ #(#variant_names),* ];
        };
    };

    quote! {
        const BASE_ID: u8 = #base::COUNT;
        const COUNT: u8 = #base::COUNT + #own_count_u8;
        // Concatenate `base::NAMES` and this type's `OWN_NAMES` at compile time so `NAMES`
        // is indexed directly by 0-based permission ID.
        const NAMES: &'static [&'static str] = &{
            const OWN_NAMES: [&str; #own_count_usize] = [ #(#variant_names),* ];
            let mut names = [""; Self::COUNT as usize];
            let mut i = 0;
            while i < #base::NAMES.len() {
                names[i] = #base::NAMES[i];
                i += 1;
            }
            let mut j = 0;
            while j < OWN_NAMES.len() {
                names[i + j] = OWN_NAMES[j];
                j += 1;
            }
            names
        };
    }
}

/// Generates either the `KernelPermission` / `KernelPermissionSet` conversions for a class-affine
/// permission type (`for <KernelClass>`), or the `__selinux_inherit_*` helper macro for a common
/// permission base type.
fn gen_class_or_common_permission_items(
    input: &ClassPermissionEnumInput,
    variant_attrs: &[&[Attribute]],
    variant_idents: &[&Ident],
) -> proc_macro2::TokenStream {
    let name = &input.name;
    if let Some(ref kc) = input.kernel_class {
        quote! {
            impl From<#name> for KernelPermission {
                #[inline]
                fn from(permission: #name) -> Self {
                    KernelPermission::new(KernelClass::#kc, permission.0)
                }
            }

            impl From<#name> for KernelPermissionSet {
                #[inline]
                fn from(permission: #name) -> Self {
                    KernelPermission::from(permission).into()
                }
            }

            impl From<PermissionSet<#name>> for KernelPermissionSet {
                #[inline]
                fn from(permissions: PermissionSet<#name>) -> Self {
                    Self::new(KernelClass::#kc, permissions.access_vector)
                }
            }
        }
    } else {
        let inherit_macro_name = inherit_macro_ident(name);
        let parent_macro_call = input.extends.as_ref().map(|base| {
            let base_macro_name = inherit_macro_ident(base);
            quote! { #base_macro_name!($target); }
        });
        quote! {
            // Emit a declarative helper macro that derived permission types invoke to copy
            // this base type's permission constants and `From` conversions into their scope.
            #[allow(unused_macros)]
            macro_rules! #inherit_macro_name {
                ($target:ty) => {
                    #parent_macro_call

                    #[allow(non_upper_case_globals)]
                    impl $target {
                        #(
                            #(#variant_attrs)*
                            pub const #variant_idents: Self = Self(#name::#variant_idents.0);
                        )*
                    }

                    impl From<#name> for $target {
                        #[inline]
                        fn from(other: #name) -> Self {
                            Self(other.0)
                        }
                    }

                    impl From<#name> for PermissionSet<$target> {
                        #[inline]
                        fn from(other: #name) -> Self {
                            PermissionSet::<#name>::from(other).into()
                        }
                    }

                    impl From<PermissionSet<#name>> for PermissionSet<$target> {
                        #[inline]
                        fn from(other: PermissionSet<#name>) -> Self {
                            Self {
                                access_vector: other.access_vector,
                                _marker: std::marker::PhantomData,
                            }
                        }
                    }
                };
            }
        }
    }
}

/// Converts a `CamelCase` identifier string into `snake_case`.
fn camel_to_snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}
