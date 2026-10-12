//! Sign-in providers for the API, part of the read **Facade**: the
//! `sso_providers` kernel setting and each provider's client secret, sealed
//! as the ecosystem secret [`SsoProvider::secret_name`] (as the SMTP
//! password is). Every change commits together with its audit row.

use serde_json::json;
use sun_core::model::AuditAction;
use sun_core::sso::{MAX_SSO_PROVIDERS, SsoProvider};
use sun_core::{Error, Result};
use sun_store::{AuditRepo, SecretRepo, SettingRepo, Tx};

use crate::Engine;
use crate::knowledge::ecosystem_scope;

/// `kernel_setting` key of the provider list.
const SSO: &str = "sso_providers";

/// The stored list; `None` until the first save (or import).
async fn load(tx: &mut Tx) -> Result<Option<Vec<SsoProvider>>> {
    match tx.setting(SSO).await? {
        Some(v) => serde_json::from_value(v)
            .map(Some)
            .map_err(|e| Error::InvalidInput(format!("sso_providers: {e}"))),
        None => Ok(None),
    }
}

/// Stores `providers` and, when given, `secret` for `provider`, audited as
/// `actor` (never the secret).
async fn save(
    tx: &mut Tx,
    providers: &[SsoProvider],
    provider: &SsoProvider,
    secret: Option<&str>,
    actor: &str,
) -> Result<()> {
    tx.put_setting(SSO, &json!(providers)).await?;
    let scope = ecosystem_scope()?;
    let note = match secret {
        Some(secret) => {
            tx.set_secret(&scope, &provider.secret_name(), secret)
                .await?;
            ", secret set"
        }
        None => "",
    };
    let detail = format!("sso {} {} saved{note}", provider.id, provider.kind);
    tx.append_audit(actor, AuditAction::SettingsChange, SSO, &detail)
        .await?;
    Ok(())
}

impl Engine {
    /// Every provider, with whether its client secret is stored.
    pub async fn sso_providers(&self) -> Result<Vec<(SsoProvider, bool)>> {
        let mut tx = self.store.read().await?;
        let providers = load(&mut tx).await?.unwrap_or_default();
        let secrets = tx.list_secrets(Some(&ecosystem_scope()?)).await?;
        Ok(providers
            .into_iter()
            .map(|p| {
                let has = secrets.iter().any(|s| s.name == p.secret_name());
                (p, has)
            })
            .collect())
    }

    /// Provider `id` and its client secret (for the sign-in flow only).
    pub async fn sso_provider(&self, id: &str) -> Result<Option<(SsoProvider, Option<String>)>> {
        let mut tx = self.store.read().await?;
        let Some(provider) = load(&mut tx)
            .await?
            .unwrap_or_default()
            .into_iter()
            .find(|p| p.id == id)
        else {
            return Ok(None);
        };
        let secret = match tx
            .secret_value(&ecosystem_scope()?, &provider.secret_name())
            .await
        {
            Ok(value) => Some(value),
            Err(e) => match Error::from(e) {
                Error::NotFound(_) => None,
                e => return Err(e),
            },
        };
        Ok(Some((provider, secret)))
    }

    /// Adds or replaces (by id) `provider` after [`SsoProvider::normalized`];
    /// `Some(secret)` seals the client secret, `None` keeps the stored one.
    /// Audited as `actor` (never the secret).
    pub async fn put_sso_provider(
        &self,
        provider: SsoProvider,
        secret: Option<&str>,
        actor: &str,
    ) -> Result<SsoProvider> {
        let provider = provider.normalized()?;
        let mut tx = self.store.begin().await?;
        let mut providers = load(&mut tx).await?.unwrap_or_default();
        match providers.iter().position(|p| p.id == provider.id) {
            Some(at) => providers[at] = provider.clone(),
            None if providers.len() >= MAX_SSO_PROVIDERS => {
                return Err(Error::InvalidInput(format!(
                    "Keep at most {MAX_SSO_PROVIDERS} sign-in providers."
                )));
            }
            None => providers.push(provider.clone()),
        }
        save(&mut tx, &providers, &provider, secret, actor).await?;
        tx.commit().await?;
        Ok(provider)
    }

    /// Removes provider `id` and its client secret, audited. Members it
    /// linked keep their accounts (and can sign in with a password).
    pub async fn delete_sso_provider(&self, id: &str, actor: &str) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let mut providers = load(&mut tx).await?.unwrap_or_default();
        let Some(at) = providers.iter().position(|p| p.id == id) else {
            return Err(Error::NotFound(format!("sign-in provider {id}")));
        };
        let gone = providers.remove(at);
        tx.put_setting(SSO, &json!(providers)).await?;
        tx.delete_secret(&ecosystem_scope()?, &gone.secret_name())
            .await?;
        let detail = format!("sso {} {} removed", gone.id, gone.kind);
        tx.append_audit(actor, AuditAction::SettingsChange, SSO, &detail)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The one-time import of `sun.yaml`'s old `oidc:` entry: saves
    /// `provider` and `secret` only while no provider list was ever saved,
    /// so a provider an admin later removed doesn't come back. True when it
    /// imported.
    pub async fn import_sso_provider(
        &self,
        provider: SsoProvider,
        secret: Option<&str>,
    ) -> Result<bool> {
        let provider = provider.normalized()?;
        let mut tx = self.store.begin().await?;
        if load(&mut tx).await?.is_some() {
            return Ok(false);
        }
        let providers = [provider.clone()];
        save(&mut tx, &providers, &provider, secret, "system:sun.yaml").await?;
        tx.commit().await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sun_core::HostId;
    use sun_core::sso::SsoKind;
    use sun_store::Store;

    use super::*;
    use crate::gates::tests::{LocalHost, mods};

    #[tokio::test]
    async fn sso_import_runs_once_and_secrets_stay_sealed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let config = crate::EngineConfig {
            host: HostId::generate(),
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: crate::WatchdogConfig::default(),
        };
        let host = LocalHost::new(dir.path(), |_, _| {});
        let (engine, _task) = Engine::start(store, host, mods(json!({})), config)
            .await
            .unwrap();
        let provider = SsoProvider {
            id: "oidc".into(),
            kind: SsoKind::Oidc,
            name: "Keycloak".into(),
            client_id: "sun".into(),
            issuer_url: Some("https://sso.acme.io/realms/a".into()),
            allowed_domains: vec![],
        };
        let imported = engine.import_sso_provider(provider.clone(), Some("s3cret"));
        assert!(imported.await.unwrap());
        let (stored, secret) = engine.sso_provider("oidc").await.unwrap().unwrap();
        assert_eq!(
            (stored, secret.as_deref()),
            (provider.clone(), Some("s3cret"))
        );
        // Removed by an admin: a restart doesn't import it again.
        engine.delete_sso_provider("oidc", "user:me").await.unwrap();
        assert!(!engine.import_sso_provider(provider, None).await.unwrap());
        assert!(engine.sso_providers().await.unwrap().is_empty());
        assert!(engine.sso_provider("oidc").await.unwrap().is_none());
    }
}
