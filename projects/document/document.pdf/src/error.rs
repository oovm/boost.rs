//! Failure type shared with the byte-level PDF engine, plus its frozen payload
//! contract.

pub use data_binary_pdf::error::{
    ErrorParams, ParamError, PdfError as ServiceError, Result, argument, codes, exists_error_at, invalid, params, read_error_at, unsupported,
    write_error_at,
};

#[cfg(test)]
mod contract {
    use std::path::Path;

    use serde_json::{Map, Value, json};

    use super::{
        params::{ErrorParams, ParamError},
        *,
    };

    fn frozen_samples() -> Vec<ErrorParams> {
        vec![
            ErrorParams::ArgumentInvalid {
                reason: "page 9 is outside the 3-page document".to_owned(),
                pages: Some("9".to_owned()),
                arg: None,
                with: None,
            },
            ErrorParams::ArgumentInvalid {
                reason: "`times` must be between 2 and 100".to_owned(),
                pages: None,
                arg: Some("times".to_owned()),
                with: None,
            },
            ErrorParams::ArgumentInvalid {
                reason: "the two selections overlap".to_owned(),
                pages: Some("1,2".to_owned()),
                arg: None,
                with: Some("2,3".to_owned()),
            },
            ErrorParams::Invalid { reason: "trailer has no /Root".to_owned() },
            ErrorParams::Unsupported { reason: "encrypted PDF is not supported".to_owned() },
            ErrorParams::Read { path: "input.pdf".to_owned(), kind: "NotFound".to_owned() },
            ErrorParams::Write { path: "output.pdf".to_owned() },
            ErrorParams::Exists { path: "output.pdf".to_owned() },
        ]
    }

    fn keys(value: &Value) -> Vec<String> {
        let mut keys: Vec<String> = value.as_object().map(|object: &Map<String, Value>| object.keys().cloned().collect()).unwrap_or_default();
        keys.sort();
        keys
    }

    #[test]
    fn every_frozen_payload_round_trips() {
        for sample in frozen_samples() {
            let payload = sample.to_json();
            let parsed = ErrorParams::parse(sample.code(), &payload).expect("a frozen payload parses");
            assert_eq!(parsed, sample, "{} must round-trip", sample.code());
            assert_eq!(parsed.code(), sample.code());
        }
    }

    #[test]
    fn every_code_constant_is_covered() {
        let covered: Vec<&str> = frozen_samples().iter().map(ErrorParams::code).collect();
        for code in
            [codes::ARGUMENT_INVALID, codes::FORMAT_INVALID, codes::FORMAT_UNSUPPORTED, codes::IO_READ, codes::IO_WRITE, codes::IO_EXISTS]
        {
            assert!(covered.contains(&code), "no frozen sample covers {code}");
        }
    }

    #[test]
    fn an_error_without_its_payload_is_refused() {
        let error = ServiceError::new(codes::IO_EXISTS, "output.pdf already exists");
        assert_eq!(error.frozen_params(), Err(ParamError::Unstructured(codes::IO_EXISTS.to_owned())));
    }

    #[test]
    fn path_aware_constructors_carry_frozen_payloads() {
        let read = read_error_at(Path::new("input.pdf"), "NotFound", "cannot read input.pdf");
        assert_eq!(read.frozen_params(), Ok(ErrorParams::Read { path: "input.pdf".to_owned(), kind: "NotFound".to_owned() }));
        let write = write_error_at(Path::new("output.pdf"), "cannot write output.pdf");
        assert_eq!(write.frozen_params(), Ok(ErrorParams::Write { path: "output.pdf".to_owned() }));
        let exists = exists_error_at(Path::new("output.pdf"), "output.pdf already exists");
        assert_eq!(exists.frozen_params(), Ok(ErrorParams::Exists { path: "output.pdf".to_owned() }));
    }

    #[test]
    fn message_only_helpers_still_freeze_their_reason() {
        for (error, expected) in [
            (
                argument("empty selection"),
                ErrorParams::ArgumentInvalid { reason: "empty selection".to_owned(), pages: None, arg: None, with: None },
            ),
            (invalid("trailer has no /Root"), ErrorParams::Invalid { reason: "trailer has no /Root".to_owned() }),
            (unsupported("encrypted PDF is not supported"), ErrorParams::Unsupported { reason: "encrypted PDF is not supported".to_owned() }),
        ] {
            assert_eq!(error.frozen_params(), Ok(expected));
        }
    }

    #[test]
    fn strict_parsing_refuses_bad_payloads() {
        assert_eq!(ErrorParams::parse("weird/code", &json!({})), Err(ParamError::UnknownCode("weird/code".to_owned())));
        assert_eq!(ErrorParams::parse(codes::IO_EXISTS, &json!("not an object")), Err(ParamError::NotAnObject));
        assert_eq!(ErrorParams::parse(codes::IO_READ, &json!({ "path": "input.pdf" })), Err(ParamError::Missing("kind")));
        assert_eq!(
            ErrorParams::parse(codes::IO_EXISTS, &json!({ "path": "output.pdf", "extra": 1 })),
            Err(ParamError::UnknownKey("extra".to_owned()))
        );
        assert_eq!(ErrorParams::parse(codes::IO_EXISTS, &json!({ "path": 5 })), Err(ParamError::WrongType("path")));
        assert_eq!(ErrorParams::parse(codes::ARGUMENT_INVALID, &json!({ "reason": "r", "pages": 5 })), Err(ParamError::WrongType("pages")));
    }

    #[test]
    fn rewriting_the_message_keeps_code_and_keys() {
        let original = argument("page 9 is outside the 3-page document");
        let rewritten = argument("page 9 lies beyond this file");
        assert_eq!(original.code(), rewritten.code(), "the code is contract, not prose");
        let original_params = original.params().expect("argument carries its reason").to_json();
        let rewritten_params = rewritten.params().expect("argument carries its reason").to_json();
        assert_eq!(keys(&original_params), keys(&rewritten_params), "rewording must not change the payload keys");
        assert_eq!(keys(&original_params), vec!["reason".to_owned()]);
    }
}
