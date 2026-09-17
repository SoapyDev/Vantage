use std::{collections::HashMap, sync::OnceLock};

use serde_json::Value;

/// A placeholder that [`Injector`] could not resolve against a dictionary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    /// `{{name}}` (or a `${name}` name segment) references a variable that is
    /// absent from the current scope.
    UnknownVariable { name: String },
    /// `{{variable/pointer}}` where `variable` exists but the JSON pointer does
    /// not resolve inside it.
    UnknownPointer { variable: String, pointer: String },
}

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownVariable { name } => {
                write!(f, "template variable `{name}` is not defined")
            }
            Self::UnknownPointer { variable, pointer } => {
                write!(f, "pointer `{pointer}` not found in variable `{variable}`")
            }
        }
    }
}

impl std::error::Error for TemplateError {}

/// Resolves `{{...}}` template placeholders against a dictionary:
/// variable lookup, `${name}` dynamic segments, `/pointer` reads into a
/// variable, and `:type` casts.
pub struct Injector;

impl Injector {
    /// Resolves templates in `s` and returns the result when it is a string.
    /// Returns `None` when a placeholder cannot be resolved or the resolved
    /// value is not a string.
    pub fn inject_str(s: &str, dictionary: &HashMap<String, Value>) -> Option<String> {
        match Self::try_evaluate_string(s, dictionary).ok()? {
            Value::String(v) => Some(v),
            _ => None,
        }
    }

    /// Like [`inject_str`](Self::inject_str) but reports the first placeholder
    /// that could not be resolved instead of falling back to the raw string.
    ///
    /// # Errors
    ///
    /// Returns the missing variable or JSON pointer (see [`TemplateError`]).
    pub fn inject_str_checked(
        s: &str,
        dictionary: &HashMap<String, Value>,
    ) -> Result<String, TemplateError> {
        match Self::try_evaluate_string(s, dictionary)? {
            Value::String(v) => Ok(v),
            other => Ok(other.to_string()),
        }
    }

    /// Resolves templates in place, recursively, throughout a JSON value.
    /// Unresolvable placeholders are left untouched.
    pub fn inject(json: &mut Value, dictionary: &HashMap<String, Value>) {
        match json {
            Value::String(s) => {
                if let Ok(new_val) = Self::try_evaluate_string(s, dictionary) {
                    *json = new_val;
                }
            }
            Value::Object(obj) => {
                for value in obj.values_mut() {
                    Self::inject(value, dictionary);
                }
            }
            Value::Array(items) => {
                for item in items.iter_mut() {
                    Self::inject(item, dictionary);
                }
            }
            _ => {}
        }
    }

