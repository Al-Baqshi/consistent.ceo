//! Providers that turn text into embedding vectors for semantic memory
//! recall (OpenAI-compatible API, Gemini API, a local Ollama model, and an
//! unimplemented in-process stub), plus a caching wrapper to avoid
//! re-embedding repeated strings.
//!
//! Every HNSW index in the product is 1536 wide (the OpenAI convention). A
//! local model with fewer dimensions is fitted to that width by
//! [`fit_to_index_width`]: L2-normalise, then zero-pad. Zero-padding leaves
//! every dot product and every norm unchanged, so cosine similarity between
//! two padded vectors is exactly the similarity of the originals. What it
//! cannot do is make two *different models'* vectors comparable; that is what
//! [`EmbeddingProvider::identity`] is for (the gateway clears stored vectors
//! when the identity changes).

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

const OPENAI_EMBEDDING_DIM: usize = 1536;
const LOCAL_EMBEDDING_DIM: usize = 384;

/// Width of every HNSW index the stores define.
pub const INDEX_EMBEDDING_DIM: usize = OPENAI_EMBEDDING_DIM;

/// Turns text into a fixed-length embedding vector for semantic search.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
    /// Length of the vectors this provider returns.
    fn dimensions(&self) -> usize;
    /// Which vector space this provider writes, e.g. `ollama:nomic-embed-text`.
    /// Vectors from two identities must never be compared; the default
    /// (`unknown`) is only for test doubles.
    fn identity(&self) -> String {
        "unknown".to_string()
    }
}

/// Fit a vector from a narrower model into an index of width `width`:
/// L2-normalise, then zero-pad. Cosine similarity between two fitted vectors
/// equals the similarity of the originals. Fails if the vector is wider than
/// the index or empty.
pub fn fit_to_index_width(mut v: Vec<f32>, width: usize) -> Result<Vec<f32>> {
    if v.is_empty() {
        anyhow::bail!("empty embedding");
    }
    if v.len() > width {
        anyhow::bail!(
            "embedding has {} dimensions; the index holds at most {width}. Pick a smaller embedding model",
            v.len()
        );
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in &mut v {
            *x /= norm;
        }
    }
    v.resize(width, 0.0);
    Ok(v)
}

/// Local embedding (e.g. candle/ONNX). Not implemented; use ApiEmbedding or mock in tests.
pub struct LocalEmbedding;

#[async_trait]
impl EmbeddingProvider for LocalEmbedding {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        anyhow::bail!("Local embedding not implemented; use ApiEmbedding or enable candle feature")
    }

    fn dimensions(&self) -> usize {
        LOCAL_EMBEDDING_DIM
    }
}

#[derive(Debug, Serialize)]
struct OpenAiEmbedRequest<'a> {
    model: &'a str,
    input: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    encoding_format: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbedResponse {
    data: Vec<OpenAiEmbedData>,
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbedData {
    embedding: Vec<f32>,
}

/// OpenAI-compatible embeddings API (e.g. text-embedding-3-small).
pub struct ApiEmbedding {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    client: reqwest::Client,
}

impl ApiEmbedding {
    /// Defaults to OpenAI's public API and the `text-embedding-3-small` model;
    /// pass `base_url` to point at a compatible endpoint instead.
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        Self {
            api_key,
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            model: "text-embedding-3-small".to_string(),
            client,
        }
    }

    /// Override the default embedding model.
    pub fn with_model(mut self, model: String) -> Self {
        self.model = model;
        self
    }
}

