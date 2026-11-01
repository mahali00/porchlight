use crate::config::Input;
use indexmap::IndexMap;
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    Text(String),
    Input(String),
}

pub type Word = Vec<Part>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookupKind {
    Env,
    File,
}

pub type Lookup<'a> = &'a dyn Fn(LookupKind, &str) -> Option<String>;

pub(super) fn push_text(word: &mut Word, text: &str) {
    if text.is_empty() {
        return;
    }

    if let Some(Part::Text(last)) = word.last_mut() {
        last.push_str(text);
    } else {
        word.push(Part::Text(text.to_owned()));
    }
}

pub(super) fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

pub(super) enum Reference<'a> {
    Lookup(LookupKind, &'a str),
    Input(&'a str),
}

pub(super) fn reference(inner: &str) -> Option<Reference<'_>> {
    if let Some(name) = inner.strip_prefix("env:").filter(|name| !name.is_empty()) {
        return Some(Reference::Lookup(LookupKind::Env, name));
    }

    if let Some(name) = inner.strip_prefix("file:").filter(|name| !name.is_empty()) {
        return Some(Reference::Lookup(LookupKind::File, name));
    }

    (!inner.is_empty() && inner.chars().all(is_word_char)).then_some(Reference::Input(inner))
}

pub fn parse_word(word: &str, lookup: Lookup) -> Result<Word, String> {
    let mut parts = Word::new();
    let mut rest = word;

    while let Some(open) = rest.find('{') {
        let (before, from_brace) = rest.split_at(open);
        push_text(&mut parts, before);
        let after_brace = from_brace.get(1..).unwrap_or_default();
        let found = after_brace.find('}').and_then(|close| {
            let inner = after_brace.get(..close)?;
            Some((reference(inner)?, after_brace.get(close + 1..).unwrap_or_default()))
        });

        match found {
            Some((Reference::Input(name), tail)) => {
                parts.push(Part::Input(name.to_owned()));
                rest = tail;
            }
            Some((Reference::Lookup(kind, name), tail)) => {
                let value = lookup(kind, name).ok_or_else(|| match kind {
                    LookupKind::Env => format!("{name} isn't set"),
                    LookupKind::File => format!("can't read {name}"),
                })?;
                push_text(&mut parts, &value);
                rest = tail;
            }
            None => {
                push_text(&mut parts, "{");
                rest = after_brace;
            }
        }
    }

    push_text(&mut parts, rest);

    Ok(parts)
}

pub fn literal(word: &Word) -> String {
    word.iter()
        .map(|part| match part {
            Part::Text(text) => text.clone(),
            Part::Input(name) => format!("{{{name}}}"),
        })
        .collect()
}

pub fn inputs_of(words: &[Word]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();

    for part in words.iter().flatten() {
        if let Part::Input(name) = part
            && !names.contains(name)
        {
            names.push(name.clone());
        }
    }

    names
}

pub fn sole_input(word: &Word) -> Option<&str> {
    match word.as_slice() {
        [Part::Input(name)] => Some(name),
        _ => None,
    }
}

pub(super) fn has_input(word: &Word) -> bool {
    word.iter().any(|part| matches!(part, Part::Input(_)))
}

pub(super) fn word_text(word: &Word) -> Option<String> {
    word.iter()
        .map(|part| match part {
            Part::Text(text) => Some(text.as_str()),
            Part::Input(_) => None,
        })
        .collect::<Option<Vec<_>>>()
        .map(|parts| parts.concat())
}

pub(super) fn number_text(number: &serde_json::Number) -> String {
    match number.as_f64() {
        Some(float) if !number.is_i64() && !number.is_u64() && float.fract() == 0.0 && float.abs() < 1e21 => {
            format!("{float:.0}")
        }
        _ => number.to_string(),
    }
}

pub(super) fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number_text(number),
        other => other.to_string(),
    }
}

pub(super) fn fill_word(word: &Word, values: &Map<String, Value>, inputs: &IndexMap<String, Input>) -> Option<String> {
    if word.iter().any(|part| matches!(part, Part::Input(name) if !values.contains_key(name))) {
        return None;
    }

    if let Some(name) = sole_input(word)
        && let Some(flag) = inputs.get(name).and_then(|input| input.flag.as_ref())
    {
        return (values.get(name) == Some(&Value::Bool(true))).then(|| flag.clone());
    }

    Some(
        word.iter()
            .map(|part| match part {
                Part::Text(text) => text.clone(),
                Part::Input(name) => values.get(name).map(value_text).unwrap_or_default(),
            })
            .collect(),
    )
}

pub fn fill(words: &[Word], values: &Map<String, Value>, inputs: &IndexMap<String, Input>) -> Vec<String> {
    words.iter().filter_map(|word| fill_word(word, values, inputs)).collect()
}

pub(super) fn starts_word(words: &[Word], name: &str) -> bool {
    words.iter().any(|word| matches!(word.first(), Some(Part::Input(first)) if first == name))
}
