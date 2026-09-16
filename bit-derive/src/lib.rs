use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Data, DataEnum, DataStruct, DeriveInput, Fields, Ident, Lit, Meta, Type, parse_macro_input};

/// Derive the `Schema` and `SchemaType` traits.
///
/// Supports:
/// - Structs with named fields: produces `Schema` (a `StructType`) and a
///   `SchemaType` that wraps it as `Type::Struct`.
/// - Enums (typically `#[serde(untagged)]`): produces `SchemaType` as a
///   `Type::Union` of each variant's schema. `Schema` is not implemented
///   for enums. Variants may be:
///     - unit (no fields) — currently unsupported
///     - newtype (one unnamed field) — uses the field's schema type
///     - struct variants (named fields) — produces a `Type::Struct`
///
/// Field-level support:
/// - Doc comments as field/struct descriptions
/// - `#[serde(flatten)]` to inline fields from another `Schema` type
/// - `#[serde(rename = "...")]` to rename a field in the schema
/// - `#[serde(default)]` / `#[serde(default = "...")]` makes collection
///   and `bool` types optional
/// - `Option<T>`, `Vec<T>`, `HashMap<_, V>`, `bool`, `String`, numerics;
///   any other named type is resolved via its `SchemaType` impl
/// - `#[schema(description = "...")]` to override the description
#[proc_macro_derive(Schema, attributes(schema, serde))]
pub fn derive_schema(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    match &input.data {
        Data::Struct(data) => derive_schema_struct(name, &input.attrs, data),
        Data::Enum(data) => derive_schema_enum(name, data),
        Data::Union(_) => syn::Error::new_spanned(name, "Schema cannot be derived for unions")
            .to_compile_error()
            .into(),
    }
}

fn derive_schema_struct(name: &Ident, attrs: &[syn::Attribute], data: &DataStruct) -> TokenStream {
    let struct_doc = extract_doc_comment(attrs);

    let Fields::Named(fields) = &data.fields else {
        return syn::Error::new_spanned(name, "Schema requires named fields")
            .to_compile_error()
            .into();
    };

    let mut field_exprs = Vec::new();

    for field in &fields.named {
        let field_name = field.ident.as_ref().unwrap();

        // #[serde(flatten)] — inline the inner type's schema fields.
        if has_serde_attr(&field.attrs, "flatten") {
            let ty = &field.ty;
            field_exprs.push(quote! {
                fields.extend(<#ty as crate::schema::Schema>::schema().fields);
            });
            continue;
        }

        // "depends_on" / "after" are engine-level and aren't part of the
        // provider's schema.
        let name_str = field_name.to_string();
        if name_str == "depends_on" || name_str == "after" {
            continue;
        }

        let schema_name = get_serde_rename(&field.attrs).unwrap_or_else(|| name_str.clone());
        let desc_expr = description_expr(&field.attrs);
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

        impl crate::schema::SchemaType for #name {
            fn schema_type() -> crate::value::Type {
                crate::value::Type::Struct(<Self as crate::schema::Schema>::schema())
            }
        }
    };

    expanded.into()
}

fn derive_schema_enum(name: &Ident, data: &DataEnum) -> TokenStream {
    let mut variant_exprs: Vec<TokenStream2> = Vec::new();

    for variant in &data.variants {
        let type_expr = match &variant.fields {
            Fields::Unit => {
                return syn::Error::new_spanned(variant, "Schema: unit enum variants are not supported")
                    .to_compile_error()
                    .into();
            }
            Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
                // Newtype variant: delegate to the inner type's schema.
                let ty = &fields.unnamed.first().unwrap().ty;
                rust_type_to_schema_type(ty, false)
            }
            Fields::Unnamed(_) => {
                return syn::Error::new_spanned(
                    variant,
                    "Schema: tuple variants with multiple fields are not supported",
                )
                .to_compile_error()
                .into();
            }
            Fields::Named(fields) => {
                let inner_exprs: Vec<TokenStream2> = fields
                    .named
                    .iter()
                    .map(|f| {
                        let field_name = f.ident.as_ref().unwrap().to_string();
                        let field_name = get_serde_rename(&f.attrs).unwrap_or(field_name);
                        let desc_expr = description_expr(&f.attrs);
                        let has_default = has_serde_attr(&f.attrs, "default");
                        let typ = rust_type_to_schema_type(&f.ty, has_default);
                        quote! {
                            fields.push((
                                #field_name.into(),
                                crate::value::StructField {
                                    typ: #typ,
                                    default: None,
                                    description: #desc_expr,
                                },
                            ));
                        }
                    })
                    .collect();
                let variant_desc_expr = match extract_doc_comment(&variant.attrs) {
                    Some(d) => quote! { Some(#d.into()) },
                    None => quote! { None },
                };
                quote! {
                    {
                        let mut fields = Vec::new();
                        #(#inner_exprs)*
                        crate::value::Type::Struct(crate::value::StructType {
                            description: #variant_desc_expr,
                            fields,
                        })
                    }
                }
            }
        };
        variant_exprs.push(type_expr);
    }

    let expanded = quote! {
        impl crate::schema::SchemaType for #name {
            fn schema_type() -> crate::value::Type {
                crate::value::Type::Union(vec![
                    #(#variant_exprs),*
                ])
            }
        }
    };

    expanded.into()
}

fn description_expr(attrs: &[syn::Attribute]) -> TokenStream2 {
    let doc = extract_doc_comment(attrs);
    let desc = get_schema_description(attrs).or(doc);
    match desc {
        Some(d) => quote! { Some(#d.into()) },
        None => quote! { None },
    }
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
fn rust_type_to_schema_type(ty: &Type, has_default: bool) -> TokenStream2 {
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
                "Duration" => quote! { crate::value::Type::Duration },
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
                // Unknown named type — delegate to its SchemaType impl.
                _ => quote! { <#ty as crate::schema::SchemaType>::schema_type() },
            }
        }
        // Unknown complex type shape — delegate to its SchemaType impl.
        _ => quote! { <#ty as crate::schema::SchemaType>::schema_type() },
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