#[async_trait]
impl EmbeddingProvider for ApiEmbedding {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));
        let body = OpenAiEmbedRequest {
            model: &self.model,
            input: text,
            encoding_format: Some("float"),
        };
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("embeddings request to {url}"))?;

        let status = resp.status();
        let text_body = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            anyhow::bail!("embeddings API error {}: {}", status, text_body);
        }

        let parsed: OpenAiEmbedResponse =
            serde_json::from_str(&text_body).with_context(|| "parse embeddings response")?;

        let embedding = parsed
            .data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or_else(|| anyhow::anyhow!("embeddings response had no data"))?;

        if embedding.len() != OPENAI_EMBEDDING_DIM {
            anyhow::bail!(
                "unexpected embedding dimension {} (expected {})",
                embedding.len(),
                OPENAI_EMBEDDING_DIM
            );
        }
        Ok(embedding)
    }

    fn dimensions(&self) -> usize {
        OPENAI_EMBEDDING_DIM
    }

    fn identity(&self) -> String {
        format!(
            "openai-compatible:{}@{}",
            self.model,
            self.base_url.trim_end_matches('/')
        )
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiEmbedRequest<'a> {
    model: &'a str,
    content: GeminiEmbedContent<'a>,
    output_dimensionality: usize,
}

#[derive(Debug, Serialize)]
struct GeminiEmbedContent<'a> {
    parts: [GeminiEmbedPart<'a>; 1],
}

#[derive(Debug, Serialize)]
struct GeminiEmbedPart<'a> {
    text: &'a str,
}

#[derive(Debug, Deserialize)]
struct GeminiEmbedResponse {
    embedding: GeminiEmbedValues,
}

#[derive(Debug, Deserialize)]
struct GeminiEmbedValues {
    values: Vec<f32>,
}

/// Google Gemini embeddings (`gemini-embedding-001`), requested at 1536 dims
/// to match the OpenAI convention the HNSW indexes are built on. Gemini only
/// L2-normalizes its native 3072-dim output, so truncated dimensions are
/// re-normalized here — cosine recall assumes unit-ish vectors.
pub struct GeminiEmbedding {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    client: reqwest::Client,
}

impl GeminiEmbedding {
    /// Defaults to Google's public Generative Language API and `gemini-embedding-001`.
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        Self {
            api_key,
            base_url: base_url
                .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".to_string()),
            model: "gemini-embedding-001".to_string(),
            client,
        }
    }
}

#[async_trait]
impl EmbeddingProvider for GeminiEmbedding {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let url = format!(
            "{}/models/{}:embedContent",
            self.base_url.trim_end_matches('/'),
            self.model
        );
        let model_path = format!("models/{}", self.model);
        let body = GeminiEmbedRequest {
            model: &model_path,
            content: GeminiEmbedContent {
                parts: [GeminiEmbedPart { text }],
            },
            output_dimensionality: OPENAI_EMBEDDING_DIM,
        };
        let resp = self
            .client
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("gemini embeddings request to {url}"))?;

        let status = resp.status();
        let text_body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("gemini embeddings API error {}: {}", status, text_body);
        }

        let parsed: GeminiEmbedResponse =
            serde_json::from_str(&text_body).with_context(|| "parse gemini embed response")?;
        let mut embedding = parsed.embedding.values;
        if embedding.len() != OPENAI_EMBEDDING_DIM {
            anyhow::bail!(
                "unexpected gemini embedding dimension {} (expected {})",
                embedding.len(),
                OPENAI_EMBEDDING_DIM
            );
        }
        let norm = embedding.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > f32::EPSILON {
            for v in &mut embedding {
                *v /= norm;
            }
        }
        Ok(embedding)
    }

    fn dimensions(&self) -> usize {
        OPENAI_EMBEDDING_DIM
    }

    fn identity(&self) -> String {
        format!("gemini:{}", self.model)
    }
}

/// Default local embedding model: small (274 MB), 768 dimensions, Apache-2.0,
/// and in the Ollama library under this exact name.
pub const DEFAULT_OLLAMA_EMBED_MODEL: &str = "nomic-embed-text";

#[derive(Debug, Serialize)]
struct OllamaEmbedRequest<'a> {
    model: &'a str,
    input: &'a str,
}

#[derive(Debug, Deserialize)]
struct OllamaEmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

#[derive(Debug, Deserialize)]
struct OllamaTags {
    #[serde(default)]
    models: Vec<OllamaTag>,
}

