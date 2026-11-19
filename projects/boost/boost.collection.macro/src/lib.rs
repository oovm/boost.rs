//! Procedural macros for `boost-collection` inline fixed-shape array types.
//!
//! Stable Rust cannot take a tuple as a const generic argument, so the tuple
//! spelling `FixedArray2D<f32, (3, 3)>` is not a valid type. [`array!`] and
//! [`facade_array!`] split that tuple into the const parameters of
//! `FixedArray2D` or `FixedArray3D` and expand to a plain type.

#![warn(missing_docs)]

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    Expr, Token, Type, parenthesized,
    parse::{Parse, ParseStream},
    parse_macro_input,
};

/// Expands a fixed-shape array type from a tuple or flat extent list.
///
/// ```ignore
/// use boost_collection::array;
///
/// type Grid = array!(f32, (3, 3));
/// type Cube = array!(f32, (2, 3, 4));
/// type Inline = array!(u8, 2, 4);
/// ```
///
/// Two extents expand to `::boost_collection::FixedArray2D<Element, A, B>` and
/// three expand to `::boost_collection::FixedArray3D<Element, A, B, C>`.
/// Extents may be any const expression, not only literals.
#[proc_macro]
pub fn array(input: TokenStream) -> TokenStream {
    expand(parse_macro_input!(input as ArrayInput), quote! { ::boost_collection })
}

/// Facade entry that expands against `::boost::collection`.
///
/// Use this spelling when the consumer depends only on the `boost` facade:
///
/// ```ignore
/// use boost::collection::array;
///
/// type Grid = array!(f32, (3, 3));
/// ```
#[proc_macro]
pub fn facade_array(input: TokenStream) -> TokenStream {
    expand(parse_macro_input!(input as ArrayInput), quote! { ::boost::collection })
}

/// `Element, (A, B[, C])` or `Element, A, B[, C]`.
struct ArrayInput {
    element: Type,
    extents: Vec<Expr>,
}

impl Parse for ArrayInput {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let element: Type = input.parse()?;
        input.parse::<Token![,]>()?;
        let extents = if input.peek(syn::token::Paren) {
            let content;
            parenthesized!(content in input);
            parse_extents(&content)?
        }
        else {
            parse_extents(input)?
        };
        if !input.is_empty() {
            return Err(input.error("expected `Element, (A, B, C)` or `Element, A, B, C`"));
        }
        Ok(Self { element, extents })
    }
}

fn parse_extents(input: ParseStream<'_>) -> syn::Result<Vec<Expr>> {
    let mut extents = Vec::new();
    while !input.is_empty() {
        extents.push(input.parse()?);
        if input.is_empty() {
            break;
        }
        input.parse::<Token![,]>()?;
    }
    if !(2..=3).contains(&extents.len()) {
        return Err(input.error("expected two or three extents"));
    }
    Ok(extents)
}

fn expand(ArrayInput { element, extents }: ArrayInput, root: TokenStream2) -> TokenStream {
    let [first, second] = [&extents[0], &extents[1]];
    match extents.get(2) {
        None => quote! { #root::FixedArray2D<#element, { #first }, { #second }> }.into(),
        Some(third) => quote! { #root::FixedArray3D<#element, { #first }, { #second }, { #third }> }.into(),
    }
}
