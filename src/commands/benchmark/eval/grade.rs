//! Deterministic grading of a trace against a case's expectations. Pure: no I/O, no clock.

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use super::trace::{Trace, TraceCall};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Expect {
    pub calls: Vec<CallExpect>,
    pub calls_must_not: Vec<CallExpect>,
    pub max_calls: Option<usize>,
    pub order: Vec<String>,
    pub final_text_must: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallExpect {
    pub tool: String,
    #[serde(default)]
    pub args: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub label: String,
    pub passed: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grade {
    pub checks: Vec<Check>,
}

impl Grade {
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }
}

/// The TOML spelling of `null` (TOML has no null): the key must be absent.
const NULL_MARKER: &str = "<null>";

/// A dotted path is tried as a literal key first, then walked through nested objects.
fn lookup<'a>(actual: &'a Value, path: &str) -> Option<&'a Value> {
    if let Some(found) = actual.get(path) {
        return Some(found);
    }
    path.split('.')
        .try_fold(actual, |value, key| value.get(key))
}

fn text_of(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_string)
}

fn number_of(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

fn bound_match(op: &str, limit: &str, actual: &Value, path: &str) -> Result<(), String> {
    let limit: f64 = limit
        .trim()
        .parse()
        .map_err(|_| format!("{path}: '{op}{limit}' is not a numeric bound"))?;
    let value = number_of(actual).ok_or_else(|| format!("{path}: {actual} is not a number"))?;
    let holds = if op == "<=" {
        value <= limit
    } else {
        value >= limit
    };
    if holds {
        return Ok(());
    }
    Err(format!("{path}: {value} is not {op} {limit}"))
}

fn string_match(text: &str, actual: &Value, path: &str) -> Result<(), String> {
    if let Some(needle) = text.strip_prefix('~') {
        if text_of(actual)
            .to_lowercase()
            .contains(&needle.to_lowercase())
        {
            return Ok(());
        }
        return Err(format!("{path}: {actual} does not contain '{needle}'"));
    }
    if let Some(limit) = text.strip_prefix("<=") {
        return bound_match("<=", limit, actual, path);
    }
    if let Some(limit) = text.strip_prefix(">=") {
        return bound_match(">=", limit, actual, path);
    }
    if actual.as_str() == Some(text) {
        return Ok(());
    }
    Err(format!("{path}: {actual} is not \"{text}\""))
}

fn array_match(items: &[Value], actual: &Value, path: &str) -> Result<(), String> {
    let found = actual
        .as_array()
        .ok_or_else(|| format!("{path}: {actual} is not an array"))?;
    for item in items {
        let hit = found
            .iter()
            .any(|candidate| value_match(item, Some(candidate), path).is_ok());
        if !hit {
            return Err(format!("{path}: no element matches {item}"));
        }
    }
    Ok(())
}

fn value_match(expected: &Value, actual: Option<&Value>, path: &str) -> Result<(), String> {
    if expected.is_null() || expected.as_str() == Some(NULL_MARKER) {
        return match actual {
            None | Some(Value::Null) => Ok(()),
            Some(found) => Err(format!("{path}: expected it missing, found {found}")),
        };
    }
    let actual = actual.ok_or_else(|| format!("{path}: missing"))?;
    match expected {
        Value::Object(_) => object_match(expected, actual, path),
        Value::Array(items) => array_match(items, actual, path),
        Value::String(text) => string_match(text, actual, path),
        other if other == actual => Ok(()),
        other => Err(format!("{path}: {actual} is not {other}")),
    }
}

fn object_match(expected: &Value, actual: &Value, path: &str) -> Result<(), String> {
    let Some(map) = expected.as_object() else {
        return Err(format!("{path}: expected an object"));
    };
    if !actual.is_object() {
        return Err(format!("{path}: {actual} is not an object"));
    }
    for (key, want) in map {
        let child = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        value_match(want, lookup(actual, key), &child)?;
    }
    Ok(())
}

/// `Ok` when `actual` satisfies the partial `expected`: objects match partially, `~text` is a
/// case-insensitive substring, `"<=N"`/`">=N"` bound a number, `null` (or `"<null>"`) means the
/// key is absent, and a dotted key walks nested objects.
pub fn args_match(expected: &Value, actual: &Value) -> Result<(), String> {
    object_match(expected, actual, "")
}

fn call_matches(want: &CallExpect, call: &TraceCall) -> Result<(), String> {
    if !want.tool.eq_ignore_ascii_case(&call.name) {
        return Err(format!("tool is {}, not {}", call.name, want.tool));
    }
    match &want.args {
        Some(args) => args_match(args, &call.args),
        None => Ok(()),
    }
}

fn describe(want: &CallExpect) -> String {
    match &want.args {
        Some(args) => format!("{} {args}", want.tool),
        None => want.tool.clone(),
    }
}

fn check(label: String, outcome: Result<(), String>) -> Check {
    Check {
        label,
        passed: outcome.is_ok(),
        reason: outcome.map_or_else(|e| e, |()| "ok".to_string()),
    }
}

fn is_subsequence(order: &[String], names: &[&str]) -> bool {
    let mut remaining = names.iter();
    order
        .iter()
        .all(|want| remaining.any(|name| name.eq_ignore_ascii_case(want)))
}

pub fn grade(expect: &Expect, trace: &Trace) -> Grade {
    let names: Vec<&str> = trace.calls.iter().map(|c| c.name.as_str()).collect();
    let mut checks = Vec::new();
    for want in &expect.calls {
        let outcome = if trace.calls.iter().any(|c| call_matches(want, c).is_ok()) {
            Ok(())
        } else {
            Err(format!("no call matched; saw [{}]", names.join(", ")))
        };
        checks.push(check(format!("calls: {}", describe(want)), outcome));
    }
    for banned in &expect.calls_must_not {
        let outcome = match trace.calls.iter().find(|c| call_matches(banned, c).is_ok()) {
            Some(call) => Err(format!("forbidden call made: {} {}", call.name, call.args)),
            None => Ok(()),
        };
        checks.push(check(
            format!("calls_must_not: {}", describe(banned)),
            outcome,
        ));
    }
    if let Some(max) = expect.max_calls {
        let made = trace.calls.len();
        let outcome = if made <= max {
            Ok(())
        } else {
            Err(format!("{made} calls made, at most {max} allowed"))
        };
        checks.push(check(format!("max_calls: {max}"), outcome));
    }
    if !expect.order.is_empty() {
        let outcome = if is_subsequence(&expect.order, &names) {
            Ok(())
        } else {
            Err(format!("saw [{}]", names.join(", ")))
        };
        checks.push(check(
            format!("order: {}", expect.order.join(" -> ")),
            outcome,
        ));
    }
    for pattern in &expect.final_text_must {
        let outcome = match Regex::new(pattern) {
            Ok(re) if re.is_match(&trace.final_text) => Ok(()),
            Ok(_) => Err("final text does not match".to_string()),
            Err(error) => Err(format!("invalid regex: {error}")),
        };
        checks.push(check(format!("final_text_must: {pattern}"), outcome));
    }
    Grade { checks }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::benchmark::eval::trace::TraceCall;
    use serde_json::json;

    fn call(name: &str, args: Value) -> TraceCall {
        TraceCall {
            name: name.to_string(),
            args,
            is_error: false,
        }
    }

    fn trace(calls: Vec<TraceCall>, text: &str) -> Trace {
        Trace {
            calls,
            final_text: text.to_string(),
        }
    }

    fn expect(toml_text: &str) -> Expect {
        toml::from_str(toml_text).unwrap()
    }

    #[test]
    fn partial_objects_match_extra_keys_in_actual() {
        let actual = json!({"a": 1, "b": "x", "c": [1]});
        assert!(args_match(&json!({"a": 1}), &actual).is_ok());
        assert!(args_match(&json!({"a": 2}), &actual).is_err());
        assert!(args_match(&json!({"z": 1}), &actual).is_err());
    }

    #[test]
    fn tilde_is_a_case_insensitive_substring() {
        let actual = json!({"command": "Zirv Skill Load implement"});
        assert!(args_match(&json!({"command": "~zirv skill load"}), &actual).is_ok());
        assert!(args_match(&json!({"command": "~benchmark"}), &actual).is_err());
        assert!(args_match(&json!({"n": "~12"}), &json!({"n": 120})).is_ok());
    }

    #[test]
    fn numeric_bounds() {
        let actual = json!({"limit": 5, "s": "7"});
        assert!(args_match(&json!({"limit": "<=5"}), &actual).is_ok());
        assert!(args_match(&json!({"limit": "<=4"}), &actual).is_err());
        assert!(args_match(&json!({"limit": ">=5"}), &actual).is_ok());
        assert!(args_match(&json!({"limit": ">=6"}), &actual).is_err());
        assert!(args_match(&json!({"s": ">=7"}), &actual).is_ok());
        assert!(args_match(&json!({"s": "<=x"}), &actual).is_err());
    }

    #[test]
    fn null_means_the_key_is_missing() {
        let actual = json!({"a": 1, "n": null});
        assert!(args_match(&json!({"b": null}), &actual).is_ok());
        assert!(args_match(&json!({"n": null}), &actual).is_ok());
        assert!(args_match(&json!({"a": null}), &actual).is_err());
        assert!(args_match(&json!({"b": "<null>"}), &actual).is_ok());
        assert!(args_match(&json!({"a": "<null>"}), &actual).is_err());
    }

    #[test]
    fn dotted_paths_reach_nested_keys() {
        let actual = json!({"opts": {"depth": 3, "mode": "Fast"}, "dotted.key": 1});
        assert!(args_match(&json!({"opts.depth": 3}), &actual).is_ok());
        assert!(args_match(&json!({"opts.mode": "~fast"}), &actual).is_ok());
        assert!(args_match(&json!({"opts.depth": "<=2"}), &actual).is_err());
        assert!(args_match(&json!({"opts.missing": null}), &actual).is_ok());
        assert!(args_match(&json!({"dotted.key": 1}), &actual).is_ok());
        assert!(args_match(&json!({"opts": {"depth": 3}}), &actual).is_ok());
    }

    #[test]
    fn arrays_need_each_expected_element_somewhere() {
        let actual = json!({"tags": ["a", "b", "c"]});
        assert!(args_match(&json!({"tags": ["c", "a"]}), &actual).is_ok());
        assert!(args_match(&json!({"tags": ["z"]}), &actual).is_err());
    }

    #[test]
    fn calls_require_a_matching_call() {
        let e = expect(r#"calls = [{ tool = "shell", args = { command = "~cargo test" } }]"#);
        let good = trace(vec![call("shell", json!({"command": "cargo test -q"}))], "");
        let wrong_args = trace(vec![call("shell", json!({"command": "ls"}))], "");
        let wrong_tool = trace(vec![call("Read", json!({"command": "cargo test"}))], "");
        assert!(grade(&e, &good).passed());
        assert!(!grade(&e, &wrong_args).passed());
        assert!(!grade(&e, &wrong_tool).passed());
        assert!(!grade(&e, &trace(vec![], "")).passed());
    }

    #[test]
    fn calls_must_not_rejects_any_matching_call() {
        let e = expect(
            r#"calls_must_not = [{ tool = "shell", args = { command = "~push" } }, { tool = "Delete" }]"#,
        );
        assert!(
            grade(
                &e,
                &trace(vec![call("shell", json!({"command": "ls"}))], "")
            )
            .passed()
        );
        assert!(
            !grade(
                &e,
                &trace(vec![call("shell", json!({"command": "git push"}))], "")
            )
            .passed()
        );
        assert!(!grade(&e, &trace(vec![call("Delete", json!({}))], "")).passed());
    }

    #[test]
    fn max_calls_bounds_the_trace_length() {
        let e = expect("max_calls = 1");
        let one = trace(vec![call("a", json!({}))], "");
        let two = trace(vec![call("a", json!({})), call("b", json!({}))], "");
        assert!(grade(&e, &one).passed());
        assert!(!grade(&e, &two).passed());
    }

    #[test]
    fn order_is_a_subsequence_of_tool_names() {
        let e = expect(r#"order = ["Edit", "shell"]"#);
        let ok = trace(
            vec![
                call("Read", json!({})),
                call("Edit", json!({})),
                call("Read", json!({})),
                call("shell", json!({})),
            ],
            "",
        );
        let reversed = trace(vec![call("shell", json!({})), call("Edit", json!({}))], "");
        assert!(grade(&e, &ok).passed());
        assert!(!grade(&e, &reversed).passed());
    }

    #[test]
    fn final_text_must_match_every_regex() {
        let e = expect(r#"final_text_must = ["(?i)tests? pass", "done$"]"#);
        assert!(grade(&e, &trace(vec![], "Tests pass, done")).passed());
        assert!(!grade(&e, &trace(vec![], "Tests pass")).passed());
        let bad = expect(r#"final_text_must = ["("]"#);
        assert!(!grade(&bad, &trace(vec![], "x")).passed());
    }

    #[test]
    fn every_expectation_reports_its_own_reason() {
        let e = expect(
            r#"
            max_calls = 0
            calls = [{ tool = "shell" }]
            "#,
        );
        let g = grade(&e, &trace(vec![call("Read", json!({}))], ""));
        assert_eq!(g.checks.len(), 2);
        assert_eq!(g.checks.iter().filter(|c| !c.passed).count(), 2);
        assert!(g.checks.iter().all(|c| !c.reason.is_empty()));
    }

    #[test]
    fn unknown_expectation_keys_are_rejected() {
        assert!(toml::from_str::<Expect>("calls_mustnt = []").is_err());
        assert!(toml::from_str::<Expect>(r#"calls = [{ tool = "a", arg = {} }]"#).is_err());
    }
}
