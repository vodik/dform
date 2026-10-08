//! `#[derive(Resource)]` (dform-sdk, R-25): a resource type's schema facts
//! from its Rust fields, so a provider never writes them by hand.
//!
//! ```ignore
//! #[derive(Resource, Serialize, Deserialize)]
//! #[dform(type = "acme.bucket", replace = "destroy_first", retry = 3, lookup = "name")]
//! struct Bucket {
//!     #[dform(required, force_new)] name: String,
//!     #[dform(list_key = "name")] rules: Vec<Rule>,
//!     #[dform(computed, id)] id: Option<String>,
//!     #[dform(computed, sensitive)] key: Option<String>,
//! }
//! ```
//!
//! Each field is a `type_attr(TYPE, "field", TY, [FLAGS])`: TY from the
//! Rust type (`String` string, the integers int, `f64` number, `bool`
//! bool, `Vec` list, a map map, anything else obj; `Option<T>` is T's), or
//! `ty = ".."` (`ty = "ref(net.vpc)"`). Flags are the field's: `required`,
//! `computed`, `id`, `sensitive`, `force_new`, `optional_computed`,
//! `nullable`, `write_only` (R-106: the API takes it and never answers
//! it; state keeps its digest), `name_like`. `list_key = "a,b"` is a `type_list_key`; the struct's
//! `replace` a `type_replace`, its `retry` a `type_retry`, its `lookup =
//! "a,b"` a `type_lookup` (R-195: what a Create that timed out is found
//! by, each one of the struct's fields).

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, LitInt, LitStr, Type, parse_macro_input};

const FLAGS: [&str; 9] = [
    "required",
    "computed",
    "id",
    "sensitive",
    "force_new",
    "optional_computed",
    "nullable",
    "write_only",
    "name_like",
];

fn ty_of(t: &Type) -> String {
    let Type::Path(p) = t else {
        return "obj".into();
    };
    let Some(last) = p.path.segments.last() else {
        return "obj".into();
    };
    let name = last.ident.to_string();
    if name == "Option"
        && let syn::PathArguments::AngleBracketed(a) = &last.arguments
        && let Some(syn::GenericArgument::Type(inner)) = a.args.first()
    {
        return ty_of(inner);
    }
    match name.as_str() {
        "String" | "str" => "string",
        "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "usize" | "isize" => "int",
        "f32" | "f64" => "number",
        "bool" => "bool",
        "Vec" | "VecDeque" | "BTreeSet" | "HashSet" => "list",
        "BTreeMap" | "HashMap" => "map",
        _ => "obj",
    }
    .into()
}

/// `"x"` as a fact's string argument.
fn quoted(s: &str) -> String {
    format!("{s:?}")
}

#[proc_macro_derive(Resource, attributes(dform))]
pub fn derive_resource(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(&input) {
        Ok(t) => t.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let ident = &input.ident;
    let (mut typ, mut replace, mut retry) = (None::<String>, None::<String>, None::<u32>);
    let mut lookup = None::<LitStr>;
    for a in input.attrs.iter().filter(|a| a.path().is_ident("dform")) {
        a.parse_nested_meta(|m| {
            if m.path.is_ident("type") {
                typ = Some(m.value()?.parse::<LitStr>()?.value());
            } else if m.path.is_ident("replace") {
                let v = m.value()?.parse::<LitStr>()?.value();
                if !["create_first", "destroy_first", "either"].contains(&v.as_str()) {
                    return Err(m.error("replace is create_first, destroy_first or either"));
                }
                replace = Some(v);
            } else if m.path.is_ident("retry") {
                retry = Some(m.value()?.parse::<LitInt>()?.base10_parse()?);
            } else if m.path.is_ident("lookup") {
                lookup = Some(m.value()?.parse::<LitStr>()?);
            } else {
                return Err(m.error("expected type, replace, retry or lookup"));
            }
            Ok(())
        })?;
    }
    let typ = typ.ok_or_else(|| {
        syn::Error::new_spanned(ident, "#[dform(type = \"provider.type\")] names the type")
    })?;
    let Data::Struct(s) = &input.data else {
        return Err(syn::Error::new_spanned(ident, "a resource is a struct"));
    };
    let Fields::Named(fields) = &s.fields else {
        return Err(syn::Error::new_spanned(
            ident,
            "a resource's fields are named",
        ));
    };
    let mut lines = Vec::new();
    let mut names = Vec::new();
    for f in &fields.named {
        let name = f.ident.as_ref().expect("named").to_string();
        let name = name.strip_prefix("r#").unwrap_or(&name).to_string();
        names.push(name.clone());
        let mut flags = Vec::new();
        let mut ty = ty_of(&f.ty);
        let mut key = None;
        for a in f.attrs.iter().filter(|a| a.path().is_ident("dform")) {
            a.parse_nested_meta(|m| {
                if m.path.is_ident("ty") {
                    ty = m.value()?.parse::<LitStr>()?.value();
                } else if m.path.is_ident("list_key") {
                    key = Some(m.value()?.parse::<LitStr>()?.value());
                } else if let Some(flag) = FLAGS.iter().find(|f| m.path.is_ident(f)) {
                    flags.push(quoted(flag));
                } else {
                    return Err(m.error(format!(
                        "expected ty, list_key or a flag: {}",
                        FLAGS.join(", ")
                    )));
                }
                Ok(())
            })?;
        }
        lines.push(format!(
            "type_attr({}, {}, {}, [{}])",
            quoted(&typ),
            quoted(&name),
            quoted(&ty),
            flags.join(", ")
        ));
        if let Some(k) = key {
            let keys: Vec<String> = k.split(',').map(|x| quoted(x.trim())).collect();
            lines.push(format!(
                "type_list_key({}, {}, [{}])",
                quoted(&typ),
                quoted(&name),
                keys.join(", ")
            ));
        }
    }
    if let Some(r) = replace {
        lines.push(format!("type_replace({}, {})", quoted(&typ), quoted(&r)));
    }
    if let Some(n) = retry {
        lines.push(format!("type_retry({}, {n})", quoted(&typ)));
    }
    if let Some(l) = lookup {
        let attrs: Vec<String> = l.value().split(',').map(|x| x.trim().to_string()).collect();
        if let Some(a) = attrs.iter().find(|a| !names.contains(a)) {
            return Err(syn::Error::new_spanned(
                &l,
                format!("lookup: {a} is not a field of {ident}"),
            ));
        }
        let attrs: Vec<String> = attrs.iter().map(|a| quoted(a)).collect();
        lines.push(format!(
            "type_lookup({}, [{}])",
            quoted(&typ),
            attrs.join(", ")
        ));
    }
    let facts = lines.join("\n");
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    Ok(quote! {
        impl #impl_generics ::dform_sdk::Resource for #ident #ty_generics #where_clause {
            const TYPE: &'static str = #typ;
            const FACTS: &'static str = #facts;
        }
    })
}
