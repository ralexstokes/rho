use serde_json::Value;

use crate::{ToolArgumentError, ToolDefinition};

/// Invalid tool schema.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid JSON Schema: {message}")]
pub struct SchemaError {
    /// Validation or compilation detail.
    pub message: String,
}

/// Validates a tool definition without resolving external files or URLs.
pub fn validate_tool_definition(definition: &ToolDefinition) -> Result<(), SchemaError> {
    if let Some(meta_schema) = definition.parameters.get("$schema") {
        let supported = matches!(
            meta_schema.as_str(),
            Some("https://json-schema.org/draft/2020-12/schema")
                | Some("https://json-schema.org/draft/2020-12/schema#")
        );
        if !supported {
            return Err(SchemaError {
                message: format!("unsupported $schema declaration: {meta_schema}"),
            });
        }
    }
    jsonschema::draft202012::meta::validate(&definition.parameters).map_err(|error| {
        SchemaError {
            message: error.to_string(),
        }
    })?;
    jsonschema::draft202012::options()
        .build(&definition.parameters)
        .map(|_| ())
        .map_err(|error| SchemaError {
            message: error.to_string(),
        })
}

/// Validates parsed arguments against one tool's JSON Schema without coercion.
pub fn validate_tool_arguments(
    definition: &ToolDefinition,
    arguments: &Value,
) -> Result<(), ToolArgumentError> {
    let validator = jsonschema::draft202012::options()
        .build(&definition.parameters)
        .map_err(|error| ToolArgumentError {
            kind: "invalid_schema".to_owned(),
            message: error.to_string(),
        })?;
    let mut errors = validator
        .iter_errors(arguments)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    // `jsonschema` uses hash-backed internal indexes. Error ordering is not part
    // of validation semantics, so normalize it before it crosses the pure
    // boundary or becomes journal-visible provider output.
    errors.sort_unstable();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ToolArgumentError {
            kind: "schema_validation".to_owned(),
            message: errors.join("; "),
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn count_tool() -> ToolDefinition {
        ToolDefinition::new(
            "count",
            "Count exactly this many items.",
            json!({
                "type": "object",
                "properties": { "count": { "type": "integer", "minimum": 0 } },
                "required": ["count"],
                "additionalProperties": false
            }),
        )
    }

    #[test]
    fn valid_arguments_pass_without_conversion() {
        let definition = count_tool();
        validate_tool_definition(&definition).unwrap();
        validate_tool_arguments(&definition, &json!({"count": 5})).unwrap();
    }

    #[test]
    fn string_is_not_coerced_to_integer() {
        let error = validate_tool_arguments(&count_tool(), &json!({"count": "5"})).unwrap_err();
        assert_eq!(error.kind, "schema_validation");
    }

    #[test]
    fn invalid_schema_is_rejected() {
        let definition = ToolDefinition::new("bad", "bad", json!({"type": "wat"}));
        assert!(validate_tool_definition(&definition).is_err());
    }

    #[test]
    fn unknown_meta_schema_is_rejected_without_panicking() {
        let definition = ToolDefinition::new(
            "bad",
            "bad",
            json!({"$schema": "https://example.com/custom", "type": "object"}),
        );
        assert!(validate_tool_definition(&definition).is_err());
    }

    #[test]
    fn multiple_validation_errors_have_stable_order() {
        let error =
            validate_tool_arguments(&count_tool(), &json!({"count": -1, "unexpected": true}))
                .unwrap_err();
        let messages = error.message.split("; ").collect::<Vec<_>>();
        assert!(messages.windows(2).all(|pair| pair[0] <= pair[1]));
    }
}
