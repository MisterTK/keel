//! `#[keel::wrap]` — the Rust front end's one-attribute wrapping promise
//! (architecture-spec.md §5.3, dx-spec.md invariant 1).
//!
//! v1 scope (deliberately narrow, see `crates/keel/src/lib.rs`'s crate docs
//! and the session gap brief for the full descoping rationale):
//!
//! * The target is **explicit only**: `#[keel::wrap(target = "...")]`.
//!   Signature/host inference was judged too fiddly for a free function (no
//!   receiver to read a base URL off, unlike a Python/Node HTTP client
//!   object) to be worth building for v1; an explicit string is one token
//!   more than the ideal but still a single attribute, and it is exactly
//!   the string the same policy key in `keel.toml` uses, so it stays
//!   copy-pasteable and greppable.
//! * The wrapped function must be a free (no receiver) `async fn` returning
//!   `Result<T, E>` where `T: Serialize + DeserializeOwned` (round-tripped
//!   through the engine's `Value` payload — required for the *cache* path,
//!   where no "live" Rust value exists because the effect never ran) and
//!   `E: std::error::Error + Send + Sync + 'static`.
//! * Every parameter type must implement `Clone`: a retried call re-invokes
//!   the wrapped body, and the macro cannot move the caller's arguments more
//!   than once, so it clones them fresh for each attempt.
//! * At compile time, the macro walks up from `CARGO_MANIFEST_DIR` looking
//!   for a `keel.toml` (mirroring how `Cargo.toml`/`tsconfig.json` discovery
//!   works). If found, it must be valid TOML (a hard compile error
//!   otherwise — this is the main compile-time value the brief asks for:
//!   catching a broken policy file before you ever run the binary). If the
//!   file has a non-empty `[target]` table and the given `target` string
//!   does not appear in it (and no glob-style key containing `*` is
//!   present, which might match it at runtime), the macro emits a
//!   `compile_error!` naming the closest thing it *did* find — this catches
//!   the common typo case (`#[keel::wrap(target = "stripe")]` vs a
//!   `[target."stripe-api"]` table) statically. The runtime policy is still
//!   (re)loaded and applied by `keel::init`/the lazy engine init — this is a
//!   best-effort static lint layered on top, not a cache of the policy.
use std::path::{Path, PathBuf};

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    Expr, ExprLit, FnArg, GenericArgument, ItemFn, Lit, MetaNameValue, Pat, PathArguments,
    ReturnType, Token, Type,
    parse::{Parse, ParseStream},
    parse_macro_input,
    punctuated::Punctuated,
    spanned::Spanned,
};

/// `#[keel::wrap(target = "...", idempotent = true)]`.
#[proc_macro_attribute]
pub fn wrap(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as WrapArgs);
    let input = parse_macro_input!(item as ItemFn);
    match expand(&args, &input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

struct WrapArgs {
    target: syn::LitStr,
    idempotent: bool,
}

impl Parse for WrapArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if input.is_empty() {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "#[keel::wrap] requires `target = \"...\"`, e.g. \
                 #[keel::wrap(target = \"orders-api\")] (explicit-target-only in v1 — \
                 see crates/keel-macros/src/lib.rs for why signature inference is deferred)",
            ));
        }
        let pairs = Punctuated::<MetaNameValue, Token![,]>::parse_terminated(input)?;
        let mut target = None;
        let mut idempotent = false;
        for pair in pairs {
            let Some(key) = pair.path.get_ident().map(ToString::to_string) else {
                return Err(syn::Error::new_spanned(
                    &pair.path,
                    "expected a simple identifier (`target` or `idempotent`)",
                ));
            };
            match key.as_str() {
                "target" => target = Some(expect_litstr(&pair.value)?),
                "idempotent" => idempotent = expect_litbool(&pair.value)?,
                other => {
                    return Err(syn::Error::new_spanned(
                        &pair.path,
                        format!(
                            "unknown #[keel::wrap] argument `{other}`; expected `target` or `idempotent`"
                        ),
                    ));
                }
            }
        }
        let target = target.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "#[keel::wrap] requires `target = \"...\"`",
            )
        })?;
        Ok(WrapArgs { target, idempotent })
    }
}

fn expect_litstr(expr: &Expr) -> syn::Result<syn::LitStr> {
    match expr {
        Expr::Lit(ExprLit {
            lit: Lit::Str(s), ..
        }) => Ok(s.clone()),
        other => Err(syn::Error::new_spanned(other, "expected a string literal")),
    }
}

fn expect_litbool(expr: &Expr) -> syn::Result<bool> {
    match expr {
        Expr::Lit(ExprLit {
            lit: Lit::Bool(b), ..
        }) => Ok(b.value),
        other => Err(syn::Error::new_spanned(other, "expected `true` or `false`")),
    }
}

