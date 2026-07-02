//! JSON Schema generation for typed tool parameters.
//!
//! One `Params` struct is the single source of truth: schemars derives the
//! wire schema here, serde derives the argument parsing in the blanket
//! [`ErasedTool`](super::ErasedTool) impl. DeepSeek's function calling expects
//! OpenAI-style schemas — draft-07 keywords, no `$ref` indirection, no
//! `$schema` header — so schemars output is normalized accordingly.

use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde_json::Value;

pub(crate) fn parameters_schema<P: JsonSchema>() -> Value {
    let generator = SchemaSettings::draft07()
        .with(|settings| {
            settings.meta_schema = None;
            settings.inline_subschemas = true;
        })
        .into_generator();
    let mut value = generator.into_root_schema_for::<P>().to_value();
    normalize(&mut value, true);
    value
}

fn normalize(value: &mut Value, top_level: bool) {
    match value {
        Value::Object(object) => {
            object.remove("$schema");
            // The derive emits the struct name as `title` — noise for the model.
            object.remove("title");
            // `Option<T>` emits `{"type": ["X", "null"]}`; collapse to the
            // hand-written style `{"type": "X"}` (absence from `required`
            // already expresses optionality to the model).
            if let Some(Value::Array(types)) = object.get_mut("type") {
                types.retain(|entry| entry != "null");
                if types.len() == 1 {
                    let only = types[0].clone();
                    object.insert("type".to_string(), only);
                }
            }
            if top_level
                && object.get("type").is_some_and(|kind| kind == "object")
                && !object.contains_key("additionalProperties")
            {
                object.insert("additionalProperties".to_string(), Value::Bool(false));
            }
            for child in object.values_mut() {
                normalize(child, false);
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize(item, false);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct SampleParams {
        /// The path.
        path: String,
        /// Max lines.
        max_lines: Option<u64>,
    }

    #[test]
    fn generates_flat_draft07_style_schema() {
        let schema = parameters_schema::<SampleParams>();
        assert_eq!(schema["type"], "object");
        assert!(schema.get("$schema").is_none());
        assert!(schema.get("title").is_none());
        assert_eq!(schema["additionalProperties"], Value::Bool(false));
        assert_eq!(schema["required"], json!(["path"]));
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(schema["properties"]["path"]["description"], "The path.");
        // Option<u64> collapses to plain "integer".
        assert_eq!(schema["properties"]["max_lines"]["type"], "integer");
    }

    #[test]
    fn emits_no_refs_or_definitions() {
        let schema = parameters_schema::<SampleParams>();
        let text = schema.to_string();
        assert!(!text.contains("$ref"));
        assert!(!text.contains("definitions"));
    }
}
