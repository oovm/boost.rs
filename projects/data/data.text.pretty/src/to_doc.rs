use std::{boxed::Box, string::String, vec::Vec};

use crate::Document;

/// A trait for types that can be converted to a document for pretty printing.
///
/// The associated [`AsDocument::Config`] type carries formatting parameters and
/// defaults to `()` when a type needs none.
///
/// # Example
/// ```rust
/// use data_text_pretty::{AsDocument, Doc};
///
/// struct MyType {
///     value: i32,
/// }
///
/// #[derive(Default)]
/// struct MyTypeParams {
///     indent: usize,
/// }
///
/// impl AsDocument for MyType {
///     type Config = MyTypeParams;
///
///     fn as_document(&self, config: &Self::Config) -> Doc<'_> {
///         Doc::text(format!("MyType({}) with indent {}", self.value, config.indent))
///     }
/// }
///
/// let my_type = MyType { value: 42 };
/// let params = MyTypeParams { indent: 2 };
/// let doc = my_type.as_document(&params);
/// ```
pub trait AsDocument {
    /// The type of parameters used for formatting, `()` when unspecified.
    type Config = ();

    /// Converts this type to a document for pretty printing.
    fn as_document(&self, config: &Self::Config) -> Document<'_>;
}

/// A trait for types that can be converted to a document value, potentially consuming the input.
pub trait ToDocument<'a> {
    /// Converts this type to a document value.
    fn to_document(self) -> Document<'a>;
}

impl AsDocument for String {
    fn as_document(&self, _config: &Self::Config) -> Document<'_> {
        Document::Text(self.as_str().into())
    }
}

impl AsDocument for str {
    fn as_document(&self, _config: &Self::Config) -> Document<'_> {
        Document::Text(self.into())
    }
}

impl<'a> AsDocument for Document<'a> {
    fn as_document(&self, _config: &Self::Config) -> Document<'_> {
        self.clone()
    }
}

impl<T: AsDocument> AsDocument for Vec<T> {
    type Config = T::Config;

    fn as_document(&self, config: &Self::Config) -> Document<'_> {
        Document::Concat(self.iter().map(|t| t.as_document(config)).collect())
    }
}

impl<T: AsDocument> AsDocument for Option<T> {
    type Config = T::Config;

    fn as_document(&self, config: &Self::Config) -> Document<'_> {
        match self {
            Some(t) => t.as_document(config),
            None => Document::Nil,
        }
    }
}

impl<T: AsDocument + ?Sized> AsDocument for &T {
    type Config = T::Config;

    fn as_document(&self, config: &Self::Config) -> Document<'_> {
        (**self).as_document(config)
    }
}

impl<T: AsDocument + ?Sized> AsDocument for Box<T> {
    type Config = T::Config;

    fn as_document(&self, config: &Self::Config) -> Document<'_> {
        self.as_ref().as_document(config)
    }
}

impl<'a> ToDocument<'a> for Document<'a> {
    fn to_document(self) -> Document<'a> {
        self
    }
}

impl<'a, T: AsDocument + ?Sized> ToDocument<'a> for &'a T
where
    T::Config: Default,
{
    fn to_document(self) -> Document<'a> {
        self.as_document(&T::Config::default())
    }
}

impl<'a> ToDocument<'a> for String {
    fn to_document(self) -> Document<'a> {
        Document::Text(self.into())
    }
}
