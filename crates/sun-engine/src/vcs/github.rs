//! GitHub App **Adapter**: the App's RS256 JWT, installation tokens, the
//! installation's account and repos. The App itself (id, slug, private key,
//! webhook secret) is ecosystem-wide, set by an admin.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::{Value, json};
use sun_core::model::PrSnapshot;
use sun_core::{Error, Result, Timestamp};
use sun_store::Store;

use super::{RemoteRepo, check_url, ecosystem_secret, encode, send};

/// Ecosystem secrets of the App.
pub const APP_ID: &str = "GITHUB_APP_ID";
/// The App's URL name: `https://github.com/apps/<slug>`.
pub const APP_SLUG: &str = "GITHUB_APP_SLUG";
/// The App's private key (PEM, PKCS#1 or PKCS#8).
pub const APP_PRIVATE_KEY: &str = "GITHUB_APP_PRIVATE_KEY";
/// Signs the App's webhook deliveries (`X-Hub-Signature-256`).
pub const APP_WEBHOOK_SECRET: &str = "GITHUB_APP_WEBHOOK_SECRET";
/// What the App needs before workspaces can install it (`not_set_up` names them).
pub(crate) const REQUIRED: &[&str] = &[APP_ID, APP_SLUG, APP_PRIVATE_KEY, APP_WEBHOOK_SECRET];
/// Optional: the REST API base (GitHub Enterprise Server, tests).
pub const APP_API_URL: &str = "GITHUB_APP_API_URL";
const DEFAULT_API: &str = "https://api.github.com";
/// Where installations live; a GitHub connection's `instance_url`.
pub(crate) const WEB: &str = "https://github.com";
/// Picker page size.
const PER_PAGE: u32 = 50;
/// Pages a search reads (the installation repos endpoint can't search).
const SEARCH_PAGES: u32 = 10;

/// The App's identity, loaded per call (an admin may change it any time).
pub(crate) struct App {
    id: String,
    key: String,
    pub(crate) api: String,
}

impl App {
    /// `Unavailable` until an admin set the App's id and private key.
    pub(crate) async fn load(store: &Store) -> Result<Self> {
        let (Some(id), Some(key)) = (
            ecosystem_secret(store, APP_ID).await?,
            ecosystem_secret(store, APP_PRIVATE_KEY).await?,
        ) else {
            return Err(not_set_up());
        };
        let api = match ecosystem_secret(store, APP_API_URL).await? {
            Some(url) => check_url(&url)?,
            None => DEFAULT_API.into(),
        };
        Ok(Self {
            id: id.trim().to_owned(),
            key,
            api,
        })
    }

    /// The App's JWT: RS256, issued a minute back (clock skew), 9 minutes valid.
    fn jwt(&self) -> Result<String> {
        sign_jwt(&self.id, &self.key, chrono::Utc::now().timestamp())
    }

    /// An installation token and when it expires (about an hour).
    pub(crate) async fn installation_token(
        &self,
        installation: i64,
    ) -> Result<(String, Timestamp)> {
        let url = format!(
            "{}/app/installations/{installation}/access_tokens",
            self.api
        );
        let reply = send("POST", url, Some(self.jwt()?), Some(json!({}).into())).await?;
        let body = reply.check("GitHub", "POST /app/installations/{id}/access_tokens")?;
        let (Some(token), Some(expires)) = (body["token"].as_str(), body["expires_at"].as_str())
        else {
            return Err(Error::Protocol("GitHub sent no installation token".into()));
        };
        let expires = chrono::DateTime::parse_from_rfc3339(expires)
            .map_err(|_| Error::Protocol("GitHub sent a bad token expiry".into()))?;
        // Redacted from everything leaving the kernel as text from now on.
        sun_core::redact::remember_secret(token);
        Ok((token.to_owned(), expires.to_utc()))
    }

    /// The org or user the installation is on; `InvalidInput` for an unknown one.
    pub(crate) async fn installation_account(&self, installation: i64) -> Result<String> {
        let url = format!("{}/app/installations/{installation}", self.api);
        let body = send("GET", url, Some(self.jwt()?), None)
            .await?
            .check("GitHub", "GET /app/installations/{id}")?;
        body["account"]["login"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| Error::Protocol("GitHub sent no installation account".into()))
    }
}

fn not_set_up() -> Error {
    Error::Unavailable(format!(
        "The GitHub App isn't set up: an admin sets {APP_ID}, {APP_SLUG}, {APP_PRIVATE_KEY} \
         and {APP_WEBHOOK_SECRET} in Ecosystem settings."
    ))
}

/// The install page for the App, carrying `state` back to the setup URL.
pub(crate) async fn install_url(store: &Store, state: &str) -> Result<String> {
    let slug = ecosystem_secret(store, APP_SLUG)
        .await?
        .ok_or_else(not_set_up)?;
    let slug = slug.trim();
    if slug.is_empty() || !slug.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(Error::InvalidInput(format!(
            "{APP_SLUG} is not an App slug"
        )));
    }
    Ok(format!(
        "{WEB}/apps/{slug}/installations/new?state={}",
        encode(state)
    ))
}

