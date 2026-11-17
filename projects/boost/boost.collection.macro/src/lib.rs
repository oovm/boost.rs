//! Procedural macros for `boost-collection` fixed-shape array types.
//!
//! Stable Rust cannot take a tuple as a const generic argument, so the tuple
//! spelling `Array2D<f32, (3, 3)>` is not a valid type. [`array!`] and
//! [`facade_array!`] split that tuple into the two const parameters of
//! `Array2D` and expand to a plain type.

#![deny(missing_docs)]

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    Expr, Token, Type, parenthesized,
    parse::{Parse, ParseStream},
    parse_macro_input,
};

/// Expands a fixed-shape array type from a tuple or flat extent pair.
///
/// ```ignore
/// use boost_collection::array;
///
/// type Grid = array!(f32, (3, 3));
/// type Inline = array!(u8, 2, 4);
/// ```
///
/// Both spellings expand to `::boost_collection::Array2D<Element, ROWS, COLUMNS>`.
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

/// `Element, (ROWS, COLUMNS)` or `Element, ROWS, COLUMNS`.
struct ArrayInput {
    element: Type,
    rows: Expr,
    columns: Expr,
}

impl Parse for ArrayInput {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let element: Type = input.parse()?;
        input.parse::<Token![,]>()?;
        let (rows, columns) = if input.peek(syn::token::Paren) {
            let content;
            parenthesized!(content in input);
            let rows: Expr = content.parse()?;
            content.parse::<Token![,]>()?;
            let columns: Expr = content.parse()?;
            (rows, columns)
        }
        else {
            let rows: Expr = input.parse()?;
            input.parse::<Token![,]>()?;
            let columns: Expr = input.parse()?;
            (rows, columns)
        };
        if !input.is_empty() {
            return Err(input.error("expected `Element, (ROWS, COLUMNS)` or `Element, ROWS, COLUMNS`"));
        }
        Ok(Self { element, rows, columns })
    }
}

fn expand(ArrayInput { element, rows, columns }: ArrayInput, root: TokenStream2) -> TokenStream {
    quote! { #root::Array2D<#element, { #rows }, { #columns }> }.into()
}
