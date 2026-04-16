use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, Lit, Meta, Type, parse_macro_input};

/// Derive the `Schema` trait for a struct, generating a `StructType` from its fields.
///
/// Supports:
/// - Doc comments as field/struct descriptions
/// - `#[serde(flatten)]` to inline fields from another `Schema` type
/// - `Option<T>`, `Vec<T>`, `bool`, `String`, numeric types
/// - `#[schema(description = "...")]` to override the description
#[proc_macro_derive(Schema, attributes(schema, serde))]
pub fn derive_schema(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let struct_doc = extract_doc_comment(&input.attrs);

    let Data::Struct(data) = &input.data else {
        return syn::Error::new_spanned(&input.ident, "Schema can only be derived for structs")
            .to_compile_error()
            .into();
    };

    let Fields::Named(fields) = &data.fields else {
        return syn::Error::new_spanned(&input.ident, "Schema requires named fields")
            .to_compile_error()
            .into();
    };

    let mut field_exprs = Vec::new();

    for field in &fields.named {
        let field_name = field.ident.as_ref().unwrap();

        // Check for #[serde(flatten)] — inline the inner type's schema fields.
        if has_serde_attr(&field.attrs, "flatten") {
            let ty = &field.ty;
            field_exprs.push(quote! {
                fields.extend(<#ty as crate::schema::Schema>::schema().fields);
            });
            continue;
        }

        // Skip fields named "depends_on" or "after" — these are engine-level.
        let name_str = field_name.to_string();
        if name_str == "depends_on" || name_str == "after" {
            continue;
        }

        // Use serde rename if present, otherwise the field name.
        let schema_name = get_serde_rename(&field.attrs).unwrap_or_else(|| name_str.clone());
        let doc = extract_doc_comment(&field.attrs);
        let schema_desc = get_schema_description(&field.attrs).or(doc);

        let desc_expr = match &schema_desc {
            Some(d) => quote! { Some(#d.into()) },
            None => quote! { None },
        };

        let has_default = has_serde_attr(&field.attrs, "default");
        let type_expr = rust_type_to_schema_type(&field.ty, has_default);

        field_exprs.push(quote! {
            fields.push((
                #schema_name.into(),
                crate::value::StructField {
                    typ: #type_expr,
                    default: None,
                    description: #desc_expr,
                },
            ));
        });
    }

    let struct_desc_expr = match &struct_doc {
        Some(d) => quote! { Some(#d.into()) },
        None => quote! { None },
    };

    let expanded = quote! {
        impl crate::schema::Schema for #name {
            fn schema() -> crate::value::StructType {
                let mut fields = Vec::new();
                #(#field_exprs)*
                crate::value::StructType {
                    description: #struct_desc_expr,
                    fields,
                }
            }
        }
    };

    expanded.into()
}

/// Extract the combined doc comment from `#[doc = "..."]` attributes.
fn extract_doc_comment(attrs: &[syn::Attribute]) -> Option<String> {
    let lines: Vec<String> = attrs
        .iter()
        .filter_map(|attr| {
            if !attr.path().is_ident("doc") {
                return None;
            }
            if let Meta::NameValue(nv) = &attr.meta
                && let syn::Expr::Lit(lit) = &nv.value
                && let Lit::Str(s) = &lit.lit
            {
                return Some(s.value().trim().to_owned());
            }
            None
        })
        .collect();

    if lines.is_empty() { None } else { Some(lines.join(" ")) }
}

/// Check if a field has `#[serde(X)]` where X matches the given name.
fn has_serde_attr(attrs: &[syn::Attribute], name: &str) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("serde") {
            return false;
        }
        let mut found = false;
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident(name) {
                found = true;
            }
            Ok(())
        });
        found
    })
}

/// Get `#[serde(rename = "...")]` value if present.
fn get_serde_rename(attrs: &[syn::Attribute]) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        let mut rename = None;
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                let value = meta.value()?;
                let lit: Lit = value.parse()?;
                if let Lit::Str(s) = lit {
                    rename = Some(s.value());
                }
            }
            Ok(())
        });
        if rename.is_some() {
            return rename;
        }
    }
    None
}

/// Get `#[schema(description = "...")]` value if present.
fn get_schema_description(attrs: &[syn::Attribute]) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("schema") {
            continue;
        }
        let mut desc = None;
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("description") {
                let value = meta.value()?;
                let lit: Lit = value.parse()?;
                if let Lit::Str(s) = lit {
                    desc = Some(s.value());
                }
            }
            Ok(())
        });
        if desc.is_some() {
            return desc;
        }
    }
    None
}

/// Map a Rust type to a `crate::value::Type` token expression.
/// `has_default` indicates `#[serde(default)]` — Vec/HashMap/bool become optional.
fn rust_type_to_schema_type(ty: &Type, has_default: bool) -> proc_macro2::TokenStream {
    match ty {
        Type::Path(tp) => {
            let seg = tp.path.segments.last().unwrap();
            let ident = seg.ident.to_string();

            match ident.as_str() {
                "String" => quote! { crate::value::Type::String },
                "bool" => {
                    if has_default {
                        quote! { crate::value::Type::Optional(Box::new(crate::value::Type::Bool)) }
                    } else {
                        quote! { crate::value::Type::Bool }
                    }
                }
                "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "f32" | "f64" | "usize" | "isize" => {
                    quote! { crate::value::Type::Number }
                }
                "Option" => {
                    let inner = extract_generic_arg(seg);
                    let inner_expr = rust_type_to_schema_type(&inner, false);
                    quote! { crate::value::Type::Optional(Box::new(#inner_expr)) }
                }
                "Vec" => {
                    let inner = extract_generic_arg(seg);
                    let inner_expr = rust_type_to_schema_type(&inner, false);
                    let list = quote! { crate::value::Type::List(Box::new(#inner_expr)) };
                    if has_default {
                        quote! { crate::value::Type::Optional(Box::new(#list)) }
                    } else {
                        list
                    }
                }
                "HashMap" => {
                    let inner = extract_second_generic_arg(seg);
                    let inner_expr = rust_type_to_schema_type(&inner, false);
                    let map = quote! { crate::value::Type::Map(Box::new(#inner_expr)) };
                    if has_default {
                        quote! { crate::value::Type::Optional(Box::new(#map)) }
                    } else {
                        map
                    }
                }
                _ => quote! { crate::value::Type::String },
            }
        }
        _ => quote! { crate::value::Type::String },
    }
}

/// Extract the first generic type argument (e.g. `Option<T>` -> `T`).
fn extract_generic_arg(seg: &syn::PathSegment) -> Type {
    if let syn::PathArguments::AngleBracketed(args) = &seg.arguments
        && let Some(syn::GenericArgument::Type(ty)) = args.args.first()
    {
        return ty.clone();
    }
    syn::parse_quote!(String)
}

/// Extract the second generic type argument (e.g. `HashMap<K, V>` -> `V`).
fn extract_second_generic_arg(seg: &syn::PathSegment) -> Type {
    if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
        let mut iter = args.args.iter();
        iter.next();
        if let Some(syn::GenericArgument::Type(ty)) = iter.next() {
            return ty.clone();
        }
    }
    syn::parse_quote!(String)
}
