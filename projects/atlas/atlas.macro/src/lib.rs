//! Atlas 编译期生成层。
//!
//! 只提供一个入口 [`Component`]：识别字段上的 `#[wire]`，生成只含普通字段的输入类型、
//! 依赖描述与 `atlas-wire` 的构造实现。宏只生成对运行时合同的调用，不把容器算法藏进展开
//! 代码，也不对运行时 crate 建立普通依赖。
//!
//! ```ignore
//! use std::sync::Arc;
//!
//! struct Primary;
//!
//! #[derive(atlas::Component)]
//! #[atlas(input = OrderServiceInput, runtime = atlas::wire)]
//! struct OrderService {
//!     region: String,
//!     #[wire]
//!     repository: Arc<dyn OrderRepository + Send + Sync>,
//!     #[wire(key = Primary)]
//!     model: Arc<ModelClient>,
//! }
//! ```

#![deny(missing_docs)]

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::{
    Attribute, Data, DeriveInput, Field, Fields, GenericArgument, Ident, LitBool, LitStr, Meta, Path, PathArguments, Result, Type,
    parse_macro_input, spanned::Spanned,
};

/// 为组件生成普通输入类型与 `atlas-wire` 构造实现。
///
/// 结构体级参数只有 `input = Ident` 与 `runtime = Path`，两者都必填。`runtime` 指向实际导入
/// 的运行时路径，支持 Cargo 依赖重命名与门面重导出。
#[proc_macro_derive(Component, attributes(wire, atlas))]
pub fn component(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(input).unwrap_or_else(|error| error.to_compile_error()).into()
}

struct Config {
    input: Ident,
    runtime: Path,
}

struct WireField {
    ident: Ident,
    ty: Type,
    key: Option<Path>,
    optional: Option<Type>,
}

