//! A small blocking client of the kernel's REST API (`sun_api` routes)
//! for the `task` and `workflow` commands: every write goes through the
//! running kernel, never around it.

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// The kernel URL and the token to call it with.
pub struct Client {
    base: String,
    token: String,
    agent: ureq::Agent,
}

impl Client {
    /// A client of `base` (e.g. `http://127.0.0.1:7700`).
    pub fn new(base: &str, token: &str) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            base: base.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            agent,
        }
    }

    /// The kernel URL, without a trailing `/`.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The id of the workspace whose id or name (any case) is `workspace`.
    pub fn workspace_id(&self, workspace: &str) -> Result<String> {
        self.get("/workspaces")?
            .as_array()
            .into_iter()
            .flatten()
            .find(|b| {
                b["id"] == workspace
                    || b["slug"] == workspace
                    || b["name"]
                        .as_str()
                        .is_some_and(|n| n.eq_ignore_ascii_case(workspace))
            })
            .and_then(|b| b["id"].as_str())
            .map(str::to_owned)
            .with_context(|| format!("no workspace {workspace}: see `sun workspace list`"))
    }

    /// `GET /api/v1<path>`.
    pub fn get(&self, path: &str) -> Result<Value> {
        let response = self
            .agent
            .get(format!("{}/api/v1{path}", self.base))
            .header("authorization", format!("Bearer {}", self.token))
            .call();
        answer(response)
    }

    /// `POST /api/v1<path>` with a JSON body.
    pub fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let response = self
            .agent
            .post(format!("{}/api/v1{path}", self.base))
            .header("authorization", format!("Bearer {}", self.token))
            .send_json(body);
        answer(response)
    }

    /// `PUT /api/v1<path>` with a JSON body.
    pub fn put(&self, path: &str, body: &Value) -> Result<Value> {
        let response = self
            .agent
            .put(format!("{}/api/v1{path}", self.base))
            .header("authorization", format!("Bearer {}", self.token))
            .send_json(body);
        answer(response)
    }

    /// `PATCH /api/v1<path>` with a JSON body.
    pub fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        let response = self
            .agent
            .patch(format!("{}/api/v1{path}", self.base))
            .header("authorization", format!("Bearer {}", self.token))
            .send_json(body);
        answer(response)
    }

    /// `DELETE /api/v1<path>`.
    pub fn delete(&self, path: &str) -> Result<Value> {
        let response = self
            .agent
            .delete(format!("{}/api/v1{path}", self.base))
            .header("authorization", format!("Bearer {}", self.token))
            .call();
        answer(response)
    }
}

fn answer(response: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<Value> {
    let mut response = match response {
        Ok(response) => response,
        Err(e) => bail!("cannot reach the kernel ({e}); is `sun serve` running?"),
    };
    let status = response.status();
    let body: Value = response.body_mut().read_json().unwrap_or(Value::Null);
    if !status.is_success() {
        let message = body["error"]["message"].as_str().unwrap_or("no details");
        bail!("{status}: {message}");
    }
    Ok(body)
}
