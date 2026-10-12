//! Sign-in providers admins manage in Settings → Sign-in: Google,
//! GitHub and any OIDC provider. Stored as the `sso_providers` kernel
//! setting; each client secret is the ecosystem secret
//! [`SsoProvider::secret_name`], never part of this.

use serde::{Deserialize, Serialize};

use crate::Error;

/// Google's OIDC issuer; a `google` provider always uses it.
pub const GOOGLE_ISSUER: &str = "https://accounts.google.com";
/// The member issuer of a GitHub identity (its subject is the numeric user id).
pub const GITHUB_ISSUER: &str = "github";
/// Most providers one install keeps.
pub const MAX_SSO_PROVIDERS: usize = 20;

string_enum! {
    /// How a provider signs people in.
    pub enum SsoKind {
        /// OIDC with the issuer [`GOOGLE_ISSUER`].
        Google => "google",
        /// GitHub's OAuth2 code flow plus its primary verified email.
        Github => "github",
        /// Any OIDC provider with discovery (Keycloak, Authentik, Okta, …).
        Oidc => "oidc",
    }
}

/// One sign-in provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SsoProvider {
    /// `a-z 0-9 _`, 1–32 chars, starting with a letter; part of the
    /// callback URL `/api/v1/auth/sso/{id}/callback`.
    pub id: String,
    /// How it signs people in.
    pub kind: SsoKind,
    /// Shown on the login button: "Continue with {name}".
    pub name: String,
    /// Client id at the provider.
    pub client_id: String,
    /// The issuer, `oidc` only (discovery reads
    /// `<issuer>/.well-known/openid-configuration`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer_url: Option<String>,
    /// Lowercase email domains allowed in (a verified email is then
    /// required). Empty: any invited member the provider verifies.
    #[serde(default)]
    pub allowed_domains: Vec<String>,
}

impl SsoProvider {
    /// The ecosystem secret holding the client secret, e.g. `SSO_GOOGLE_SECRET`.
    pub fn secret_name(&self) -> String {
        format!("SSO_{}_SECRET", self.id.to_ascii_uppercase())
    }

    /// Trims the fields, lowercases the domains, keeps `issuer_url` only for
    /// `oidc` (where it is required, kept as entered: a trailing slash is part
    /// of the issuer the provider's tokens name), and checks every field.
    pub fn normalized(mut self) -> Result<Self, Error> {
        let bad = |m: &str| Err(Error::InvalidInput(m.into()));
        let text_ok =
            |s: &str, max: usize| (1..=max).contains(&s.len()) && !s.chars().any(char::is_control);
        self.id = self.id.trim().to_owned();
        self.name = self.name.trim().to_owned();
        self.client_id = self.client_id.trim().to_owned();
        let id_ok = (1..=32).contains(&self.id.len())
            && self.id.starts_with(|c: char| c.is_ascii_lowercase())
            && self
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !id_ok {
            return bad("A provider id is 1 to 32 of a-z, 0-9 and _, starting with a letter.");
        }
        if !text_ok(&self.name, 64) {
            return bad("Give the provider a name of 1 to 64 characters.");
        }
        if !text_ok(&self.client_id, 256) {
            return bad("Enter the client id from the provider.");
        }
        self.issuer_url = match self.kind {
            SsoKind::Oidc => {
                let url = self.issuer_url.as_deref().unwrap_or("").trim();
                let url_ok = text_ok(url, 512)
                    && (url.starts_with("https://") || url.starts_with("http://"))
                    && !url.contains(char::is_whitespace);
                if !url_ok {
                    return bad(
                        "Enter the issuer URL, such as https://sso.example.com/realms/acme.",
                    );
                }
                Some(url.to_owned())
            }
            SsoKind::Google | SsoKind::Github => None,
        };
        let mut domains = Vec::new();
        for domain in &self.allowed_domains {
            let domain = domain.trim().trim_start_matches('@').to_ascii_lowercase();
            let ok = (1..=253).contains(&domain.len())
                && domain.contains('.')
                && domain
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-.".contains(c));
            if !ok {
                return bad("Allowed domains are host names such as example.com.");
            }
            if !domains.contains(&domain) {
                domains.push(domain);
            }
        }
        if domains.len() > 50 {
            return bad("List at most 50 allowed domains.");
        }
        self.allowed_domains = domains;
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(kind: SsoKind, id: &str, issuer: Option<&str>) -> SsoProvider {
        SsoProvider {
            id: id.into(),
            kind,
            name: " Acme ".into(),
            client_id: "client".into(),
            issuer_url: issuer.map(Into::into),
            allowed_domains: vec!["@Acme.io".into(), "acme.io".into()],
        }
    }

    #[test]
    fn sso_provider_normalizes_and_refuses_bad_fields() {
        let google = provider(SsoKind::Google, "google", Some("https://evil.test"))
            .normalized()
            .unwrap();
        assert_eq!(google.issuer_url, None);
        assert_eq!(google.name, "Acme");
        assert_eq!(google.allowed_domains, ["acme.io"]);
        assert_eq!(google.secret_name(), "SSO_GOOGLE_SECRET");
        assert!(crate::model::Secret::validate_name(&google.secret_name()).is_ok());

        let oidc = provider(SsoKind::Oidc, "kc_1", Some("https://sso.acme.io/realms/a/"))
            .normalized()
            .unwrap();
        assert_eq!(
            oidc.issuer_url.as_deref(),
            Some("https://sso.acme.io/realms/a/")
        );
        let github = provider(SsoKind::Github, "gh", Some("https://x.test"));
        assert_eq!(github.normalized().unwrap().issuer_url, None);

        for bad in [
            provider(SsoKind::Oidc, "kc", None),
            provider(SsoKind::Oidc, "kc", Some("ftp://x")),
            provider(SsoKind::Github, "Git-Hub", None),
            provider(SsoKind::Github, "1gh", None),
            provider(SsoKind::Github, "", None),
            SsoProvider {
                allowed_domains: vec!["acme io".into()],
                ..provider(SsoKind::Github, "gh", None)
            },
            SsoProvider {
                client_id: " ".into(),
                ..provider(SsoKind::Github, "gh", None)
            },
        ] {
            assert!(bad.clone().normalized().is_err(), "{bad:?}");
        }
    }
}
