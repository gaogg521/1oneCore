//! OpenAI-compatible embeddings client (D1 decision). Talks to any endpoint
//! exposing `POST {base_url}/embeddings` with the OpenAI request/response
//! shape — OpenAI, vLLM, Ollama, Xinference, 智谱, etc.

use serde::{Deserialize, Serialize};

use crate::error::DevopsError;

/// Runtime embedding configuration (one_rag_config row).
#[derive(Debug, Clone)]
pub struct EmbeddingConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl EmbeddingConfig {
    fn validated(&self) -> Result<(), DevopsError> {
        if self.base_url.trim().is_empty() || self.model.trim().is_empty() {
            return Err(DevopsError::BadRequest(
                "RAG embedding endpoint not configured (base_url + model required)".into(),
            ));
        }
        Ok(())
    }

    /// `{base_url}/embeddings`, tolerating a trailing slash.
    fn endpoint(&self) -> String {
        format!("{}/embeddings", self.base_url.trim().trim_end_matches('/'))
    }
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(Deserialize)]
struct EmbeddingDatum {
    embedding: Vec<f32>,
}

/// Embed a batch of texts. Returns one vector per input, in order.
pub async fn embed(config: &EmbeddingConfig, inputs: &[String]) -> Result<Vec<Vec<f32>>, DevopsError> {
    config.validated()?;
    if inputs.is_empty() {
        return Ok(vec![]);
    }

    let client = reqwest::Client::new();
    let mut req = client.post(config.endpoint()).json(&EmbeddingRequest {
        model: config.model.trim(),
        input: inputs,
    });
    let key = config.api_key.trim();
    if !key.is_empty() {
        req = req.bearer_auth(key);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| DevopsError::Internal(format!("embedding request failed: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(DevopsError::Internal(format!("embedding endpoint {status}: {body}")));
    }

    let parsed: EmbeddingResponse = resp
        .json()
        .await
        .map_err(|e| DevopsError::Internal(format!("embedding response parse failed: {e}")))?;
    if parsed.data.len() != inputs.len() {
        return Err(DevopsError::Internal(format!(
            "embedding count mismatch: got {}, expected {}",
            parsed.data.len(),
            inputs.len()
        )));
    }
    Ok(parsed.data.into_iter().map(|d| d.embedding).collect())
}

/// Pack an f32 vector into a little-endian BLOB for SQLite storage.
pub fn pack_embedding(vec: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vec.len() * 4);
    for &v in vec {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Unpack a little-endian BLOB back into an f32 vector.
pub fn unpack_embedding(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

/// Cosine similarity; returns 0.0 for zero-magnitude or length-mismatched vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Split text into overlapping chunks by character count. Simple and
/// language-agnostic; good enough for the metadata-scale corpus here.
pub fn chunk_text(text: &str, chunk_size: usize, overlap: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![];
    }
    let size = chunk_size.max(1);
    let step = size.saturating_sub(overlap).max(1);
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + size).min(chars.len());
        let piece: String = chars[start..end].iter().collect();
        let trimmed = piece.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_owned());
        }
        if end == chars.len() {
            break;
        }
        start += step;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_roundtrip() {
        let v = vec![0.5f32, -1.25, 3.0, 0.0];
        assert_eq!(unpack_embedding(&pack_embedding(&v)), v);
    }

    #[test]
    fn cosine_basics() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn chunking_overlaps_and_trims() {
        let chunks = chunk_text("abcdefghij", 4, 1);
        // step = 3: [0..4) abcd, [3..7) defg, [6..10) ghij, then end.
        assert_eq!(chunks, vec!["abcd", "defg", "ghij"]);
        assert!(chunk_text("   ", 4, 1).is_empty());
    }
}
