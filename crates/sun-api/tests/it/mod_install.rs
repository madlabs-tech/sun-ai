//! `POST /api/v1/mods/{name}/install` at its permission boundary: a caller
//! without the manage permission is refused before the installer runs, and a
//! permission set other than the listed one is refused by the installer's
//! consent check.
#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::json;
use sun_core::model::{ModInstall, ModKind};
use sun_core::{AvailableMod, Error, GroupId, ModInstaller, Result};

use crate::common::kernel;
use crate::workspaces::{call, call_as, member_token};

const ASKS: &str = "secret:GITHUB_TOKEN";

/// Lists `github`; installs only with exactly its listed permissions (the
/// [`ModInstaller`] consent contract) and records every call.
#[derive(Default)]
struct Installer(Mutex<Vec<Vec<String>>>);

#[async_trait]
impl ModInstaller for Installer {
    async fn available(&self) -> Result<Vec<AvailableMod>> {
        Ok(vec![AvailableMod {
            name: "github".into(),
            kind: ModKind::Plugin,
            version: "0.1.0".into(),
            permissions: vec![ASKS.into()],
            signed: false,
            description: "GitHub.".into(),
            repo: "https://github.com/madlabs-tech/sun-plugin-github".into(),
            tier: "official".into(),
        }])
    }

    async fn install(&self, name: &str, granted: &[String], _actor: &str) -> Result<ModInstall> {
        self.0.lock().unwrap().push(granted.to_vec());
        if granted != [ASKS] {
            return Err(Error::InvalidInput("permissions differ".into()));
        }
        Ok(ModInstall {
            name: name.into(),
            kind: ModKind::Plugin,
            version: "0.1.0".into(),
            manifest: json!({}),
            granted_permissions: granted.to_vec(),
            package_digest: None,
            signed_by: None,
        })
    }

    async fn remove(&self, name: &str, _purge: bool, _actor: &str) -> Result<Vec<String>> {
        Err(Error::NotFound(format!("mod {name}")))
    }
}

#[tokio::test]
async fn mod_install_refuses_non_admins_and_a_changed_permission_set() {
    let k = kernel().await;
    let installer = Arc::new(Installer::default());
    let app = sun_api::router(k.api.clone().with_mod_installer(installer.clone()));
    let path = "/api/v1/mods/github/install";
    let right = json!({ "permissions": [ASKS] });

    for (name, groups) in [
        ("mia", vec![GroupId::MEMBER]),
        ("vic", vec![GroupId::VIEWER]),
    ] {
        let token = member_token(&k.store, name, &groups).await;
        let (status, body) = call_as(&app, &token, "POST", path, Some(right.clone())).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{name}: {body}");
    }
    assert!(
        installer.0.lock().unwrap().is_empty(),
        "refused before the installer"
    );

    // An extra permission, a missing one: the installer refuses, nothing installs.
    for wrong in [json!([ASKS, "secret:OTHER"]), json!([])] {
        let body = json!({ "permissions": wrong });
        let (status, error) = call(&app, "POST", path, Some(body)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
        assert_eq!(error["error"]["code"], "invalid_input");
    }

    let (status, row) = call(&app, "POST", path, Some(right)).await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    assert_eq!(row["permissions"], json!([ASKS]));
    assert_eq!(installer.0.lock().unwrap().len(), 3);
}