fn expand(args: &WrapArgs, input: &ItemFn) -> syn::Result<TokenStream2> {
    if input.sig.asyncness.is_none() {
        return Err(syn::Error::new(
            input.sig.fn_token.span(),
            "#[keel::wrap] only supports `async fn` (v1 scope: the engine chain is driven on \
             the caller's async runtime; wrap a sync body in `tokio::task::spawn_blocking` \
             yourself first if needed)",
        ));
    }
    if input.sig.receiver().is_some() {
        return Err(syn::Error::new(
            input.sig.fn_token.span(),
            "#[keel::wrap] only supports free functions in v1, not methods with a `self` \
             receiver — pull the body out into a free function and call it from the method",
        ));
    }
    if !input.sig.generics.params.is_empty() || input.sig.generics.where_clause.is_some() {
        return Err(syn::Error::new(
            input.sig.generics.span(),
            "#[keel::wrap] does not support generic functions in v1",
        ));
    }

    let (ok_ty, err_ty) = parse_result_return(&input.sig.output)?;
    validate_manifest_policy(args)?;

    let vis = &input.vis;
    let attrs = &input.attrs;
    let name = &input.sig.ident;
    let inner_name = format_ident!("__keel_inner_{}", name);
    let inputs = &input.sig.inputs;
    let output = &input.sig.output;
    let body = &input.block;
    let target_lit = &args.target;
    let idempotent_lit = args.idempotent;

    let mut param_idents = Vec::with_capacity(inputs.len());
    for arg in inputs {
        let FnArg::Typed(pat_ty) = arg else {
            unreachable!("receiver already rejected above")
        };
        let Pat::Ident(pat_ident) = pat_ty.pat.as_ref() else {
            return Err(syn::Error::new_spanned(
                &pat_ty.pat,
                "#[keel::wrap] requires simple identifier parameters in v1 (no destructuring \
                 patterns) — each parameter is cloned once per retry attempt",
            ));
        };
        param_idents.push(pat_ident.ident.clone());
    }
    let ref_idents: Vec<_> = param_idents
        .iter()
        .map(|p| format_ident!("__keel_ref_{}", p))
        .collect();

    let op_expr = quote! { concat!(module_path!(), "::", stringify!(#name)) };

    Ok(quote! {
        #[doc(hidden)]
        #(#attrs)*
        async fn #inner_name(#inputs) #output {
            #body
        }

        #(#attrs)*
        #vis async fn #name(#inputs) -> ::std::result::Result<#ok_ty, ::keel::Error<#err_ty>>
        where
            #ok_ty: ::keel::__private::serde::Serialize
                + ::keel::__private::serde::de::DeserializeOwned
                + ::std::marker::Send
                + 'static,
            #err_ty: ::std::error::Error + ::std::marker::Send + ::std::marker::Sync + 'static,
        {
            #( let #ref_idents = &#param_idents; )*
            ::keel::__private::wrap_call(
                #target_lit,
                #op_expr,
                #idempotent_lit,
                move || {
                    #( let #param_idents = ::std::clone::Clone::clone(#ref_idents); )*
                    async move { #inner_name(#(#param_idents),*).await }
                },
            )
            .await
        }
    })
}

fn parse_result_return(output: &ReturnType) -> syn::Result<(Type, Type)> {
    let ReturnType::Type(_, ty) = output else {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[keel::wrap] requires a `-> Result<T, E>` return type",
        ));
    };
    let Type::Path(type_path) = ty.as_ref() else {
        return Err(syn::Error::new_spanned(
            ty,
            "#[keel::wrap] requires a `-> Result<T, E>` return type",
        ));
    };
    let Some(last) = type_path.path.segments.last() else {
        return Err(syn::Error::new_spanned(ty, "malformed return type"));
    };
    if last.ident != "Result" {
        return Err(syn::Error::new_spanned(
            &last.ident,
            "#[keel::wrap] requires a `-> Result<T, E>` return type",
        ));
    }
    let PathArguments::AngleBracketed(generics) = &last.arguments else {
        return Err(syn::Error::new_spanned(
            last,
            "#[keel::wrap]'s `Result` must be written with explicit `<T, E>` type arguments \
             (a bare `Result` alias cannot be inspected at macro-expansion time)",
        ));
    };
    let mut types = generics.args.iter().filter_map(|a| match a {
        GenericArgument::Type(t) => Some(t.clone()),
        _ => None,
    });
    let (Some(ok_ty), Some(err_ty)) = (types.next(), types.next()) else {
        return Err(syn::Error::new_spanned(
            generics,
            "#[keel::wrap]'s `Result<T, E>` must specify both type arguments",
        ));
    };
    Ok((ok_ty, err_ty))
}

/// Best-effort compile-time policy check (module docs). Never fails the
/// build just because there is no `keel.toml` — Level 0 defaults apply at
/// runtime the same way they do for the Python/Node front ends.
fn validate_manifest_policy(args: &WrapArgs) -> syn::Result<()> {
    let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") else {
        return Ok(());
    };
    let Some(path) = find_keel_toml(Path::new(&manifest_dir)) else {
        return Ok(());
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let parsed: toml::Value = toml::from_str(&contents).map_err(|err| {
        syn::Error::new(
            proc_macro2::Span::call_site(),
            format!(
                "keel.toml at {} is not valid TOML: {err} (fix it, or delete it to fall back \
                 to Level 0 defaults)",
                path.display()
            ),
        )
    })?;
    let Some(table) = parsed.get("target").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    if table.is_empty() {
        return Ok(());
    }
    let target = args.target.value();
    if table.contains_key(&target) {
        return Ok(());
    }
    if table.keys().any(|k| k.contains('*')) {
        // A glob-style pattern key is present; it might match this target at
        // runtime (targeting.md), and the macro does not implement glob
        // matching — do not false-positive.
        return Ok(());
    }
    let known: Vec<_> = table.keys().map(String::as_str).collect();
    Err(syn::Error::new(
        args.target.span(),
        format!(
            "target \"{target}\" is not a key in {}'s [target] table, and no glob-style key \
             (containing `*`) is present that could match it at runtime. Known target keys: \
             [{}]. If this is intentional (e.g. the target falls back to [defaults.outbound]), \
             add an explicit `[target.\"{target}\"]` table (even an empty one) to keel.toml to \
             silence this check.",
            path.display(),
            known.join(", "),
        ),
    ))
}

fn find_keel_toml(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join("keel.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}
