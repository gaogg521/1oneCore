-- RAG vector pipeline: embedding config, document content, and chunk vectors.
-- Embedding provider decision (D1): OpenAI-compatible endpoint configured at
-- runtime; the model dimension is discovered from the first embedding call.

-- Singleton config row (id = 'default'). api_key stored as-is (server-side,
-- same trust boundary as SSO secrets in one_sso_providers).
CREATE TABLE IF NOT EXISTS one_rag_config (
    id         TEXT    PRIMARY KEY NOT NULL DEFAULT 'default',
    base_url   TEXT    NOT NULL DEFAULT '',
    api_key    TEXT    NOT NULL DEFAULT '',
    model      TEXT    NOT NULL DEFAULT '',
    dimensions INTEGER,
    updated_at INTEGER NOT NULL DEFAULT 0
);

-- Inline document text to embed. register_rag_document keeps storing metadata;
-- content is set when the caller supplies text (or a file is read server-side).
ALTER TABLE one_rag_documents ADD COLUMN content TEXT;

-- One row per chunk. embedding is a little-endian f32 array packed into a BLOB;
-- retrieval brute-forces cosine similarity (document volume is small).
CREATE TABLE IF NOT EXISTS one_rag_chunks (
    id          TEXT    PRIMARY KEY NOT NULL,
    document_id TEXT    NOT NULL,
    chunk_index INTEGER NOT NULL,
    content     TEXT    NOT NULL,
    embedding   BLOB    NOT NULL,
    created_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_one_rag_chunks_doc ON one_rag_chunks(document_id);