#[derive(Debug, Deserialize)]
struct OllamaTag {
    name: String,
}

/// A local Ollama embedding model (`POST /api/embed`). No key, no network
/// beyond this machine. Vectors are fitted to [`INDEX_EMBEDDING_DIM`] with
/// [`fit_to_index_width`], so it shares the stores' existing indexes.
pub struct OllamaEmbedding {
    /// Ollama's native root, e.g. `http://127.0.0.1:11434` (no `/v1`).
    pub base_url: String,
    pub model: String,
    client: reqwest::Client,
}

impl OllamaEmbedding {
    /// `base_url` may carry the OpenAI-compatible `/v1` suffix the chat
    /// config uses; it is stripped, since embeddings use the native API.
    pub fn new(base_url: &str, model: &str) -> Self {
        let client = reqwest::Client::builder()
            // The first call after Ollama starts loads the model from disk.
            .timeout(Duration::from_secs(60))
            .build()
            .expect("reqwest client");
        let base = base_url.trim().trim_end_matches('/');
        let base = base.strip_suffix("/v1").unwrap_or(base).to_string();
        Self {
            base_url: base,
            model: model.trim().to_string(),
            client,
        }
    }

    /// Whether Ollama answers and already has this model. `Err` means Ollama
    /// itself is not reachable (not installed, or not running).
    pub async fn has_model(&self) -> Result<bool> {
        let url = format!("{}/api/tags", self.base_url);
        let tags: OllamaTags = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .with_context(|| format!("ollama not reachable at {url}"))?
            .error_for_status()?
            .json()
            .await
            .context("parse ollama tags")?;
        let want = self.model.as_str();
        Ok(tags
            .models
            .iter()
            .any(|m| m.name == want || m.name.strip_suffix(":latest") == Some(want)))
    }

    /// Download the model into Ollama. Blocks until done; run it in the
    /// background. A 274 MB model takes a minute or two on home broadband.
    pub async fn pull(&self) -> Result<()> {
        let url = format!("{}/api/pull", self.base_url);
        let resp = self
            .client
            .post(&url)
            .timeout(Duration::from_secs(60 * 30))
            .json(&serde_json::json!({ "model": self.model, "stream": false }))
            .send()
            .await
            .with_context(|| format!("ollama pull request to {url}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("ollama pull of {} failed ({status}): {body}", self.model);
        }
        Ok(())
    }
}

#[async_trait]
impl EmbeddingProvider for OllamaEmbedding {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let url = format!("{}/api/embed", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&OllamaEmbedRequest {
                model: &self.model,
                input: text,
            })
            .send()
            .await
            .with_context(|| format!("ollama embed request to {url}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("ollama embed error {status}: {body}");
        }
        let parsed: OllamaEmbedResponse =
            serde_json::from_str(&body).context("parse ollama embed response")?;
        let v = parsed
            .embeddings
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("ollama embed response had no vectors"))?;
        fit_to_index_width(v, INDEX_EMBEDDING_DIM)
    }

    fn dimensions(&self) -> usize {
        INDEX_EMBEDDING_DIM
    }

    fn identity(&self) -> String {
        format!(
            "ollama:{}",
            self.model.strip_suffix(":latest").unwrap_or(&self.model)
        )
    }
}

/// In-memory cache for embeddings to avoid re-embedding identical strings.
pub struct CachedEmbedding<E> {
    inner: E,
    cache: RwLock<HashMap<String, Vec<f32>>>,
    max_entries: usize,
}

impl<E: EmbeddingProvider> CachedEmbedding<E> {
    /// Wrap `inner`, capping the cache at `max_entries` distinct strings.
    pub fn new(inner: E, max_entries: usize) -> Self {
        Self {
            inner,
            cache: RwLock::new(HashMap::new()),
            max_entries,
        }
    }

