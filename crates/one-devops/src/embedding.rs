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

/// Sentence terminators for both Latin and CJK punctuation. A knowledge base
/// holds Chinese and English side by side, so splitting on `.` alone would
/// leave Chinese documents with no sentence boundaries at all.
const SENTENCE_ENDINGS: [char; 8] = ['.', '!', '?', '。', '！', '？', '；', ';'];

/// Split text into overlapping chunks, preferring structural boundaries.
///
/// The previous implementation cut purely on character count, which routinely
/// sliced sentences — and CJK words — in half. A chunk that begins mid-sentence
/// embeds poorly (the vector describes a fragment, not an idea) and reads badly
/// when shown as a citation.
///
/// Strategy, cheapest boundary first:
/// 1. split on blank lines (paragraphs), which are the author's own structure;
/// 2. any paragraph still over `chunk_size` is split on sentence endings;
/// 3. anything still oversized — a minified blob, a long CJK run with no
///    punctuation — falls back to the old fixed-width cut so a pathological
///    input can never produce an unbounded chunk.
///
/// `overlap` characters of the previous chunk are prepended to each subsequent
/// chunk so an answer that straddles a boundary is still retrievable.
pub fn chunk_text(text: &str, chunk_size: usize, overlap: usize) -> Vec<String> {
    let size = chunk_size.max(1);
    if text.trim().is_empty() {
        return vec![];
    }

    // 1 + 2: build units that are each within `size` where the text allows it.
    let mut units: Vec<String> = Vec::new();
    for paragraph in text.split("\n\n") {
        let paragraph = paragraph.trim();
        if paragraph.is_empty() {
            continue;
        }
        if char_len(paragraph) <= size {
            units.push(paragraph.to_owned());
            continue;
        }
        for sentence in split_sentences(paragraph) {
            if char_len(&sentence) <= size {
                units.push(sentence);
            } else {
                // 3: no usable boundary inside this run.
                units.extend(split_fixed(&sentence, size));
            }
        }
    }

    // Pack units back up to `size` so we do not emit a chunk per short line.
    let mut packed: Vec<String> = Vec::new();
    for unit in units {
        match packed.last_mut() {
            Some(last) if char_len(last) + 1 + char_len(&unit) <= size => {
                last.push('\n');
                last.push_str(&unit);
            }
            _ => packed.push(unit),
        }
    }

    if overlap == 0 || packed.len() < 2 {
        return packed;
    }

    // Prepend the tail of the previous chunk to each chunk after the first.
    let mut out = Vec::with_capacity(packed.len());
    for (i, chunk) in packed.iter().enumerate() {
        if i == 0 {
            out.push(chunk.clone());
            continue;
        }
        let prev: Vec<char> = packed[i - 1].chars().collect();
        let take = overlap.min(prev.len());
        let tail: String = prev[prev.len() - take..].iter().collect();
        out.push(format!("{tail}{chunk}"));
    }
    out
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Split on sentence terminators, keeping the terminator with its sentence.
fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if SENTENCE_ENDINGS.contains(&ch) {
            let trimmed = current.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_owned());
            }
            current.clear();
        }
    }
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_owned());
    }
    out
}

/// Last-resort fixed-width split for a run with no internal boundary.
fn split_fixed(text: &str, size: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + size).min(chars.len());
        let piece: String = chars[start..end].iter().collect();
        let trimmed = piece.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_owned());
        }
        start = end;
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
    fn chunking_falls_back_to_fixed_width_without_boundaries() {
        // No paragraph or sentence structure to exploit, so the fixed-width
        // path runs and each chunk after the first carries 1 char of overlap.
        assert_eq!(chunk_text("abcdefghij", 4, 1), vec!["abcd", "defgh", "hij"]);
        assert!(chunk_text("   ", 4, 1).is_empty());
        assert!(chunk_text("", 4, 1).is_empty());
    }

    #[test]
    fn chunking_never_exceeds_the_limit_before_overlap() {
        // A pathological run with no punctuation at all must still be bounded.
        let text = "x".repeat(1000);
        for chunk in chunk_text(&text, 100, 0) {
            assert!(chunk.chars().count() <= 100, "chunk overflowed the size limit");
        }
    }

    #[test]
    fn chunking_prefers_paragraph_boundaries() {
        // Two short paragraphs fit in one chunk together; a third pushes over.
        let text = "alpha\n\nbeta\n\ngamma";
        let chunks = chunk_text(text, 12, 0);
        assert_eq!(chunks, vec!["alpha\nbeta", "gamma"]);
    }

    #[test]
    fn chunking_splits_long_paragraphs_on_sentences() {
        let text = "First sentence. Second sentence. Third sentence.";
        let chunks = chunk_text(text, 20, 0);
        // Each chunk ends at a sentence terminator rather than mid-word.
        for chunk in &chunks {
            assert!(
                chunk.trim_end().ends_with('.'),
                "chunk should end on a sentence boundary, got {chunk:?}"
            );
        }
    }

    #[test]
    fn chunking_handles_cjk_sentence_endings() {
        // Splitting on '.' alone would give Chinese text no boundaries at all,
        // which is exactly the corpus this knowledge base is built for.
        let text = "第一句话很长很长。第二句话也不短。第三句话结束。";
        let chunks = chunk_text(text, 12, 0);
        assert!(chunks.len() > 1, "CJK text must split on 。");
        for chunk in &chunks {
            assert!(
                chunk.contains('。'),
                "each chunk should carry its terminator: {chunk:?}"
            );
        }
    }

    #[test]
    fn chunking_overlap_links_adjacent_chunks() {
        let chunks = chunk_text("alpha\n\nbeta\n\ngamma", 12, 3);
        assert_eq!(chunks.len(), 2);
        // The second chunk starts with the tail of the first, so an answer
        // straddling the boundary is still retrievable from either side.
        assert!(
            chunks[1].starts_with("eta"),
            "expected overlap prefix, got {:?}",
            chunks[1]
        );
    }
}
