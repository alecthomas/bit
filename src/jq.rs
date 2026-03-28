//! Thin wrapper around jaq for applying jq expressions to JSON.

use crate::provider::BoxError;

/// Apply a jq expression to a JSON string, returning the first output as a JSON string.
pub fn transform(expression: &str, input: &str) -> Result<String, BoxError> {
    use jaq_core::data::JustLut;
    use jaq_core::load::{Arena, File, Loader};
    use jaq_core::{Compiler, Ctx, Vars};

    let input_val: jaq_json::Val =
        jaq_json::read::parse_single(input.as_bytes()).map_err(|e| format!("invalid JSON input: {e}"))?;

    let program = File {
        code: expression,
        path: (),
    };

    let defs = jaq_core::defs().chain(jaq_std::defs()).chain(jaq_json::defs());
    let funs = jaq_core::funs().chain(jaq_std::funs()).chain(jaq_json::funs());

    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = loader
        .load(&arena, program)
        .map_err(|errs| format!("jq compile error: {errs:?}"))?;
    let filter = Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|errs| format!("jq compile error: {errs:?}"))?;

    let ctx: Ctx<JustLut<jaq_json::Val>> = Ctx::new(&filter.lut, Vars::new([]));
    let mut outputs = filter.id.run((ctx, input_val));

    let first: jaq_json::Val = outputs
        .next()
        .ok_or("jq expression produced no output")?
        .map_err(|e| format!("jq runtime error: {e:?}"))?;

    Ok(first.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity() {
        let result = transform(".", r#"{"a": 1}"#).unwrap();
        assert_eq!(result, r#"{"a":1}"#);
    }

    #[test]
    fn select_field() {
        let result = transform(".name", r#"{"name": "test", "value": 42}"#).unwrap();
        assert_eq!(result, r#""test""#);
    }

    #[test]
    fn array_map() {
        let result = transform("[.[] | . * 2]", "[1, 2, 3]").unwrap();
        assert_eq!(result, "[2,4,6]");
    }

    #[test]
    fn construct_object() {
        let result = transform(
            r#"{name: .n, status: (if .ok then "passed" else "failed" end)}"#,
            r#"{"n": "test1", "ok": true}"#,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["name"], "test1");
        assert_eq!(parsed["status"], "passed");
    }

    #[test]
    fn invalid_expression() {
        assert!(transform(".[invalid", "{}").is_err());
    }

    #[test]
    fn invalid_json() {
        assert!(transform(".", "not json").is_err());
    }
}
