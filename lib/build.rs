use std::{env, fs, path::PathBuf};

use serde_json::{Map, Value};
const OPENAPI_SPEC_URL: &str = "https://raw.githubusercontent.com/anomalyco/opencode/c98f61638535c9cc57a2b710decc780f7289fc2f/packages/sdk/openapi.json";
const GENERATED_FILE_NAME: &str = "opencode_client.rs";

fn main() {
    println!("cargo:rerun-if-env-changed=OPENCODE_OPENAPI_SPEC_URL");

    let spec_url = env::var("OPENCODE_OPENAPI_SPEC_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| OPENAPI_SPEC_URL.to_string());

    if spec_url.contains("/refs/heads/") {
        println!(
            "cargo:warning=Using a branch-based OpenAPI URL ({spec_url}). For reproducible builds, prefer a commit-pinned URL via OPENCODE_OPENAPI_SPEC_URL."
        );
    }

    let response = reqwest::blocking::get(&spec_url)
        .unwrap_or_else(|error| panic!("failed to download OpenAPI spec from {spec_url}: {error}"));
    let response = response
        .error_for_status()
        .unwrap_or_else(|error| panic!("OpenAPI spec request failed for {spec_url}: {error}"));
    let mut spec_json = response
        .json::<Value>()
        .unwrap_or_else(|error| panic!("failed to decode OpenAPI JSON from {spec_url}: {error}"));

    normalize_openapi_for_progenitor(&mut spec_json);
    simplify_operation_responses(&mut spec_json);

    let spec = serde_json::from_value::<openapiv3::OpenAPI>(spec_json)
        .unwrap_or_else(|error| panic!("failed to parse OpenAPI spec from {spec_url}: {error}"));

    let mut generator = progenitor::Generator::default();
    let tokens = generator.generate_tokens(&spec).unwrap_or_else(|error| {
        panic!("failed to generate OpenAPI client from {spec_url}: {error}")
    });
    let syntax_tree = syn::parse2(tokens)
        .unwrap_or_else(|error| panic!("failed to parse generated OpenAPI client tokens: {error}"));
    let formatted = prettyplease::unparse(&syntax_tree);

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR should be set by cargo"));
    let destination = out_dir.join(GENERATED_FILE_NAME);
    fs::write(&destination, formatted).unwrap_or_else(|error| {
        panic!(
            "failed to write generated OpenAPI client to {}: {error}",
            destination.display()
        )
    });
}

fn normalize_openapi_for_progenitor(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for nested in map.values_mut() {
                normalize_openapi_for_progenitor(nested);
            }

            for union_key in ["anyOf", "oneOf"] {
                rewrite_nullable_union(map, union_key);
            }

            if let Some(openapi_version) = map.get_mut("openapi") {
                *openapi_version = Value::String("3.0.3".to_string());
            }

            if let Some(number) = map.get("exclusiveMinimum").and_then(Value::as_f64) {
                map.entry("minimum".to_string())
                    .or_insert_with(|| Value::from(number));
                map.insert("exclusiveMinimum".to_string(), Value::Bool(true));
            }

            if let Some(number) = map.get("exclusiveMaximum").and_then(Value::as_f64) {
                map.entry("maximum".to_string())
                    .or_insert_with(|| Value::from(number));
                map.insert("exclusiveMaximum".to_string(), Value::Bool(true));
            }

            if let Some(const_value) = map.remove("const") {
                map.entry("enum".to_string())
                    .or_insert_with(|| Value::Array(vec![const_value]));
            }

            map.remove("$schema");
        }
        Value::Array(values) => {
            for nested in values {
                normalize_openapi_for_progenitor(nested);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn simplify_operation_responses(spec: &mut Value) {
    let Some(paths) = spec
        .as_object_mut()
        .and_then(|root| root.get_mut("paths"))
        .and_then(Value::as_object_mut)
    else {
        return;
    };

    for path_item in paths.values_mut() {
        let Some(path_object) = path_item.as_object_mut() else {
            continue;
        };

        for method in [
            "get", "put", "post", "delete", "options", "head", "patch", "trace",
        ] {
            let Some(operation) = path_object.get_mut(method).and_then(Value::as_object_mut) else {
                continue;
            };
            let Some(responses) = operation
                .get_mut("responses")
                .and_then(Value::as_object_mut)
            else {
                continue;
            };

            let success_keys = responses
                .keys()
                .filter(|status| is_success_status(status))
                .cloned()
                .collect::<Vec<_>>();
            if let Some(keep_key) = pick_preferred_success_status(&success_keys) {
                for status in success_keys {
                    if status != keep_key {
                        responses.remove(&status);
                    }
                }
                simplify_response_content_type(responses, &keep_key);
            }

            let error_keys = responses
                .keys()
                .filter(|status| is_error_or_default_status(status))
                .cloned()
                .collect::<Vec<_>>();
            if let Some(keep_key) = pick_preferred_error_status(&error_keys) {
                for status in error_keys {
                    if status != keep_key {
                        responses.remove(&status);
                    }
                }
                simplify_response_content_type(responses, &keep_key);
            }
        }
    }
}

fn is_success_status(status: &str) -> bool {
    if status == "default" {
        return false;
    }
    if let Ok(code) = status.parse::<u16>() {
        return (200..300).contains(&code);
    }
    status.starts_with('2')
}

fn is_error_or_default_status(status: &str) -> bool {
    if status == "default" {
        return true;
    }
    if let Ok(code) = status.parse::<u16>() {
        return (400..600).contains(&code);
    }
    status.starts_with('4') || status.starts_with('5')
}

fn pick_preferred_success_status(statuses: &[String]) -> Option<String> {
    statuses
        .iter()
        .min_by_key(|status| {
            if status.as_str() == "200" {
                return 0_u16;
            }
            status
                .parse::<u16>()
                .ok()
                .filter(|code| (200..300).contains(code))
                .unwrap_or(299)
                + 1
        })
        .cloned()
}

fn pick_preferred_error_status(statuses: &[String]) -> Option<String> {
    if statuses.iter().any(|status| status == "default") {
        return Some("default".to_string());
    }

    statuses
        .iter()
        .min_by_key(|status| {
            status
                .parse::<u16>()
                .ok()
                .filter(|code| (400..600).contains(code))
                .unwrap_or(599)
        })
        .cloned()
}

fn simplify_response_content_type(responses: &mut Map<String, Value>, status_key: &str) {
    let Some(response) = responses.get_mut(status_key).and_then(Value::as_object_mut) else {
        return;
    };
    let Some(content) = response.get_mut("content").and_then(Value::as_object_mut) else {
        return;
    };

    let media_types = content.keys().cloned().collect::<Vec<_>>();
    let Some(preferred_media_type) = pick_preferred_media_type(&media_types) else {
        return;
    };
    for media_type in media_types {
        if media_type != preferred_media_type {
            content.remove(&media_type);
        }
    }
}

fn pick_preferred_media_type(media_types: &[String]) -> Option<String> {
    if media_types
        .iter()
        .any(|media_type| media_type == "application/json")
    {
        return Some("application/json".to_string());
    }
    if media_types
        .iter()
        .any(|media_type| media_type == "text/event-stream")
    {
        return Some("text/event-stream".to_string());
    }
    media_types.first().cloned()
}

fn rewrite_nullable_union(map: &mut Map<String, Value>, union_key: &str) {
    let union = map.get(union_key).cloned();
    let Some(Value::Array(items)) = union else {
        return;
    };

    let mut non_null = Vec::new();
    let mut had_null = false;
    for item in items {
        if is_null_schema(&item) {
            had_null = true;
        } else {
            non_null.push(item);
        }
    }

    if !had_null {
        return;
    }

    map.remove(union_key);

    if non_null.len() == 1 {
        if let Value::Object(mut inner) = non_null.pop().expect("len checked") {
            if !inner.contains_key("nullable") {
                inner.insert("nullable".to_string(), Value::Bool(true));
            }

            let outer_entries = map.clone();
            for (key, value) in outer_entries {
                inner.entry(key).or_insert(value);
            }
            *map = inner;
        } else {
            map.insert(union_key.to_string(), Value::Array(non_null));
            map.insert("nullable".to_string(), Value::Bool(true));
        }
        return;
    }

    map.insert(union_key.to_string(), Value::Array(non_null));
    map.insert("nullable".to_string(), Value::Bool(true));
}

fn is_null_schema(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            let Some(Value::String(schema_type)) = map.get("type") else {
                return false;
            };
            schema_type == "null"
        }
        _ => false,
    }
}
