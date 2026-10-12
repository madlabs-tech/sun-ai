//! Opt-in embeddings for hybrid retrieval. Nothing is embedded or
//! sent anywhere unless the caller passes an [`Embedder`] (C5).

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use sun_core::Error;

/// Turns texts into vectors (**Strategy**: provider API, local Ollama, or a
/// mod-bundled model).
///
/// Contract: one vector per input text, in input order, all of the same
/// length. Callers validate the reply, normalize and int8-quantize it.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Names the vector space, e.g. `ollama:bge-m3`. Stored with every
    /// vector; a different model re-embeds and never mixes spaces.
    fn model(&self) -> &str;
    /// Embeds `texts`. Transport, rate-limit and 5xx failures are
    /// [`Error::Unavailable`]; a refused request is [`Error::InvalidInput`].
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error>;
}

/// The two wire formats.
#[derive(Debug, Clone)]
enum Api {
    /// `POST {base}/embeddings` (OpenAI, Azure-compatible gateways, vLLM, LM Studio, ...).
    OpenAi {
        api_key: Option<String>,
        dimensions: Option<u32>,
    },
    /// `POST {base}/api/embed` (Ollama ≥ 0.3).
    Ollama,
}

/// An [`Embedder`] over HTTP (**Adapter**): OpenAI-compatible or Ollama.
#[derive(Clone)]
pub struct HttpEmbedder {
    api: Api,
    base_url: String,
    /// The provider's model name, sent on the wire.
    name: String,
    /// [`Embedder::model`]: provider, name and dimensions.
    space: String,
}

impl std::fmt::Debug for HttpEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never prints the API key.
        f.debug_struct("HttpEmbedder")
            .field("base_url", &self.base_url)
            .field("model", &self.space)
            .finish_non_exhaustive()
    }
}

impl HttpEmbedder {
    /// An OpenAI-compatible endpoint, e.g. `https://api.openai.com/v1`.
    /// `dimensions` asks Matryoshka models (`text-embedding-3-*`) for shorter
    /// vectors. The key is only ever sent over https or to loopback.
    pub fn openai(
        base_url: &str,
        api_key: Option<String>,
        model: &str,
        dimensions: Option<u32>,
    ) -> Result<Self, Error> {
        let api_key = api_key.filter(|k| !k.is_empty());
        let base_url = check_url(base_url, api_key.is_some())?;
        let space = match dimensions {
            Some(d) => format!("openai:{model}@{d}"),
            None => format!("openai:{model}"),
        };
        Self::new(
            Api::OpenAi {
                api_key,
                dimensions,
            },
            base_url,
            model,
            space,
        )
    }

    /// A local or self-hosted Ollama, e.g. `http://localhost:11434`.
    pub fn ollama(base_url: &str, model: &str) -> Result<Self, Error> {
        let base_url = check_url(base_url, false)?;
        Self::new(Api::Ollama, base_url, model, format!("ollama:{model}"))
    }

    fn new(api: Api, base_url: String, model: &str, space: String) -> Result<Self, Error> {
        let model = model.trim();
        if model.is_empty() || model.len() > 256 {
            return Err(Error::InvalidInput(
                "embedding model must be 1..=256 bytes".into(),
            ));
        }
        Ok(Self {
            api,
            base_url,
            name: model.to_owned(),
            space,
        })
    }