    // Evict an arbitrary entry (HashMap iteration order, not LRU) to make
    // room when the cache is full; simple rather than tracking access order.
    fn evict_one(cache: &mut HashMap<String, Vec<f32>>) {
        if let Some(key) = cache.keys().next().cloned() {
            cache.remove(&key);
        }
    }
}

#[async_trait]
impl<E: EmbeddingProvider + Send + Sync> EmbeddingProvider for CachedEmbedding<E> {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        {
            let guard = self
                .cache
                .read()
                .map_err(|e| anyhow::anyhow!("cache lock: {}", e))?;
            if let Some(cached) = guard.get(text) {
                return Ok(cached.clone());
            }
        }
        let vec = self.inner.embed(text).await?;
        {
            let mut guard = self
                .cache
                .write()
                .map_err(|e| anyhow::anyhow!("cache lock: {}", e))?;
            while guard.len() >= self.max_entries {
                Self::evict_one(&mut guard);
            }
            guard.insert(text.to_string(), vec.clone());
        }
        Ok(vec)
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn identity(&self) -> String {
        self.inner.identity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        dot / (na * nb)
    }

    #[test]
    fn fitting_preserves_cosine_similarity() {
        let a: Vec<f32> = (0..768).map(|i| ((i * 7 % 13) as f32) - 6.0).collect();
        let b: Vec<f32> = (0..768).map(|i| ((i * 5 % 11) as f32) - 4.5).collect();
        let before = cosine(&a, &b);
        let fa = fit_to_index_width(a, INDEX_EMBEDDING_DIM).unwrap();
        let fb = fit_to_index_width(b, INDEX_EMBEDDING_DIM).unwrap();
        assert_eq!(fa.len(), INDEX_EMBEDDING_DIM);
        assert!((cosine(&fa, &fb) - before).abs() < 1e-5);
        // Unit length after fitting.
        let norm = fa.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn fitting_refuses_wider_or_empty_vectors() {
        assert!(
            fit_to_index_width(vec![0.1; INDEX_EMBEDDING_DIM + 1], INDEX_EMBEDDING_DIM).is_err()
        );
        assert!(fit_to_index_width(Vec::new(), INDEX_EMBEDDING_DIM).is_err());
        assert!(fit_to_index_width(vec![0.5; INDEX_EMBEDDING_DIM], INDEX_EMBEDDING_DIM).is_ok());
    }

    #[test]
    fn ollama_uses_the_native_root_and_a_stable_identity() {
        let e = OllamaEmbedding::new("http://127.0.0.1:11434/v1/", "nomic-embed-text:latest");
        assert_eq!(e.base_url, "http://127.0.0.1:11434");
        assert_eq!(e.identity(), "ollama:nomic-embed-text");
        assert_eq!(e.dimensions(), INDEX_EMBEDDING_DIM);
    }

    /// Needs a running Ollama with `nomic-embed-text` pulled:
    /// `cargo test -p consistent-world-memory -- --ignored ollama_live`.
    #[tokio::test]
    #[ignore]
    async fn ollama_live_embeds_and_fits_the_index() {
        let e = OllamaEmbedding::new("http://127.0.0.1:11434", DEFAULT_OLLAMA_EMBED_MODEL);
        assert!(e.has_model().await.expect("ollama reachable"));
        let a = e.embed("the invoice is overdue").await.expect("embed");
        let b = e
            .embed("payment for the bill is late")
            .await
            .expect("embed");
        let c = e.embed("a recipe for banana bread").await.expect("embed");
        assert_eq!(a.len(), INDEX_EMBEDDING_DIM);
        assert!(
            cosine(&a, &b) > cosine(&a, &c),
            "related text should sit closer"
        );
    }

    #[test]
    fn hosted_identities_name_their_vector_space() {
        let o = ApiEmbedding::new("k".into(), None);
        assert_eq!(
            o.identity(),
            "openai-compatible:text-embedding-3-small@https://api.openai.com/v1"
        );
        let g = GeminiEmbedding::new("k".into(), None);
        assert_eq!(g.identity(), "gemini:gemini-embedding-001");
    }
}