    /// Like [`inject`](Self::inject) but stops at the first unresolved
    /// placeholder and reports it, leaving the value partially injected.
    ///
    /// # Errors
    ///
    /// Returns the missing variable or JSON pointer (see [`TemplateError`]).
    pub fn inject_checked(
        json: &mut Value,
        dictionary: &HashMap<String, Value>,
    ) -> Result<(), TemplateError> {
        match json {
            Value::String(s) => {
                *json = Self::try_evaluate_string(s, dictionary)?;
            }
            Value::Object(obj) => {
                for value in obj.values_mut() {
                    Self::inject_checked(value, dictionary)?;
                }
            }
            Value::Array(items) => {
                for item in items.iter_mut() {
                    Self::inject_checked(item, dictionary)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Resolves `${ident}` name-interpolation segments inside a placeholder
    /// body. Returns `None` if any referenced variable is missing.
    fn resolve_dynamic_segments(
        content: &str,
        dictionary: &HashMap<String, Value>,
    ) -> Result<String, TemplateError> {
        static RE: OnceLock<regex::Regex> = OnceLock::new();
        let re = RE.get_or_init(|| regex::Regex::new(r"\$\{([a-zA-Z_][a-zA-Z0-9_]*)}").unwrap());

        let mut out = String::new();
        let mut last_end = 0usize;

        for caps in re.captures_iter(content) {
            let m = caps.get(0).expect("regex match always has group 0");
            out.push_str(&content[last_end..m.start()]);

            let name = caps
                .get(1)
                .expect("dynamic segment always has a name group")
                .as_str();
            let value = dictionary
                .get(name)
                .ok_or_else(|| TemplateError::UnknownVariable {
                    name: name.to_string(),
                })?;
            let segment = match value {
                Value::String(v) => v.clone(),
                Value::Null => {
                    return Err(TemplateError::UnknownVariable {
                        name: name.to_string(),
                    });
                }
                other => other.to_string(),
            };

            out.push_str(&segment);
            last_end = m.end();
        }

        out.push_str(&content[last_end..]);
        Ok(out)
    }

    /// Parses a resolved placeholder body (`name[/pointer][:type]`) and looks
    /// the value up in the dictionary, descending into it when a JSON pointer
    /// is present.
    fn lookup(content: &str, dictionary: &HashMap<String, Value>) -> Result<Value, TemplateError> {
        // Split off an optional `:type` suffix.
        let (path, ty) = match content.rsplit_once(':') {
            Some((path, ty))
                if !path.is_empty()
                    && ty
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                    && ty.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
            {
                (path, Some(ty))
            }
            _ => (content, None),
        };

        // Split off an optional `/pointer` suffix.
        let (variable, pointer) = match path.find('/') {
            Some(pos) => (&path[..pos], &path[pos..]),
            None => (path, ""),
        };

        let mut value = dictionary
            .get(variable)
            .ok_or_else(|| TemplateError::UnknownVariable {
                name: variable.to_string(),
            })?;
        if !pointer.is_empty() {
            value = value
                .pointer(pointer)
                .ok_or_else(|| TemplateError::UnknownPointer {
                    variable: variable.to_string(),
                    pointer: pointer.to_string(),
                })?;
        }

        Ok(Self::convert_value(value, ty))
    }

    fn try_evaluate_string(
        s: &str,
        dictionary: &HashMap<String, Value>,
    ) -> Result<Value, TemplateError> {
        static RE: OnceLock<regex::Regex> = OnceLock::new();

        // Placeholder body: any run of non-brace characters and/or `${...}`
        // name-interpolation segments.
        let re = RE.get_or_init(|| regex::Regex::new(r"\{\{((?:\$\{[^{}]*}|[^{}])+?)}}").unwrap());

        let mut out = String::new();
        let mut last_end = 0usize;
        let mut found_any = false;

        for caps in re.captures_iter(s) {
            let m = caps.get(0).expect("regex match always has group 0");

            let body = caps
                .get(1)
                .expect("placeholder always has a body group")
                .as_str();
            let content = Self::resolve_dynamic_segments(body, dictionary)?;
            let converted = Self::lookup(&content, dictionary)?;

            // A placeholder spanning the entire string keeps its JSON type.
            if m.start() == 0 && m.end() == s.len() {
                return Ok(converted);
            }

            out.push_str(&s[last_end..m.start()]);

            let replacement = match converted {
                Value::String(v) => v,
                Value::Null => "null".to_string(),
                other => other.to_string(),
            };

            out.push_str(&replacement);
            last_end = m.end();
            found_any = true;
        }

        if !found_any {
            return Ok(Value::String(s.to_string()));
        }

        out.push_str(&s[last_end..]);

        if ((out.starts_with('{') && out.ends_with('}'))
            || (out.starts_with('[') && out.ends_with(']')))
            && let Ok(parsed) = serde_json::from_str::<Value>(&out)
        {
            return Ok(parsed);
        }

        Ok(Value::String(out))
    }

    /// Applies an optional `:type` cast to a resolved value. An unknown type
    /// or an impossible cast leaves the value as-is.
    fn convert_value(val: &Value, expected_type: Option<&str>) -> Value {
        let ty = match expected_type {
            Some(t) => t.trim().to_lowercase(),
            None => return val.to_owned(),
        };

        match ty.as_str() {
            "int" | "integer" | "i32" | "i64" | "u32" | "u64" | "u128" | "num" | "number" => {
                to_integer(val)
            }
            "float" | "f32" | "f64" => to_float(val),
            "bool" | "boolean" => to_bool(val),
            "string" | "str" => to_string_value(val),
            _ => val.to_owned(),
        }
    }
}

/// `:int` cast: numbers pass through, integer-looking strings are parsed.
fn to_integer(val: &Value) -> Value {
    if val.is_number() {
        return val.to_owned();
    }
    if let Some(s) = val.as_str()
        && let Ok(n) = s.parse::<i64>()
    {
        return Value::Number(n.into());
    }
    val.to_owned()
}

/// `:float` cast: numbers pass through, float-looking strings are parsed.
fn to_float(val: &Value) -> Value {
    if val.is_number() {
        return val.to_owned();
    }
    if let Some(s) = val.as_str()
        && let Ok(f) = s.parse::<f64>()
        && let Some(num) = serde_json::Number::from_f64(f)
    {
        return Value::Number(num);
    }
    val.to_owned()
}

/// `:bool` cast: booleans pass through; "true"/"1" and "false"/"0" parse.
fn to_bool(val: &Value) -> Value {
    if val.is_boolean() {
        return val.to_owned();
    }
    if let Some(s) = val.as_str() {
        match s.trim().to_lowercase().as_str() {
            "true" | "1" => return Value::Bool(true),
            "false" | "0" => return Value::Bool(false),
            _ => {}
        }
    }
    val.to_owned()
}

/// `:string` cast: any non-string value is rendered as its JSON text.
fn to_string_value(val: &Value) -> Value {
    if val.is_string() {
        val.to_owned()
    } else {
        Value::String(match val {
            Value::Null => "null".to_string(),
            other => other.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dict(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    // ---------- Regression: existing behavior ----------

    #[test]
    fn replaces_simple_variable_in_string() {
        let d = dict(&[("sku", json!("100393-501"))]);
        let mut v = json!("product {{sku}}");
        Injector::inject(&mut v, &d);
        assert_eq!(v, json!("product 100393-501"));
    }

    #[test]
    fn replaces_multiple_variables() {
        let d = dict(&[("a", json!("x")), ("b", json!("y"))]);
        let mut v = json!("{{a}}-{{b}}");
        Injector::inject(&mut v, &d);
        assert_eq!(v, json!("x-y"));
    }

    #[test]
    fn missing_variable_leaves_string_untouched() {
        let d = dict(&[]);
        let mut v = json!("hello {{missing}}");
        Injector::inject(&mut v, &d);
        assert_eq!(v, json!("hello {{missing}}"));
    }

    #[test]
    fn casts_string_to_int() {
        let d = dict(&[("qty", json!("42"))]);
        let mut v = json!("{{qty:int}}");
        Injector::inject(&mut v, &d);
        assert_eq!(v, json!(42));
    }

    #[test]
    fn casts_string_to_float() {
        let d = dict(&[("price", json!("19.07"))]);
        let mut v = json!("{{price:float}}");
        Injector::inject(&mut v, &d);
        assert_eq!(v, json!(19.07));
    }

    #[test]
    fn casts_string_to_bool() {
        let d = dict(&[("on", json!("true")), ("off", json!("0"))]);
        let mut v = json!({"a": "{{on:bool}}", "b": "{{off:bool}}"});
        Injector::inject(&mut v, &d);
        assert_eq!(v, json!({"a": true, "b": false}));
    }

    #[test]
    fn casts_value_to_string() {
        let d = dict(&[("qty", json!(42)), ("obj", json!({"a": 1}))]);
        let mut v = json!({"q": "{{qty:string}}", "o": "{{obj:str}}"});
        Injector::inject(&mut v, &d);
        assert_eq!(v, json!({"q": "42", "o": "{\"a\":1}"}));
    }

    #[test]
    fn impossible_cast_keeps_the_value() {
        let d = dict(&[("name", json!("abc"))]);
        let mut v = json!("{{name:int}}");
        Injector::inject(&mut v, &d);
        assert_eq!(v, json!("abc"));
    }

    #[test]
    fn whole_object_value_is_injected_as_json() {
        let d = dict(&[("obj", json!({"a": 1}))]);
        let mut v = json!("{{obj}}");
        Injector::inject(&mut v, &d);
        assert_eq!(v, json!({"a": 1}));
    }

    #[test]
    fn injects_recursively_in_objects_and_arrays() {
        let d = dict(&[("sku", json!("A"))]);
        let mut v = json!({"payload": {"_sku": "{{sku}}"}, "list": ["{{sku}}"]});
        Injector::inject(&mut v, &d);
        assert_eq!(v, json!({"payload": {"_sku": "A"}, "list": ["A"]}));
    }

    #[test]
    fn inject_str_resolves_url() {
        let d = dict(&[("BASE_URL", json!("https://api.test"))]);
        let out = Injector::inject_str("{{BASE_URL}}/api/x", &d);
        assert_eq!(out, Some("https://api.test/api/x".to_string()));
    }

    // ---------- New: permissive variable names ----------

    #[test]
    fn resolves_variable_name_containing_dash_and_digits() {
        let d = dict(&[("100393-501-detail", json!({"price": 1.5}))]);
        let mut v = json!("{{100393-501-detail}}");
        Injector::inject(&mut v, &d);
        assert_eq!(v, json!({"price": 1.5}));
    }

    // ---------- New: ${var} name interpolation ----------

    #[test]
    fn resolves_dynamic_variable_name() {
        let d = dict(&[
            ("current_sku", json!("100393-501")),
            ("100393-501-detail", json!({"price": 19.07})),
        ]);
        let mut v = json!("{{${current_sku}-detail}}");
        Injector::inject(&mut v, &d);
        assert_eq!(v, json!({"price": 19.07}));
    }

    #[test]
    fn missing_dynamic_segment_leaves_string_untouched() {
        let d = dict(&[("A-detail", json!(1))]);
        let mut v = json!("{{${nope}-detail}}");
        Injector::inject(&mut v, &d);
        assert_eq!(v, json!("{{${nope}-detail}}"));
    }

    // ---------- New: pointer-read inside a variable ----------

    #[test]
    fn pointer_read_into_variable() {
        let d = dict(&[("detail", json!({"price": {"amount": 19.07}}))]);
        let mut v = json!("{{detail/price/amount}}");
        Injector::inject(&mut v, &d);
        assert_eq!(v, json!(19.07));
    }

    #[test]
    fn pointer_read_with_dynamic_segment() {
        let d = dict(&[
            ("sku", json!("100393-501")),
            (
                "sku_details",
                json!({"100393-501": {"price": 19.07, "currency": "CAD"}}),
            ),
        ]);
        let mut v = json!("{{sku_details/${sku}/currency}}");
        Injector::inject(&mut v, &d);
        assert_eq!(v, json!("CAD"));
    }

    #[test]
    fn pointer_read_with_dynamic_segment_and_cast() {
        let d = dict(&[
            ("sku", json!("A")),
            ("sku_details", json!({"A": {"price": "19.07"}})),
        ]);
        let mut v = json!("{{sku_details/${sku}/price:float}}");
        Injector::inject(&mut v, &d);
        assert_eq!(v, json!(19.07));
    }

    #[test]
    fn pointer_read_missing_path_leaves_string_untouched() {
        let d = dict(&[("detail", json!({"a": 1}))]);
        let mut v = json!("{{detail/nope}}");
        Injector::inject(&mut v, &d);
        assert_eq!(v, json!("{{detail/nope}}"));
    }

    #[test]
    fn pointer_read_array_index() {
        let d = dict(&[("items", json!([{"sku": "A"}, {"sku": "B"}]))]);
        let mut v = json!("{{items/1/sku}}");
        Injector::inject(&mut v, &d);
        assert_eq!(v, json!("B"));
    }

    // ---------- New: checked injection reports the missing placeholder ----------

    #[test]
    fn inject_str_checked_resolves_like_inject_str() {
        let d = dict(&[("BASE_URL", json!("https://api.test"))]);
        assert_eq!(
            Injector::inject_str_checked("{{BASE_URL}}/x", &d),
            Ok("https://api.test/x".to_string())
        );
    }

    #[test]
    fn inject_str_checked_names_the_unknown_variable() {
        let d = dict(&[("access_token", json!("t"))]);
        assert_eq!(
            Injector::inject_str_checked("Bearer {{acces_token}}", &d),
            Err(TemplateError::UnknownVariable {
                name: "acces_token".to_string()
            })
        );
    }

    #[test]
    fn inject_checked_reports_unknown_variable_deep_in_a_payload() {
        let d = dict(&[("sku", json!("A"))]);
        let mut v = json!({"payload": {"_sku": "{{sku}}", "_id": "{{missing}}"}});
        let err = Injector::inject_checked(&mut v, &d).unwrap_err();
        assert_eq!(
            err,
            TemplateError::UnknownVariable {
                name: "missing".to_string()
            }
        );
    }

    #[test]
    fn inject_checked_reports_unknown_pointer() {
        let d = dict(&[("detail", json!({"a": 1}))]);
        let mut v = json!("{{detail/nope}}");
        let err = Injector::inject_checked(&mut v, &d).unwrap_err();
        assert_eq!(
            err,
            TemplateError::UnknownPointer {
                variable: "detail".to_string(),
                pointer: "/nope".to_string()
            }
        );
    }

    #[test]
    fn inject_checked_succeeds_when_everything_resolves() {
        let d = dict(&[("sku", json!("A"))]);
        let mut v = json!({"_sku": "{{sku}}"});
        assert!(Injector::inject_checked(&mut v, &d).is_ok());
        assert_eq!(v, json!({"_sku": "A"}));
    }
}