    /// Blocking request; run on the blocking pool.
    fn post(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
        let (path, body) = match &self.api {
            Api::OpenAi { dimensions, .. } => {
                let mut body = json!({ "model": self.name, "input": texts });
                if let Some(d) = dimensions {
                    body["dimensions"] = json!(d);
                }
                ("/embeddings", body)
            }
            Api::Ollama => ("/api/embed", json!({ "model": self.name, "input": texts })),
        };
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .build()
            .new_agent();
        let mut request = agent
            .post(format!("{}{path}", self.base_url))
            .content_type("application/json");
        if let Api::OpenAi {
            api_key: Some(key), ..
        } = &self.api
        {
            request = request.header("authorization", format!("Bearer {key}"));
        }
        // Messages carry the status only: no key, no echoed document text.
        let what = format!("embedding request to {}{path}", self.base_url);
        let mut response = request
            .send(body.to_string())
            .map_err(|e| Error::Unavailable(format!("{what}: {e}")))?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| Error::Unavailable(format!("{what}: {e}")))?;
        match status {
            200..=299 => {}
            429 | 500.. => return Err(Error::Unavailable(format!("{what}: HTTP {status}"))),
            _ => return Err(Error::InvalidInput(format!("{what}: HTTP {status}"))),
        }
        let reply: Value = serde_json::from_str(&text)
            .map_err(|e| Error::Protocol(format!("{what}: not JSON: {e}")))?;
        let vectors = match &self.api {
            Api::OpenAi { .. } => {
                let mut data: Vec<(u64, Value)> = reply["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|d| {
                        (
                            d["index"].as_u64().unwrap_or(u64::MAX),
                            d["embedding"].clone(),
                        )
                    })
                    .collect();
                data.sort_by_key(|(index, _)| *index);
                data.into_iter().map(|(_, v)| v).collect()
            }
            Api::Ollama => reply["embeddings"].as_array().cloned().unwrap_or_default(),
        };
        vectors
            .iter()
            .map(|v| {
                v.as_array()
                    .ok_or_else(|| Error::Protocol(format!("{what}: no embeddings in reply")))?
                    .iter()
                    .map(|x| {
                        x.as_f64()
                            .map(|x| x as f32)
                            .ok_or_else(|| Error::Protocol(format!("{what}: non-number")))
                    })
                    .collect()
            })
            .collect()
    }
}

#[async_trait]
impl Embedder for HttpEmbedder {
    fn model(&self) -> &str {
        &self.space
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
        let this = self.clone();
        let texts = texts.to_vec();
        tokio::task::spawn_blocking(move || this.post(&texts))
            .await
            .map_err(|e| Error::Unavailable(format!("embedding task: {e}")))?
    }
}

/// http(s) only; with a key, https or loopback only. Strips a trailing `/`.
fn check_url(url: &str, has_key: bool) -> Result<String, Error> {
    let url = url.trim().trim_end_matches('/');
    let loopback = url.strip_prefix("http://").is_some_and(|rest| {
        let host = rest.split(['/', ':']).next().unwrap_or_default();
        matches!(host, "localhost" | "127.0.0.1")
    });
    let ok = url.starts_with("https://") || loopback || (!has_key && url.starts_with("http://"));
    match ok && url.len() <= 2048 {
        true => Ok(url.to_owned()),
        false => Err(Error::InvalidInput(format!(
            "embedding url must be https (or http to localhost), got {url:?}"
        ))),
    }
}

/// Checks a reply against the [`Embedder`] contract: `expected` vectors of one
/// non-zero length with finite values.
pub(crate) fn check_vectors(vectors: &[Vec<f32>], expected: usize) -> Result<(), Error> {
    let dims = vectors.first().map_or(0, Vec::len);
    let ok = vectors.len() == expected
        && (1..=8192).contains(&dims)
        && vectors
            .iter()
            .all(|v| v.len() == dims && v.iter().all(|x| x.is_finite()));
    match ok || expected == 0 {
        true => Ok(()),
        false => Err(Error::Protocol(format!(
            "embedder returned {} vectors of {dims} dims for {expected} texts",
            vectors.len()
        ))),
    }
}

/// Unit-normalizes and quantizes to int8 (`x · 127`, like sqlite-vec's
/// `vec_quantize_int8(.., 'unit')`). Cosine distance ignores the scale.
pub(crate) fn quantize(vector: &[f32]) -> Vec<i8> {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    let scale = if norm > 0.0 { 127.0 / norm } else { 0.0 };
    vector
        .iter()
        .map(|x| (x * scale).round().clamp(-127.0, 127.0) as i8)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retrieval_quantize_and_contract_checks() {
        assert_eq!(quantize(&[3.0, -4.0]), vec![76, -102]);
        assert_eq!(quantize(&[0.0, 0.0]), vec![0, 0]);
        assert!(check_vectors(&[vec![1.0, 2.0], vec![3.0, 4.0]], 2).is_ok());
        assert!(check_vectors(&[vec![1.0, 2.0]], 2).is_err());
        assert!(check_vectors(&[vec![1.0], vec![1.0, 2.0]], 2).is_err());
        assert!(check_vectors(&[vec![f32::NAN]], 1).is_err());
        assert!(check_url("http://example.com", true).is_err());
        assert!(check_url("http://gpu-box:11434/", false).is_ok_and(|u| u.ends_with("11434")));
        assert!(check_url("ftp://x", false).is_err());
    }
}
