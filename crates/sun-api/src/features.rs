//! Built-in features an operator can turn off (sun.yaml `features:`).
//! A feature that is off answers 404 on all of its routes; `sun-cli` also
//! skips its background workers, and the UI drops its entries
//! (`GET /features`). `billing` also needs Sun Cloud (`SUN_CLOUD_URL` +
//! `SUN_CLOUD_TOKEN`): `GET /features` says `false` without it, whatever
//! `sun.yaml` says.

use axum::Json;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::Api;
use crate::error::ApiError;

/// Which built-in features run. Every one defaults to on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Features {
    /// Knowledge sources, search and embeddings.
    pub knowledge: bool,
    /// The design canvas: previews and pins.
    pub canvas: bool,
    /// Evals: replays against a candidate memory or workflow.
    pub evals: bool,
    /// The runtime marketplace.
    pub market: bool,
    /// The skills and MCP catalog.
    pub catalog: bool,
    /// Plans per workspace, through Sun Cloud.
    pub billing: bool,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            knowledge: true,
            canvas: true,
            evals: true,
            market: true,
            catalog: true,
            billing: true,
        }
    }
}

impl Features {
    /// The feature that owns `path`, when it is off.
    fn off_for(&self, path: &str) -> Option<&'static str> {
        let api = path.strip_prefix("/api/v1/");
        let segments: Vec<&str> = api.unwrap_or(path).split('/').collect();
        let owner = match (api.is_some(), segments.as_slice()) {
            (false, ["", "preview", ..]) => "canvas",
            (true, ["knowledge", ..] | ["settings", "embeddings"]) => "knowledge",
            (true, ["tasks", _, "previews" | "pins" | "screens" | "design", ..]) => "canvas",
            (true, ["evals" | "eval_sets", ..]) => "evals",
            (true, ["marketplace", ..]) => "market",
            (true, ["catalog", ..]) => "catalog",
            (true, ["subscriptions", ..]) => "billing",
            _ => return None,
        };
        let on = match owner {
            "knowledge" => self.knowledge,
            "canvas" => self.canvas,
            "evals" => self.evals,
            "market" => self.market,
            "billing" => self.billing,
            _ => self.catalog,
        };
        (!on).then_some(owner)
    }
}

/// Middleware: 404 for every route of a feature that is off.
pub(crate) async fn guard(State(api): State<Api>, request: Request, next: Next) -> Response {
    match api.features.off_for(request.uri().path()) {
        Some(feature) => ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("The {feature} feature is off on this kernel (sun.yaml `features.{feature}`)."),
        )
        .into_response(),
        None => next.run(request).await,
    }
}

/// `GET /api/v1/features`: which features run, for the UI's navigation.
pub(crate) async fn get(State(api): State<Api>) -> Json<Features> {
    Json(Features {
        billing: api.features.billing && api.cloud.is_some(),
        ..api.features
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_features_own_their_paths_only() {
        let off = Features {
            knowledge: false,
            canvas: false,
            evals: false,
            market: false,
            catalog: false,
            billing: false,
        };
        for (path, owner) in [
            ("/api/v1/knowledge/sources", Some("knowledge")),
            ("/api/v1/settings/embeddings", Some("knowledge")),
            ("/api/v1/tasks/T1/pins/P1", Some("canvas")),
            ("/api/v1/tasks/T1/previews", Some("canvas")),
            ("/preview/abc/index.html", Some("canvas")),
            ("/api/v1/eval_sets", Some("evals")),
            ("/api/v1/marketplace/leases", Some("market")),
            ("/api/v1/catalog/x/install", Some("catalog")),
            ("/api/v1/subscriptions/W1/summary", Some("billing")),
            ("/api/v1/internal/workspaces/W1/seat_count", None),
            ("/api/v1/tasks/T1/context", None),
            ("/api/v1/tasks", None),
            ("/knowledge", None),
        ] {
            assert_eq!(off.off_for(path), owner, "{path}");
        }
        assert_eq!(
            Features::default().off_for("/api/v1/knowledge/search"),
            None
        );
    }
}
