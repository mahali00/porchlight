use super::CommandTool;
use super::template::{inputs_of, starts_word};
use crate::config::{Input, InputType};
use serde_json::{Map, Value, json};

pub(super) const MAX_INPUT: usize = 4_096;

pub(super) fn check_number(
    name: &str,
    value: &Value,
    input: Option<&Input>,
    integer: bool,
    starts: bool,
) -> Result<(), String> {
    let number = value.as_f64().filter(|number| number.is_finite()).ok_or(format!("{name}: must be a number"))?;

    if integer && number.fract() != 0.0 {
        return Err(format!("{name}: must be a whole number"));
    }

    if let Some(min) = input.and_then(|input| input.min).filter(|min| number < *min) {
        return Err(format!("{name}: must be at least {min}"));
    }

    if let Some(max) = input.and_then(|input| input.max).filter(|max| number > *max) {
        return Err(format!("{name}: must be at most {max}"));
    }

    if starts && number < 0.0 {
        return Err(format!("{name}: can't be negative here"));
    }

    Ok(())
}

pub(super) fn check_value(name: &str, value: &Value, input: Option<&Input>, starts: bool) -> Result<(), String> {
    match input.and_then(|input| input.kind) {
        Some(InputType::Boolean) => value.is_boolean().then_some(()).ok_or(format!("{name}: must be true or false")),
        Some(InputType::Integer) => check_number(name, value, input, true, starts),
        Some(InputType::Number) => check_number(name, value, input, false, starts),
        Some(InputType::String) | None => {
            let text = value.as_str().ok_or(format!("{name}: must be text"))?;

            if let Some(choices) = input.and_then(|input| input.choices.as_ref()) {
                return choices
                    .iter()
                    .any(|choice| choice == text)
                    .then_some(())
                    .ok_or(format!("{name}: must be one of {}", choices.join(", ")));
            }

            if text.chars().count() > MAX_INPUT {
                return Err(format!("{name}: must be at most {MAX_INPUT} characters"));
            }

            if starts && (text.is_empty() || text.starts_with('-')) {
                return Err(format!("{name}: can't be empty or start with \"-\""));
            }

            if text.split(['/', '\\']).any(|segment| segment == "..") {
                return Err(format!("{name}: can't contain .."));
            }

            Ok(())
        }
    }
}

pub fn validate(tool: &CommandTool, arguments: &Value) -> Result<Map<String, Value>, String> {
    let given = arguments.as_object().ok_or("arguments must be an object")?;
    let mut values = Map::new();

    for name in inputs_of(&tool.words) {
        let input = tool.inputs.get(&name);

        match given.get(&name) {
            None if input.is_some_and(Input::is_optional) => {}
            None => return Err(format!("{name}: is missing")),
            Some(value) => {
                check_value(&name, value, input, starts_word(&tool.words, &name))?;
                values.insert(name, value.clone());
            }
        }
    }

    Ok(values)
}

pub(super) fn property(input: Option<&Input>, starts: bool) -> Value {
    let mut schema = Map::new();

    match input.and_then(|input| input.kind) {
        Some(InputType::Boolean) => {
            schema.insert("type".into(), json!("boolean"));
        }
        Some(kind @ (InputType::Integer | InputType::Number)) => {
            schema.insert("type".into(), json!(if kind == InputType::Integer { "integer" } else { "number" }));
            let floor = if starts { Some(0.0) } else { None };
            let minimum = [input.and_then(|input| input.min), floor].into_iter().flatten().reduce(f64::max);

            if let Some(minimum) = minimum {
                schema.insert("minimum".into(), json!(minimum));
            }

            if let Some(maximum) = input.and_then(|input| input.max) {
                schema.insert("maximum".into(), json!(maximum));
            }
        }
        Some(InputType::String) | None => {
            schema.insert("type".into(), json!("string"));

            if let Some(choices) = input.and_then(|input| input.choices.as_ref()) {
                schema.insert("enum".into(), json!(choices));
            } else {
                schema.insert("maxLength".into(), json!(MAX_INPUT));

                if starts {
                    schema.insert("pattern".into(), json!("^[^-]"));
                }
            }
        }
    }

    if let Some(description) = input.and_then(|input| input.description.as_ref()) {
        schema.insert("description".into(), json!(description));
    }

    Value::Object(schema)
}

pub(super) fn input_schema(tool: &CommandTool) -> Value {
    let names = inputs_of(&tool.words);

    if names.is_empty() {
        return json!({ "type": "object", "properties": {} });
    }

    let properties: Map<String, Value> = names
        .iter()
        .map(|name| (name.clone(), property(tool.inputs.get(name), starts_word(&tool.words, name))))
        .collect();
    let required: Vec<&String> =
        names.iter().filter(|name| !tool.inputs.get(*name).is_some_and(Input::is_optional)).collect();

    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}