fn expand(input: DeriveInput) -> Result<proc_macro2::TokenStream> {
    let config = parse_config(&input.attrs)?;
    if !input.generics.params.is_empty() || input.generics.where_clause.is_some() {
        return Err(syn::Error::new(input.generics.span(), "`Component` 首版只支持无泛型、无生命周期参数、无 `where` 子句的结构体"));
    }
    reject_packed(&input.attrs)?;
    let Data::Struct(data) = &input.data
    else {
        return Err(syn::Error::new(input.span(), "`Component` 只能派生在具名结构体上"));
    };
    let Fields::Named(fields) = &data.fields
    else {
        return Err(syn::Error::new(data.fields.span(), "`Component` 首版只支持具名结构体，tuple struct 与 unit struct 暂不支持"));
    };

    let mut plain_fields = Vec::new();
    let mut wire_fields = Vec::new();
    for field in &fields.named {
        match parse_field(field)? {
            Some(spec) => wire_fields.push(spec),
            None => plain_fields.push((field.ident.clone().expect("具名字段"), field.ty.clone())),
        }
    }

    let name = &input.ident;
    let visibility = &input.vis;
    let input_name = &config.input;
    let runtime = &config.runtime;

    let input_definition = if plain_fields.is_empty() {
        quote! {
            #[doc = "由 `Component` derive 生成的构造输入；本组件没有普通字段。"]
            #visibility struct #input_name;
        }
    }
    else {
        let declarations = plain_fields.iter().map(|(ident, ty)| quote! { #visibility #ident: #ty });
        quote! {
            #[doc = "由 `Component` derive 生成的构造输入；只包含非 `#[wire]` 字段。"]
            #visibility struct #input_name {
                #(#declarations,)*
            }
        }
    };

    let mut dependencies = Vec::new();
    let mut assignments = Vec::new();
    for (ident, _) in &plain_fields {
        assignments.push(quote! { #ident: __atlas_input.#ident });
    }
    for field in &wire_fields {
        let ident = &field.ident;
        let ty = &field.ty;
        let label = LitStr::new(&ident.to_string(), ident.span());
        let optional = LitBool::new(field.optional.is_some(), ident.span());
        let key = match &field.key {
            Some(key) => quote! { #runtime::TypeKey::qualified::<#ty, #key>() },
            None => quote! { #runtime::TypeKey::of::<#ty>() },
        };
        dependencies.push(quote! {
            #runtime::Dependency::new(#key, #optional, #label)
        });
        let qualifier = match &field.key {
            Some(key) => quote! { ::core::option::Option::Some(::core::any::TypeId::of::<#key>()) },
            None => quote! { ::core::option::Option::None },
        };
        match &field.optional {
            Some(inner) => assignments.push(quote! {
                #ident: __atlas_resolver.resolve_optional::<#inner>(#qualifier)?
            }),
            None => assignments.push(quote! {
                #ident: __atlas_resolver.resolve::<#ty>(#qualifier)?
            }),
        }
    }

    Ok(quote! {
        #input_definition

        #[automatically_derived]
        impl #runtime::Component for #name {
            type Input = #input_name;

            fn dependencies() -> ::std::vec::Vec<#runtime::Dependency> {
                ::std::vec![#(#dependencies,)*]
            }

            fn construct(
                __atlas_input: Self::Input,
                __atlas_resolver: &mut #runtime::Resolver<'_>,
            ) -> ::core::result::Result<Self, #runtime::WireError> {
                ::core::result::Result::Ok(Self {
                    #(#assignments,)*
                })
            }
        }
    })
}

fn parse_config(attributes: &[Attribute]) -> Result<Config> {
    let mut input = None;
    let mut runtime = None;
    for attribute in attributes {
        if !attribute.path().is_ident("atlas") {
            continue;
        }
        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("input") {
                let value = meta.value()?;
                let ident: Ident = value.parse()?;
                if input.replace(ident).is_some() {
                    return Err(meta.error("`input` 重复声明"));
                }
                Ok(())
            }
            else if meta.path.is_ident("runtime") {
                let value = meta.value()?;
                let path: Path = value.parse()?;
                if runtime.replace(path).is_some() {
                    return Err(meta.error("`runtime` 重复声明"));
                }
                Ok(())
            }
            else {
                Err(meta.error("未知的 `atlas` 参数，只支持 `input = Ident` 与 `runtime = Path`"))
            }
        })?;
    }
    let Some(input) = input
    else {
        return Err(syn::Error::new(Span::call_site(), "缺少 `#[atlas(input = …, runtime = …)]`：`input` 必填"));
    };
    let Some(runtime) = runtime
    else {
        return Err(syn::Error::new(Span::call_site(), "缺少 `#[atlas(input = …, runtime = …)]`：`runtime` 必填"));
    };
    Ok(Config { input, runtime })
}

fn reject_packed(attributes: &[Attribute]) -> Result<()> {
    for attribute in attributes {
        if !attribute.path().is_ident("repr") {
            continue;
        }
        let mut packed = None;
        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("packed") {
                packed = Some(meta.path.span());
            }
            Ok(())
        })?;
        if let Some(span) = packed {
            return Err(syn::Error::new(span, "`Component` 不支持 `repr(packed)` 结构体"));
        }
    }
    Ok(())
}

fn parse_field(field: &Field) -> Result<Option<WireField>> {
    for attribute in &field.attrs {
        if attribute.path().is_ident("cfg") || attribute.path().is_ident("cfg_attr") {
            return Err(syn::Error::new(attribute.span(), "`Component` 首版不支持字段级 `cfg`/`cfg_attr`"));
        }
    }
    let Some(ident) = field.ident.clone()
    else {
        return Err(syn::Error::new(field.span(), "`Component` 只支持具名字段"));
    };
    let mut declared = None::<Span>;
    let mut key = None;
    let mut optional = false;
    for attribute in &field.attrs {
        if !attribute.path().is_ident("wire") {
            continue;
        }
        if declared.replace(attribute.span()).is_some() {
            return Err(syn::Error::new(attribute.span(), "字段只能出现一次 `#[wire]`"));
        }
        match &attribute.meta {
            Meta::Path(_) => {}
            Meta::List(_) => attribute.parse_nested_meta(|meta| {
                if meta.path.is_ident("key") {
                    let value = meta.value()?;
                    let path: Path = value.parse()?;
                    if key.replace(path).is_some() {
                        return Err(meta.error("`key` 重复声明"));
                    }
                    Ok(())
                }
                else if meta.path.is_ident("optional") {
                    if optional {
                        return Err(meta.error("`optional` 重复声明"));
                    }
                    optional = true;
                    Ok(())
                }
                else {
                    Err(meta.error("`#[wire]` 只支持 `key = Path` 与 `optional`"))
                }
            })?,
            Meta::NameValue(_) => {
                return Err(syn::Error::new(attribute.span(), "`#[wire]` 不接受赋值形式，请使用 `#[wire(key = …)]`"));
            }
        }
    }
    if declared.is_none() {
        return Ok(None);
    }
    let ty = field.ty.clone();
    let optional = if optional {
        let Some(inner) = option_inner(&ty)
        else {
            return Err(syn::Error::new(
                ty.span(),
                "`#[wire(optional)]` 的字段类型必须是 `Option<T>`、`std::option::Option<T>` 或 `core::option::Option<T>`",
            ));
        };
        Some(inner.clone())
    }
    else {
        None
    };
    Ok(Some(WireField { ident, ty, key, optional }))
}

/// 识别 `Option<T>` 的三种可接受写法；类型别名不在此列。
fn option_inner(ty: &Type) -> Option<&Type> {
    let Type::Path(type_path) = ty
    else {
        return None;
    };
    if type_path.qself.is_some() {
        return None;
    }
    let segments: Vec<String> = type_path.path.segments.iter().map(|segment| segment.ident.to_string()).collect();
    let matches_option = match segments.as_slice() {
        [name] => name == "Option",
        [module, name] => module == "option" && name == "Option",
        [root, module, name] => (root == "std" || root == "core") && module == "option" && name == "Option",
        _ => false,
    };
    if !matches_option {
        return None;
    }
    let PathArguments::AngleBracketed(arguments) = &type_path.path.segments.last()?.arguments
    else {
        return None;
    };
    if arguments.args.len() != 1 {
        return None;
    }
    match arguments.args.first()? {
        GenericArgument::Type(inner) => Some(inner),
        _ => None,
    }
}
