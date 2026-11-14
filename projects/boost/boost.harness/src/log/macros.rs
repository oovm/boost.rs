/// Emit a structured log event.
#[cfg(feature = "std")]
#[macro_export]
macro_rules! event {
    ($level:ident, $target:expr) => {
        $crate::log::emit($crate::log::LogEvent::log($crate::log::Level::$level, $target).build())
    };
    ($level:ident, $target:expr, $($key:ident = $value:expr),+ $(,)?) => {
        $crate::log::emit({
            let mut builder = $crate::log::LogEvent::log($crate::log::Level::$level, $target);
            $(builder = builder.field(stringify!($key), $crate::log::field_value!($value));)*
            builder.build()
        })
    };
}

/// Create a log span.
#[macro_export]
macro_rules! span {
    ($target:expr) => {
        $crate::log::LogSpan::new($target)
    };
    ($target:expr, $($key:ident = $value:expr),+ $(,)?) => {
        {
            let mut span = $crate::log::LogSpan::new($target);
            $(span = span.field(stringify!($key), $crate::log::field_value!($value));)*
            span
        }
    };
}

/// Convert a value into [`FieldValue`] while keeping its type when it has one.
///
/// Booleans, integers, floats, strings, characters, and [`FieldValue`] values keep their type.
/// Any other value must be wrapped with [`FieldValue::debug`], so a type logger cannot represent
/// fails to compile instead of turning into a `Debug` string.
#[macro_export]
macro_rules! field_value {
    ($value:expr) => {
        $crate::log::FieldValue::from_value($value)
    };
}
