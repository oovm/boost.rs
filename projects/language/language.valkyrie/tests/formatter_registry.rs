//! 装配链验证：注册 provider 后，平台按扩展名分派到与直接调用相同的 Valkyrie formatter。

use language_format::formatter::{FormatOptions, format_source_for_extension, source_registry};
use language_valkyrie::compiler::services::format_provider::register_format_providers;

const SOURCE: &str = "micro f(){a<=>b}\n";

#[test]
fn extension_dispatch_reaches_the_valkyrie_formatter() {
    register_format_providers();

    let registry = source_registry().read().expect("registry");
    for extension in ["v", "vx", "valkyrie"] {
        assert!(registry.supports_extension(extension), "extension={extension}");
    }
    drop(registry);

    let options = FormatOptions::default();
    let direct = language_valkyrie::formatter::format_source(SOURCE, &Default::default()).expect("direct format");
    let via_registry = format_source_for_extension("v", SOURCE, &options).expect("registry format");
    assert_eq!(via_registry.trim_end(), direct.trim_end());
    assert_eq!(format_source_for_extension("v", &via_registry, &options).expect("idempotent"), via_registry);
}
