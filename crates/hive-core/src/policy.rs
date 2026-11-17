//! Declarative tool policy: which tool calls are allowed, denied, or need a human.
//!
//! A policy is data, evaluated natively so every binding decides identically. Rules
//! are tried in order and the first whose tool patterns and conditions all hold wins;
//! with no match the `default` applies. Evaluation is pure: it reads the tool name and
//! arguments and nothing else, calls no model and has no side effects. A condition on
//! an argument that is missing never holds, so a rule that restricts by path cannot be
//! sidestepped by omitting the path (put a catch-all `deny` or `ask` after it).
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_RULES: usize = 256;
const MAX_PATTERNS: usize = 64;
const MAX_CONDITIONS: usize = 16;
const MAX_TEXT: usize = 2048;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    #[default]
    Allow,
    Deny,
    /// A human (or the host's approval callback) must approve this call.
    Ask,
}

impl Decision {
    /// The more restrictive of two decisions: deny beats ask beats allow.
    pub fn strictest(self, other: Self) -> Self {
        fn rank(decision: Decision) -> u8 {
            match decision {
                Decision::Allow => 0,
                Decision::Ask => 1,
                Decision::Deny => 2,
            }
        }
        if rank(other) > rank(self) {
            other
        } else {
            self
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    Equals,
    NotEquals,
    Prefix,
    Suffix,
    Contains,
    OneOf,
    Exists,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    /// A JSON Pointer into the arguments, such as `/path` or `/options/0`.
    pub arg: String,
    pub op: Operator,
    #[serde(default)]
    pub value: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Tool names; `*` matches any run of characters (`fs_*`, `*`).
    pub tools: Vec<String>,
    pub decision: Decision,
    #[serde(default)]
    pub reason: Option<String>,
    /// All must hold for the rule to apply.
    #[serde(default)]
    pub when: Vec<Condition>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicy {
    #[serde(default)]
    pub default: Decision,
    #[serde(default)]
    pub default_reason: Option<String>,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Verdict {
    pub decision: Decision,
    /// Index of the rule that decided, or `None` when the default did.
    pub rule: Option<usize>,
    pub reason: String,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Input(message.into())
}

/// `*` matches any run of characters, everything else matches itself.
fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first) || text.len() < first.len() + last.len() || !text.ends_with(last) {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for middle in &parts[1..parts.len() - 1] {
        match rest.find(middle) {
            Some(at) => rest = &rest[at + middle.len()..],
            None => return false,
        }
    }
    true
}

impl Condition {
    fn holds(&self, arguments: &Value) -> bool {
        let found = arguments
            .pointer(&self.arg)
            .filter(|value| !value.is_null());
        if self.op == Operator::Exists {
            return found.is_some() == (self.value != Value::Bool(false));
        }
        let Some(found) = found else { return false };
        let text = found.as_str();
        let wanted = self.value.as_str();
        match self.op {
            Operator::Equals => *found == self.value,
            Operator::NotEquals => *found != self.value,
            Operator::Prefix => text
                .zip(wanted)
                .is_some_and(|(text, wanted)| text.starts_with(wanted)),
            Operator::Suffix => text
                .zip(wanted)
                .is_some_and(|(text, wanted)| text.ends_with(wanted)),
            Operator::Contains => text
                .zip(wanted)
                .is_some_and(|(text, wanted)| text.contains(wanted)),
            Operator::OneOf => self
                .value
                .as_array()
                .is_some_and(|options| options.contains(found)),
            Operator::Exists => unreachable!(),
        }
    }
}

impl ToolPolicy {
    pub fn from_value(value: Value) -> Result<Self> {
        let policy: Self = serde_json::from_value(value)
            .map_err(|error| invalid(format!("invalid tool policy: {error}")))?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<()> {
        let long = |text: &str| text.len() > MAX_TEXT || text.chars().any(char::is_control);
        if self.rules.len() > MAX_RULES || self.default_reason.as_deref().is_some_and(long) {
            return Err(invalid(format!(
                "a tool policy has at most {MAX_RULES} rules and short reasons"
            )));
        }
        for rule in &self.rules {
            if rule.tools.is_empty()
                || rule.tools.len() > MAX_PATTERNS
                || rule.tools.iter().any(|p| p.is_empty() || long(p))
            {
                return Err(invalid("every rule needs 1..=64 non-empty tool patterns"));
            }
            if rule.when.len() > MAX_CONDITIONS || rule.reason.as_deref().is_some_and(long) {
                return Err(invalid(
                    "a rule has at most 16 conditions and a short reason",
                ));
            }
            for condition in &rule.when {
                if !(condition.arg.is_empty() || condition.arg.starts_with('/'))
                    || long(&condition.arg)
                {
                    return Err(invalid(
                        "condition arg must be a JSON Pointer such as /path",
                    ));
                }
                let needs_text = matches!(
                    condition.op,
                    Operator::Prefix | Operator::Suffix | Operator::Contains
                );
                if (needs_text && !condition.value.is_string())
                    || (condition.op == Operator::OneOf
                        && !condition
                            .value
                            .as_array()
                            .is_some_and(|options| !options.is_empty() && options.len() <= 256))
                    || serde_json::to_vec(&condition.value)
                        .map_or(true, |bytes| bytes.len() > 16 * 1024)
                {
                    return Err(invalid("a condition value does not fit its operator"));
                }
            }
        }
        Ok(())
    }

    pub fn evaluate(&self, tool: &str, arguments: &Value) -> Verdict {
        for (index, rule) in self.rules.iter().enumerate() {
            if rule.tools.iter().any(|pattern| glob(pattern, tool))
                && rule.when.iter().all(|condition| condition.holds(arguments))
            {
                return Verdict {
                    decision: rule.decision,
                    rule: Some(index),
                    reason: rule
                        .reason
                        .clone()
                        .unwrap_or_else(|| format!("rule {index}")),
                };
            }
        }
        Verdict {
            decision: self.default,
            rule: None,
            reason: self
                .default_reason
                .clone()
                .unwrap_or_else(|| "default".into()),
        }
    }
}

/// JSON entry point shared by the bindings: `{decision, rule, reason}` for one call.
pub fn evaluate_json(policy: Value, tool: &str, arguments: Value) -> Result<Value> {
    Ok(serde_json::to_value(
        ToolPolicy::from_value(policy)?.evaluate(tool, &arguments),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(value: Value) -> ToolPolicy {
        ToolPolicy::from_value(value).unwrap()
    }
    fn decide(policy: &ToolPolicy, tool: &str, arguments: Value) -> (Decision, Option<usize>) {
        let verdict = policy.evaluate(tool, &arguments);
        (verdict.decision, verdict.rule)
    }

    #[test]
    fn patterns_match_by_glob_and_the_first_matching_rule_wins() {
        let policy = policy(json!({"default":"deny","rules":[
            {"tools":["fs_read","fs_list"],"decision":"allow"},
            {"tools":["fs_*"],"decision":"ask","reason":"writes need approval"},
            {"tools":["obscura_*"],"decision":"allow"},
            {"tools":["*_run"],"decision":"deny"}]}));
        assert_eq!(
            decide(&policy, "fs_read", json!({})),
            (Decision::Allow, Some(0))
        );
        assert_eq!(
            decide(&policy, "fs_write", json!({})),
            (Decision::Ask, Some(1))
        );
        assert_eq!(
            policy.evaluate("fs_write", &json!({})).reason,
            "writes need approval"
        );
        assert_eq!(
            decide(&policy, "obscura_navigate", json!({})),
            (Decision::Allow, Some(2))
        );
        assert_eq!(
            decide(&policy, "command_run", json!({})),
            (Decision::Deny, Some(3))
        );
        assert_eq!(
            decide(&policy, "anything_else", json!({})),
            (Decision::Deny, None)
        );
        for (pattern, text, expected) in [
            ("a*b*c", "aXXbYYc", true),
            ("a*b*c", "aXXcYYb", false),
            ("*", "", true),
            ("a*", "a", true),
            ("*a", "ba", true),
            ("a*a", "a", false),
            ("exact", "exact!", false),
        ] {
            assert_eq!(glob(pattern, text), expected, "{pattern} vs {text}");
        }
    }

    #[test]
    fn conditions_inspect_arguments_and_a_missing_argument_never_matches() {
        let policy = policy(json!({"default":"deny","rules":[
            {"tools":["fs_write"],"decision":"allow","when":[{"arg":"/path","op":"prefix","value":"reports/"},{"arg":"/path","op":"suffix","value":".md"}]},
            {"tools":["web_fetch"],"decision":"allow","when":[{"arg":"/url","op":"contains","value":"example.com"}]},
            {"tools":["command_run"],"decision":"ask","when":[{"arg":"/program","op":"one_of","value":["git","ls"]}]},
            {"tools":["office_write_pdf"],"decision":"deny","when":[{"arg":"/overwrite","op":"equals","value":true}]},
            {"tools":["office_write_pdf"],"decision":"allow","when":[{"arg":"/overwrite","op":"exists","value":false}]},
            {"tools":["fs_edit"],"decision":"deny","when":[{"arg":"/options/0","op":"not_equals","value":"safe"}]}]}));
        assert_eq!(
            decide(&policy, "fs_write", json!({"path":"reports/q1.md"})).0,
            Decision::Allow
        );
        assert_eq!(
            decide(&policy, "fs_write", json!({"path":"reports/q1.txt"})).0,
            Decision::Deny
        );
        assert_eq!(
            decide(&policy, "fs_write", json!({"path":"/etc/passwd"})).0,
            Decision::Deny
        );
        assert_eq!(
            decide(&policy, "fs_write", json!({})).0,
            Decision::Deny,
            "omitting the argument does not satisfy the rule"
        );
        assert_eq!(
            decide(&policy, "fs_write", json!({"path":null})).0,
            Decision::Deny,
            "null counts as missing"
        );
        assert_eq!(
            decide(&policy, "fs_write", json!({"path":42})).0,
            Decision::Deny,
            "a prefix of a non-string is false"
        );
        assert_eq!(
            decide(
                &policy,
                "web_fetch",
                json!({"url":"https://api.example.com/x"})
            )
            .0,
            Decision::Allow
        );
        assert_eq!(
            decide(&policy, "command_run", json!({"program":"git"})).0,
            Decision::Ask
        );
        assert_eq!(
            decide(&policy, "command_run", json!({"program":"rm"})).0,
            Decision::Deny
        );
        assert_eq!(
            decide(&policy, "office_write_pdf", json!({"overwrite":true})).0,
            Decision::Deny
        );
        assert_eq!(
            decide(&policy, "office_write_pdf", json!({})).0,
            Decision::Allow
        );
        assert_eq!(
            decide(&policy, "fs_edit", json!({"options":["safe"]})).0,
            Decision::Deny,
            "no rule allowed it"
        );
        assert_eq!(
            decide(&policy, "fs_edit", json!({"options":["risky"]})),
            (Decision::Deny, Some(5))
        );
    }

    #[test]
    fn the_default_is_allow_so_an_empty_policy_changes_nothing_and_strictest_orders_decisions() {
        let empty = policy(json!({}));
        assert_eq!(
            empty.evaluate("anything", &json!({})).decision,
            Decision::Allow
        );
        assert_eq!(Decision::Allow.strictest(Decision::Ask), Decision::Ask);
        assert_eq!(Decision::Ask.strictest(Decision::Deny), Decision::Deny);
        assert_eq!(Decision::Deny.strictest(Decision::Allow), Decision::Deny);
        assert_eq!(Decision::Allow.strictest(Decision::Allow), Decision::Allow);
        assert_eq!(
            evaluate_json(json!({"default":"ask"}), "x", json!({})).unwrap(),
            json!({"decision":"ask","rule":null,"reason":"default"})
        );
    }

    #[test]
    fn invalid_policies_are_rejected_before_they_can_be_used() {
        for bad in [
            json!({"default":"maybe"}),
            json!({"rules":[{"tools":[],"decision":"allow"}]}),
            json!({"rules":[{"tools":[""],"decision":"allow"}]}),
            json!({"rules":[{"tools":["x"],"decision":"allow","bogus":1}]}),
            json!({"rules":[{"tools":["x"],"decision":"allow","when":[{"arg":"path","op":"equals","value":1}]}]}),
            json!({"rules":[{"tools":["x"],"decision":"allow","when":[{"arg":"/p","op":"prefix","value":3}]}]}),
            json!({"rules":[{"tools":["x"],"decision":"allow","when":[{"arg":"/p","op":"one_of","value":"a"}]}]}),
            json!({"rules":[{"tools":["x"],"decision":"allow","when":[{"arg":"/p","op":"regex","value":"a"}]}]}),
            json!({"unknown":true}),
            json!({"rules":(0..257).map(|_| json!({"tools":["x"],"decision":"allow"})).collect::<Vec<_>>()}),
        ] {
            assert!(ToolPolicy::from_value(bad.clone()).is_err(), "{bad}");
        }
    }
}