/// An RS256 JWT for App `iss` signed with `pem`, issued at `now - 60`.
pub(crate) fn sign_jwt(iss: &str, pem: &str, now: i64) -> Result<String> {
    let bad = || Error::InvalidInput(format!("{APP_PRIVATE_KEY} is not an RSA private key"));
    let der: String = pem
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let der = STANDARD.decode(der).map_err(|_| bad())?;
    let key = RsaKeyPair::from_pkcs8(&der)
        .or_else(|_| RsaKeyPair::from_der(&der))
        .map_err(|_| bad())?;
    // A numeric App id goes as a number, a Client ID as a string.
    let iss: Value = iss
        .parse::<u64>()
        .map_or_else(|_| json!(iss), |id| json!(id));
    let claims = json!({ "iat": now - 60, "exp": now + 9 * 60, "iss": iss });
    let message = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        message.as_bytes(),
        &mut signature,
    )
    .map_err(|_| Error::Unavailable("signing the GitHub App JWT failed".into()))?;
    Ok(format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature)))
}

fn remote(repo: &Value) -> Option<RemoteRepo> {
    Some(RemoteRepo {
        name: repo["full_name"].as_str()?.to_owned(),
        url: repo["clone_url"].as_str()?.to_owned(),
        web_url: repo["html_url"].as_str().unwrap_or_default().to_owned(),
        default_branch: repo["default_branch"].as_str().unwrap_or("main").to_owned(),
        description: repo["description"].as_str().unwrap_or_default().to_owned(),
        private: repo["private"].as_bool().unwrap_or(false),
    })
}

/// One page (from 1) of the repos the installation can reach, filtered by
/// `query` (case-insensitive, on `owner/name`).
pub(crate) async fn repos(
    api: &str,
    token: &str,
    query: &str,
    page: u32,
) -> Result<Vec<RemoteRepo>> {
    let fetch = |page: u32, per_page: u32| {
        let url = format!("{api}/installation/repositories?per_page={per_page}&page={page}");
        async move {
            let body = send("GET", url, Some(token.to_owned()), None)
                .await?
                .check("GitHub", "GET /installation/repositories")?;
            let repos = body["repositories"].as_array().cloned().unwrap_or_default();
            Ok::<_, Error>(repos.iter().filter_map(remote).collect::<Vec<_>>())
        }
    };
    if query.is_empty() {
        return fetch(page, PER_PAGE).await;
    }
    // ponytail: the endpoint can't search, so a search reads up to
    // SEARCH_PAGES × 100 repos and pages the matches itself.
    let query = query.to_lowercase();
    let mut hits = Vec::new();
    for at in 1..=SEARCH_PAGES {
        let batch = fetch(at, 100).await?;
        let last = batch.len() < 100;
        hits.extend(
            batch
                .into_iter()
                .filter(|r| r.name.to_lowercase().contains(&query)),
        );
        if last {
            break;
        }
    }
    let skip = (page.saturating_sub(1) * PER_PAGE) as usize;
    Ok(hits
        .into_iter()
        .skip(skip)
        .take(PER_PAGE as usize)
        .collect())
}

/// `owner/name` as the installation sees it; `InvalidInput` when it can't.
pub(crate) async fn repo(api: &str, token: &str, slug: &str) -> Result<RemoteRepo> {
    let (owner, name) = slug
        .split_once('/')
        .ok_or_else(|| Error::InvalidInput(format!("`{slug}` is not owner/name")))?;
    let url = format!("{api}/repos/{}/{}", encode(owner), encode(name));
    let reply = send("GET", url, Some(token.to_owned()), None).await?;
    if reply.status == 404 {
        return Err(Error::InvalidInput(format!(
            "The GitHub App can't see {slug}: add it to the installation on GitHub \
             (Settings → GitHub Apps → Configure)."
        )));
    }
    let body = reply.check("GitHub", "GET /repos/{owner}/{name}")?;
    remote(&body).ok_or_else(|| Error::Protocol("GitHub sent an incomplete repo".into()))
}

/// The GraphQL query behind [`pr_snapshot`]: mergeability and the head's
/// check rollup, as the task page's PR row reads it.
// ponytail: the first 100 contexts only; page `contexts` with `after:`
// when a repo runs more checks than that.
const SNAPSHOT_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){\
repository(owner:$owner,name:$name){pullRequest(number:$number){\
headRefOid mergeable mergeStateStatus \
commits(last:1){nodes{commit{statusCheckRollup{state \
contexts(first:100){nodes{__typename \
... on CheckRun{status conclusion} \
... on StatusContext{state}}}}}}}}}}";

/// The PR's mergeability and checks at its current head, from one GraphQL
/// call with an installation token.
pub(crate) async fn pr_snapshot(
    api: &str,
    token: &str,
    owner: &str,
    name: &str,
    number: i64,
) -> Result<PrSnapshot> {
    let body = json!({
        "query": SNAPSHOT_QUERY,
        "variables": { "owner": owner, "name": name, "number": number },
    });
    let reply = send(
        "POST",
        format!("{api}/graphql"),
        Some(token.to_owned()),
        Some(body.into()),
    )
    .await?
    .check("GitHub", "POST /graphql")?;
    snapshot_from(&reply["data"]["repository"]["pullRequest"])
        .ok_or_else(|| Error::Protocol("GitHub sent no pull request snapshot".into()))
}

