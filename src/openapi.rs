//! Serves the OpenAPI document (`openapi.yaml`, embedded at build time) with `servers` set to this
//! instance's BASE_URL, as JSON and YAML.

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::Value;

use crate::AppState;

pub const SPEC_YAML: &str = include_str!("../openapi.yaml");

pub fn load(base_url: &str) -> anyhow::Result<Value> {
    let mut spec: Value = serde_yaml::from_str(SPEC_YAML)?;
    spec["servers"] =
        serde_json::json!([{ "url": base_url, "description": "This Listaculous server" }]);
    Ok(spec)
}

pub async fn json(State(st): State<AppState>) -> impl IntoResponse {
    Json(st.openapi.as_ref().clone())
}

pub async fn yaml(State(st): State<AppState>) -> impl IntoResponse {
    let body = serde_yaml::to_string(st.openapi.as_ref()).unwrap_or_default();
    (
        [(header::CONTENT_TYPE, "application/yaml; charset=utf-8")],
        body,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    /// Every route registered in main.rs must be documented, and the spec must not document
    /// routes that don't exist. Keeps the docs agents rely on from drifting.
    #[test]
    fn spec_matches_router() {
        let spec = super::load("http://localhost").expect("openapi.yaml parses");
        let documented: BTreeSet<(String, String)> = spec["paths"]
            .as_object()
            .unwrap()
            .iter()
            .flat_map(|(path, item)| {
                item.as_object()
                    .unwrap()
                    .keys()
                    .filter(|m| ["get", "post", "put", "delete", "patch"].contains(&m.as_str()))
                    .map(move |m| (m.to_uppercase(), path.clone()))
            })
            .collect();

        let main = include_str!("main.rs");
        let method = regex_lite::Regex::new(r"\b(get|post|put|delete|patch)\(").unwrap();
        let mut registered = BTreeSet::new();
        // Routes before `let public_api` live under the /api nest.
        let api_end = main.find("let public_api").unwrap();
        let mut offset = 0;
        for chunk in main.split(".route(").skip(1) {
            offset = main[offset..]
                .find(".route(")
                .map(|i| offset + i + 7)
                .unwrap();
            // The route's arguments end at the next builder call or statement end.
            let end = [".route(", ".layer(", ".fallback", ".nest(", ".merge(", ";"]
                .iter()
                .filter_map(|t| chunk.find(t))
                .min()
                .unwrap_or(chunk.len());
            let args = &chunk[..end];
            let path = args.split('"').nth(1).unwrap();
            if path == "/healthz" || path.starts_with("/openapi") || path == "/docs" {
                continue;
            }
            let full = if offset < api_end {
                format!("/api{path}")
            } else {
                path.to_string()
            };
            for m in method.captures_iter(args) {
                registered.insert((m[1].to_uppercase(), full.clone()));
            }
        }
        // The spec documents itself implicitly.
        let missing: Vec<_> = registered.difference(&documented).collect();
        let extra: Vec<_> = documented.difference(&registered).collect();
        assert!(
            missing.is_empty(),
            "routes missing from openapi.yaml: {missing:?}"
        );
        assert!(
            extra.is_empty(),
            "openapi.yaml documents routes that don't exist: {extra:?}"
        );
    }

    #[test]
    fn every_operation_has_id_and_summary() {
        let spec = super::load("http://localhost").unwrap();
        let mut ids = std::collections::HashSet::new();
        for (path, item) in spec["paths"].as_object().unwrap() {
            for (m, op) in item.as_object().unwrap() {
                if m == "parameters" {
                    continue;
                }
                let id = op["operationId"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{m} {path} has no operationId"));
                assert!(ids.insert(id.to_string()), "duplicate operationId {id}");
                assert!(op["summary"].is_string(), "{m} {path} has no summary");
            }
        }
    }
}
