//! Procedural macros for `boost-harness`.

#![deny(missing_docs)]

mod diagnostic;
mod test;

use proc_macro::TokenStream;
use syn::parse_macro_input;
use test::RuntimePath;

fn expand_with_runtime(
    attribute: TokenStream,
    item: TokenStream,
    expand: fn(proc_macro2::TokenStream, syn::ItemFn, RuntimePath) -> syn::Result<proc_macro2::TokenStream>,
    default_runtime: RuntimePath,
) -> TokenStream {
    let item = parse_macro_input!(item as syn::ItemFn);
    let attribute: proc_macro2::TokenStream = attribute.into();
    expand(attribute, item, default_runtime).unwrap_or_else(|error| error.to_compile_error()).into()
}

/// Wraps a free function as a standard `#[test]` entry with a structured harness descriptor.
///
/// Generated code references `::boost_harness::test` by default. Use the facade re-export
/// `boost::test::test` or `runtime = ::boost::test` when depending only on `boost`.
#[proc_macro_attribute]
pub fn test(attribute: TokenStream, item: TokenStream) -> TokenStream {
    expand_with_runtime(attribute, item, test::expand_test, RuntimePath::direct())
}

/// Facade entry that expands against `::boost::test`.
#[proc_macro_attribute]
pub fn facade_test(attribute: TokenStream, item: TokenStream) -> TokenStream {
    expand_with_runtime(attribute, item, test::expand_test, RuntimePath::facade())
}

/// Declares a typed fixture from a setup function and an optional teardown path.
#[proc_macro_attribute]
pub fn fixture(attribute: TokenStream, item: TokenStream) -> TokenStream {
    expand_with_runtime(attribute, item, test::expand_fixture, RuntimePath::direct())
}

/// Facade entry that expands against `::boost::test`.
#[proc_macro_attribute]
pub fn facade_fixture(attribute: TokenStream, item: TokenStream) -> TokenStream {
    expand_with_runtime(attribute, item, test::expand_fixture, RuntimePath::facade())
}

/// Expands one parameterized body into a standard `#[test]` entry per declared case.
#[proc_macro_attribute]
pub fn case(attribute: TokenStream, item: TokenStream) -> TokenStream {
    expand_with_runtime(attribute, item, test::expand_case, RuntimePath::direct())
}

/// Facade entry that expands against `::boost::test`.
#[proc_macro_attribute]
pub fn facade_case(attribute: TokenStream, item: TokenStream) -> TokenStream {
    expand_with_runtime(attribute, item, test::expand_case, RuntimePath::facade())
}

/// Build a structured `boost_harness::diagnostic::Diagnostic` record at the call site.
///
/// ```ignore
/// use boost_harness_macro::diagnose;
///
/// let record = diagnose! {
///     code: "valkyrie.syntax.unexpected-token",
///     severity: error,
///     message: "unexpected token",
/// };
/// ```
#[proc_macro]
pub fn diagnose(input: TokenStream) -> TokenStream {
    let parsed = parse_macro_input!(input as diagnostic::DiagnosticInput);
    diagnostic::expand(parsed).into()
}
