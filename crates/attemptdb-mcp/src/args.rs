//! Typed access to `tools/call` arguments. Every accessor names the argument
//! and the expected type in its error so the caller can correct the call.

use serde_json::{Map, Value};

pub type ArgResult<T> = std::result::Result<T, String>;

/// Optional string argument; an empty string counts as absent.
pub fn opt_string(args: &Map<String, Value>, key: &str) -> ArgResult<Option<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim().to_string()).filter(|s| !s.is_empty())),
        Some(other) => Err(format!(
            "argument {key:?} must be a string, got {}",
            type_name(other)
        )),
    }
}

/// Required string argument.
pub fn req_string(args: &Map<String, Value>, key: &str) -> ArgResult<String> {
    opt_string(args, key)?.ok_or_else(|| format!("argument {key:?} is required"))
}

pub fn opt_bool(args: &Map<String, Value>, key: &str) -> ArgResult<Option<bool>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("true") => Ok(Some(true)),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("false") => Ok(Some(false)),
        Some(other) => Err(format!(
            "argument {key:?} must be a boolean, got {}",
            type_name(other)
        )),
    }
}

/// Optional non-negative integer (numeric strings are accepted).
pub fn opt_usize(args: &Map<String, Value>, key: &str) -> ArgResult<Option<usize>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(|v| Some(v as usize))
            .ok_or_else(|| format!("argument {key:?} must be a non-negative integer")),
        Some(Value::String(s)) => s
            .trim()
            .parse::<usize>()
            .map(Some)
            .map_err(|_| format!("argument {key:?} must be a non-negative integer, got {s:?}")),
        Some(other) => Err(format!(
            "argument {key:?} must be an integer, got {}",
            type_name(other)
        )),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accessors() {
        let m = json!({"a": "x", "b": true, "c": 3, "d": "", "e": [1]});
        let m = m.as_object().unwrap();
        assert_eq!(opt_string(m, "a").unwrap().as_deref(), Some("x"));
        assert_eq!(opt_string(m, "d").unwrap(), None);
        assert_eq!(opt_string(m, "zz").unwrap(), None);
        assert!(opt_string(m, "b").is_err());
        assert_eq!(opt_bool(m, "b").unwrap(), Some(true));
        assert!(opt_bool(m, "c").is_err());
        assert_eq!(opt_usize(m, "c").unwrap(), Some(3));
        assert!(opt_usize(m, "e").is_err());
        assert!(req_string(m, "zz").is_err());
    }
}