/// A [`PrSnapshot`] from the query's `pullRequest` object. A `null`
/// `statusCheckRollup` is "no checks yet": no rollup state, every count 0.
/// A check counts as passed on `SUCCESS`, `NEUTRAL` or `SKIPPED`, as
/// pending while not `COMPLETED` (or a status context `PENDING` /
/// `EXPECTED`), and as failed on any other conclusion.
pub(crate) fn snapshot_from(pr: &Value) -> Option<PrSnapshot> {
    let head_sha = pr["headRefOid"].as_str()?.to_owned();
    let text = |v: &Value| v.as_str().map(str::to_owned);
    let rollup = &pr["commits"]["nodes"][0]["commit"]["statusCheckRollup"];
    let (mut passed, mut failed, mut pending) = (0, 0, 0);
    for node in rollup["contexts"]["nodes"].as_array().into_iter().flatten() {
        let verdict = match node["__typename"].as_str() {
            Some("CheckRun") if node["status"].as_str() != Some("COMPLETED") => None,
            Some("CheckRun") => node["conclusion"].as_str(),
            Some("StatusContext") => match node["state"].as_str() {
                Some("SUCCESS" | "FAILURE" | "ERROR") => node["state"].as_str(),
                _ => None,
            },
            _ => continue,
        };
        match verdict {
            None => pending += 1,
            Some("SUCCESS" | "NEUTRAL" | "SKIPPED") => passed += 1,
            Some(_) => failed += 1,
        }
    }
    Some(PrSnapshot {
        head_sha,
        mergeable: text(&pr["mergeable"]),
        merge_state_status: text(&pr["mergeStateStatus"]),
        checks_rollup_state: text(&rollup["state"]),
        checks_total: passed + failed + pending,
        checks_passed: passed,
        checks_failed: failed,
        checks_pending: pending,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used)]
    use ring::signature::{KeyPair, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    use super::*;

    #[test]
    fn snapshot_counts_checks_and_reads_no_rollup_as_no_checks() {
        let pr = json!({
            "headRefOid": "abc", "mergeable": "MERGEABLE", "mergeStateStatus": "BLOCKED",
            "commits": { "nodes": [{ "commit": { "statusCheckRollup": {
                "state": "FAILURE",
                "contexts": { "nodes": [
                    { "__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS" },
                    { "__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SKIPPED" },
                    { "__typename": "CheckRun", "status": "COMPLETED", "conclusion": "CANCELLED" },
                    { "__typename": "CheckRun", "status": "IN_PROGRESS", "conclusion": null },
                    { "__typename": "StatusContext", "state": "ERROR" },
                    { "__typename": "StatusContext", "state": "EXPECTED" },
                ] }
            } } }] }
        });
        let snapshot = snapshot_from(&pr).unwrap();
        assert_eq!(
            snapshot,
            PrSnapshot {
                head_sha: "abc".into(),
                mergeable: Some("MERGEABLE".into()),
                merge_state_status: Some("BLOCKED".into()),
                checks_rollup_state: Some("FAILURE".into()),
                checks_total: 6,
                checks_passed: 2,
                checks_failed: 2,
                checks_pending: 2,
            }
        );
        let none = snapshot_from(&json!({
            "headRefOid": "abc", "mergeable": "UNKNOWN", "mergeStateStatus": "UNKNOWN",
            "commits": { "nodes": [{ "commit": { "statusCheckRollup": null } }] }
        }))
        .unwrap();
        assert_eq!(none.checks_rollup_state, None);
        assert_eq!(none.checks_total, 0);
        assert_eq!(snapshot_from(&Value::Null), None);
    }

    /// A throwaway 2048-bit key made for these tests only (PKCS#1, as GitHub
    /// hands them out).
    pub(crate) const TEST_KEY: &str = include_str!("test_app_key.pem");

    #[test]
    fn github_app_jwt_is_rs256_and_verifies() {
        let jwt = sign_jwt("123456", TEST_KEY, 1_000_000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(
            claims,
            json!({ "iat": 999_940, "exp": 1_000_540, "iss": 123_456 })
        );
        let header: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "RS256");
        // Verifies with the key's public half.
        let der: String = TEST_KEY
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect();
        let key = RsaKeyPair::from_der(&STANDARD.decode(der).unwrap()).unwrap();
        let public = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public_key().as_ref());
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        let message = format!("{}.{}", parts[0], parts[1]);
        public.verify(message.as_bytes(), &signature).unwrap();
        // A key that isn't one is refused without echoing it.
        let err = sign_jwt("1", "-----BEGIN RSA PRIVATE KEY-----\nnope\n", 0).unwrap_err();
        assert!(!err.to_string().contains("nope"), "{err}");
    }
}
