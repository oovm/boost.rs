#![feature(new_range_api)]
use core::range::Range;
use data_text_core::source::ToSource;
use data_text_json::ast::*;

#[test]
fn test_json_to_source() {
    let root = JsonRoot {
        value: JsonValueNode::Object(JsonObject {
            fields: vec![
                JsonField {
                    name: JsonString { value: "name".to_string(), span: Range { start: 0, end: 0 } },
                    value: JsonValueNode::String(JsonString { value: "oak".to_string(), span: Range { start: 0, end: 0 } }),
                    span: Range { start: 0, end: 0 },
                },
                JsonField {
                    name: JsonString { value: "version".to_string(), span: Range { start: 0, end: 0 } },
                    value: JsonValueNode::Number(JsonNumber { value: 1.0, span: Range { start: 0, end: 0 } }),
                    span: Range { start: 0, end: 0 },
                },
            ],
            span: Range { start: 0, end: 0 },
        }),
    };

    let source = root.to_source_string();
    assert_eq!(source, "{\"name\":\"oak\",\"version\":1}");
}
#[test]
fn test_json_to_doc() {
    use data_text_pretty::{AsDocument, PrinterConfig};

    let root = JsonRoot {
        value: JsonValueNode::Object(JsonObject {
            fields: vec![JsonField {
                name: JsonString { value: "name".to_string(), span: Range { start: 0, end: 0 } },
                value: JsonValueNode::String(JsonString { value: "oak".to_string(), span: Range { start: 0, end: 0 } }),
                span: Range { start: 0, end: 0 },
            }],
            span: Range { start: 0, end: 0 },
        }),
    };

    let doc = root.value.as_document(&());
    let formatted = doc.render_with_config(PrinterConfig::default());
    assert!(formatted.contains("\"name\""));
    assert!(formatted.contains("\"oak\""));
}
