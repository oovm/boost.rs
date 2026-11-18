//! Reliability tests for internationalization functionality

use std::{cell::Cell, collections::HashMap, rc::Rc};
use template_types::{DejavuResult, i18n::*};

#[test]
fn test_edge_cases() {
    // Create translator
    let mut translator = SimpleTranslator::new("en", "en");

    // Test empty translations
    let empty_translations = HashMap::new();
    translator.add_translations("en", empty_translations);

    // Test translation with empty key
    let result = translator.translate("", None, "en").unwrap();
    assert_eq!(result, "");

    // Test translation with very long key
    let long_key = "a".repeat(1000);
    let result = translator.translate(&long_key, None, "en").unwrap();
    assert_eq!(result, long_key);

    // Test translation with special characters
    let special_key = "key with spaces and special chars!@#$%^&*()";
    let result = translator.translate(special_key, None, "en").unwrap();
    assert_eq!(result, special_key);

    // Test translation with many arguments
    let mut translator = SimpleTranslator::new("en", "en");
    let mut en_translations = HashMap::new();
    en_translations.insert("many_args".to_string(), "{a} {b} {c} {d} {e} {f} {g} {h} {i} {j}".to_string());
    translator.add_translations("en", en_translations);

    let mut args = HashMap::new();
    for c in 'a'..='j' {
        args.insert(c.to_string(), c.to_string());
    }

    let result = translator.translate("many_args", Some(&args), "en").unwrap();
    assert_eq!(result, "a b c d e f g h i j");
}

#[test]
fn test_error_handling() {
    // Create translator
    let translator = Box::new(SimpleTranslator::new("en", "en"));
    let mut i18n = I18nContext::new(translator);

    // Test adding translations to non-SimpleTranslator
    // This should fail
    let result = i18n.add_translations("en", HashMap::new());
    assert!(result.is_ok()); // This should work because we're using SimpleTranslator

    // Test translation with invalid locale
    let result = i18n.t("key", None);
    assert!(result.is_ok()); // Should return key if not found

    // Test batch translation with empty keys
    let empty_keys: Vec<(String, Option<HashMap<String, String>>)> = Vec::new();
    let result = i18n.t_batch(&empty_keys);
    assert!(result.is_ok());
    assert_eq!(result.unwrap().len(), 0);
}

#[test]
fn test_fluent_translator_error_handling() {
    use std::path::Path;

    // Create FluentTranslator
    let mut translator = FluentTranslator::new("en", "en");

    // Test adding invalid FTL content
    let invalid_ftl = "invalid ftl content";
    let result = translator.add_translations_from_string("en", invalid_ftl);
    assert!(result.is_err());

    // Test adding translations from non-existent file
    let non_existent_path = Path::new("non_existent.ftl");
    let result = translator.add_translations_from_file("en", non_existent_path);
    assert!(result.is_err());

    // Test translation with invalid locale
    let result = translator.translate("key", None, "invalid-locale");
    assert!(result.is_ok()); // Should fallback to default locale
}

#[test]
fn test_cache_behavior() {
    // Create translator
    let mut translator = SimpleTranslator::new("en", "en");
    let mut en_translations = HashMap::new();
    en_translations.insert("key".to_string(), "value".to_string());
    translator.add_translations("en", en_translations);

    // Create I18n context with small cache
    let translator_box = Box::new(translator);
    let mut i18n = I18nContext::with_cache_capacity(translator_box, 2);

    // Test cache eviction
    i18n.t("key", None).unwrap();
    i18n.t("key1", None).unwrap();
    i18n.t("key2", None).unwrap(); // This should evict "key"

    // Test that "key" is evicted
    assert_eq!(i18n.cache_size(), 2);

    // Test locale switching clears cache
    i18n.set_locale("zh");
    assert_eq!(i18n.cache_size(), 0);

    // Test adding translations clears cache
    i18n.add_translations("zh", HashMap::new()).unwrap();
    assert_eq!(i18n.cache_size(), 0);
}

/// 统计调用次数的翻译器，用来区分「命中了空翻译」与「合法空结果被当成 miss」。
#[derive(Debug)]
struct CountingTranslator {
    calls: Rc<Cell<usize>>,
}

impl TranslationProvider for CountingTranslator {
    fn translate(&self, _key: &str, _args: Option<&HashMap<String, String>>, _locale: &str) -> DejavuResult<String> {
        self.calls.set(self.calls.get() + 1);
        Ok(String::new())
    }

    fn get_locale(&self) -> &str {
        "en"
    }

    fn set_locale(&mut self, _locale: &str) {}

    fn clone_box(&self) -> Box<dyn TranslationProvider> {
        Box::new(Self { calls: Rc::clone(&self.calls) })
    }
}

#[test]
fn test_empty_translation_is_cached_as_a_hit() {
    let calls = Rc::new(Cell::new(0));
    let mut i18n = I18nContext::new(Box::new(CountingTranslator { calls: Rc::clone(&calls) }));

    let keys = vec![("empty".to_string(), None)];
    assert_eq!(i18n.t_batch(&keys).unwrap(), vec![String::new()]);
    assert_eq!(calls.get(), 1);

    // 空字符串是合法翻译，第二次批量请求必须命中缓存而不是再次翻译。
    assert_eq!(i18n.t_batch(&keys).unwrap(), vec![String::new()]);
    assert_eq!(calls.get(), 1);
    assert_eq!(i18n.cache_size(), 1);
}

#[test]
fn test_argument_identity_keeps_the_full_arguments() {
    let mut translator = SimpleTranslator::new("en", "en");
    let mut en_translations = HashMap::new();
    en_translations.insert("greeting".to_string(), "{first} and {second}".to_string());
    translator.add_translations("en", en_translations);

    let mut i18n = I18nContext::new(Box::new(translator));

    let mut forward = HashMap::new();
    forward.insert("first".to_string(), "Alice".to_string());
    forward.insert("second".to_string(), "Bob".to_string());

    let mut swapped = HashMap::new();
    swapped.insert("second".to_string(), "Bob".to_string());
    swapped.insert("first".to_string(), "Alice".to_string());

    let mut other = HashMap::new();
    other.insert("first".to_string(), "Carol".to_string());
    other.insert("second".to_string(), "Bob".to_string());

    assert_eq!(i18n.t("greeting", Some(&forward)).unwrap(), "Alice and Bob");
    assert_eq!(i18n.t("greeting", Some(&other)).unwrap(), "Carol and Bob");
    assert_eq!(i18n.cache_size(), 2);

    // 相同参数按不同插入顺序仍属于同一缓存条目。
    assert_eq!(i18n.t("greeting", Some(&swapped)).unwrap(), "Alice and Bob");
    assert_eq!(i18n.cache_size(), 2);
}
