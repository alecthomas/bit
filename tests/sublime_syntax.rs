//! Keeps the word lists in the Sublime syntax in step with the tree-sitter
//! grammar, so a keyword added there is highlighted without editing the
//! syntax by hand. With `UPDATE_SUBLIME_SYNTAX` set, the test rewrites the
//! lists instead of failing; `bit sync-sublime-syntax` runs it that way.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

const GRAMMAR: &str = "integrations/tree-sitter/src/grammar.json";
const SYNTAX: &str = "integrations/sublime/Bit.sublime-syntax";
const BEGIN: &str = "  # BEGIN generated from the tree-sitter grammar by tests/sublime_syntax.rs\n";
const END: &str = "  # END generated\n";

#[test]
fn word_lists_match_grammar() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let grammar: Value = serde_json::from_str(&std::fs::read_to_string(root.join(GRAMMAR)).unwrap()).unwrap();
    let path = root.join(SYNTAX);
    let syntax = std::fs::read_to_string(&path).unwrap();
    let start = syntax.find(BEGIN).expect("syntax has no BEGIN marker") + BEGIN.len();
    let end = start + syntax[start..].find(END).expect("syntax has no END marker");
    let expected = word_variables(&grammar);
    if syntax[start..end] == expected {
        return;
    }
    if std::env::var_os("UPDATE_SUBLIME_SYNTAX").is_some() {
        std::fs::write(&path, format!("{}{expected}{}", &syntax[..start], &syntax[end..])).unwrap();
        return;
    }
    assert_eq!(
        syntax[start..end],
        expected,
        "{SYNTAX} is out of date with {GRAMMAR}; run `bit sync-sublime-syntax`"
    );
}

/// Render one syntax variable per category, each matching any of the
/// grammar's words in that category.
fn word_variables(grammar: &Value) -> String {
    let mut categories: BTreeMap<String, &str> = BTreeMap::new();
    for (rule, body) in grammar["rules"].as_object().expect("grammar has rules") {
        let role = category(rule);
        let mut words = Vec::new();
        collect_words(body, &mut words);
        for word in words {
            // A word used both generically and in a specific role, such as
            // `string` in a map type and as a scalar type, takes the role.
            let entry = categories.entry(word).or_insert(role);
            if *entry == "keywords" {
                *entry = role;
            }
        }
    }
    let mut grouped: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for (word, category) in categories {
        grouped.entry(category).or_default().insert(word);
    }
    grouped
        .iter()
        .map(|(category, words)| {
            let alternatives: Vec<&str> = words.iter().map(String::as_str).collect();
            format!("  {category}: '(?:{})'\n", alternatives.join("|"))
        })
        .collect()
}

/// The syntax variable holding the words that appear in `rule`.
fn category(rule: &str) -> &'static str {
    match rule {
        "scalar_type" => "types",
        "boolean" | "null" => "constants",
        "if_expression" => "conditionals",
        "logical_expression" | "not_expression" => "operators",
        _ if rule.ends_with("_modifier") => "modifiers",
        _ => "keywords",
    }
}

/// Collect the string tokens in a rule that look like identifiers, which are
/// the language's reserved words.
fn collect_words(node: &Value, words: &mut Vec<String>) {
    match node {
        Value::Object(fields) => {
            if fields.get("type").and_then(Value::as_str) == Some("STRING")
                && let Some(value) = fields.get("value").and_then(Value::as_str)
                && is_word(value)
            {
                words.push(value.to_owned());
            }
            fields.values().for_each(|child| collect_words(child, words));
        }
        Value::Array(items) => items.iter().for_each(|child| collect_words(child, words)),
        _ => {}
    }
}

fn is_word(token: &str) -> bool {
    let mut chars = token.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
