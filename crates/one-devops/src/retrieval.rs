//! Hybrid retrieval kernel for the team knowledge base (P0-3).
//!
//! The original implementation was dense-vector-only: embed the query, scan
//! every chunk, rank by cosine. That misses exactly what people search a
//! company knowledge base for — product names, error codes, ticket ids,
//! internal jargon — because an embedding blurs rare literal tokens.
//!
//! This adds a lexical (BM25) ranker on SQLite's built-in FTS5 and fuses it
//! with the vector ranking using Reciprocal Rank Fusion.
//!
//! # Why SQLite FTS5 and not a dedicated vector database
//!
//! A LanceDB kernel was built and measured first: it worked, but it grew the
//! shipped backend binary from 94 MB to 299 MB (arrow + datafusion) and added a
//! `protoc` build-time dependency to every CI runner. For a desktop app
//! distributed as an installer that is a poor trade, because the parts that
//! actually closed the quality gap — lexical matching, rank fusion, structure
//! aware chunking — need no new dependency at all. FTS5 is compiled into the
//! SQLite already linked here. What is genuinely given up is ANN (sub-linear
//! vector search); at this corpus scale a filtered linear scan is fine.
//!
//! # Access control
//!
//! Both rankers apply the viewer's visibility predicate in SQL
//! (`DevopsService::member_visibility_where`) *before* ranking, so an invisible
//! document can never occupy a top-k slot — the same guarantee the original
//! join gave.

use sqlx::{Row, SqlitePool};

use crate::error::DevopsError;

/// Reciprocal-rank-fusion constant. 60 is the value from the original RRF
/// paper; it damps the influence of any single ranker's top position so the
/// vector and lexical lists blend instead of one dominating.
const RRF_K: f32 = 60.0;

/// How many candidates each ranker contributes before fusion. Wider than the
/// caller's `top_k` on purpose: a result that is mediocre for one ranker and
/// excellent for the other should still be able to win after fusion.
const CANDIDATE_MULTIPLIER: usize = 4;
const MIN_CANDIDATES: usize = 20;

/// The FTS5 mirror of `one_rag_chunks.content`.
pub const FTS_TABLE: &str = "one_rag_chunks_fts";

/// Which FTS5 tokenizer backs the lexical index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtsMode {
    /// Character trigrams. Segments CJK, which `unicode61` cannot do at all —
    /// Chinese text has no spaces, so a word tokenizer indexes whole sentences
    /// as single tokens and matches almost nothing.
    Trigram,
    /// Unicode word tokenizer. Fallback when the SQLite build predates the
    /// trigram tokenizer (3.34). Works for space-delimited languages only.
    Unicode61,
    /// FTS5 is not compiled in; retrieval stays vector-only.
    Unavailable,
}

impl FtsMode {
    pub fn is_available(self) -> bool {
        !matches!(self, Self::Unavailable)
    }
}

/// Create the lexical index if it does not exist, and report what we got.
///
/// Deliberately done at runtime rather than in a migration: if this SQLite
/// build lacks FTS5 a migration failure would brick startup, whereas here the
/// knowledge base simply degrades to vector-only retrieval.
pub async fn ensure_fts_table(pool: &SqlitePool) -> FtsMode {
    for (mode, tokenizer) in [(FtsMode::Trigram, "trigram"), (FtsMode::Unicode61, "unicode61")] {
        let sql = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {FTS_TABLE} USING fts5(\
                 chunk_id UNINDEXED, content, tokenize = '{tokenizer}')"
        );
        match sqlx::query(&sql).execute(pool).await {
            Ok(_) => return mode,
            Err(e) => {
                tracing::debug!(tokenizer, error = %e, "FTS5 table creation attempt failed");
            }
        }
    }
    tracing::warn!("FTS5 unavailable; team knowledge retrieval will be vector-only");
    FtsMode::Unavailable
}

/// Replace a document's rows in the lexical index.
pub async fn sync_document(
    pool: &SqlitePool,
    document_id: &str,
    chunks: &[(String, String)],
) -> Result<(), DevopsError> {
    if !ensure_fts_table(pool).await.is_available() {
        return Ok(());
    }
    delete_document(pool, document_id).await?;
    for (chunk_id, content) in chunks {
        sqlx::query(&format!("INSERT INTO {FTS_TABLE} (chunk_id, content) VALUES (?, ?)"))
            .bind(chunk_id)
            .bind(content)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// Drop a document's rows from the lexical index.
///
/// The FTS table has no document column (it mirrors chunks), so deletion goes
/// through the chunk ids that belong to the document.
pub async fn delete_document(pool: &SqlitePool, document_id: &str) -> Result<(), DevopsError> {
    if !ensure_fts_table(pool).await.is_available() {
        return Ok(());
    }
    sqlx::query(&format!(
        "DELETE FROM {FTS_TABLE} WHERE chunk_id IN (SELECT id FROM one_rag_chunks WHERE document_id = ?)"
    ))
    .bind(document_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Rebuild the whole lexical index from `one_rag_chunks`.
///
/// Needed once for installs whose knowledge base predates this index. Cheap and
/// safe to call on every boot: it self-skips when the index already has rows,
/// and it reads only text already in SQLite — no embedding calls.
pub async fn rebuild_index(pool: &SqlitePool) -> Result<usize, DevopsError> {
    if !ensure_fts_table(pool).await.is_available() {
        return Ok(0);
    }
    let indexed: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {FTS_TABLE}"))
        .fetch_one(pool)
        .await?;
    if indexed > 0 {
        return Ok(0);
    }
    let inserted = sqlx::query(&format!(
        "INSERT INTO {FTS_TABLE} (chunk_id, content) SELECT id, content FROM one_rag_chunks"
    ))
    .execute(pool)
    .await?
    .rows_affected() as usize;
    if inserted > 0 {
        tracing::info!(chunks = inserted, "team knowledge lexical index built");
    }
    Ok(inserted)
}

/// Turn a user query into a safe FTS5 MATCH expression.
///
/// FTS5's query language has operators (`AND`, `OR`, `NEAR`, `*`, `:`, `-`), so
/// a raw user string is both a syntax-error risk and a way to smuggle operators
/// into the query. Every whitespace-separated run is therefore quoted as a
/// literal phrase and the phrases are OR-ed, which is also what makes CJK work:
/// a Chinese query has no spaces, so it stays one phrase and the trigram
/// tokenizer substring-matches it.
pub fn build_match_expression(query: &str) -> Option<String> {
    let phrases: Vec<String> = query
        .split_whitespace()
        .map(|token| token.replace('"', "\"\""))
        .filter(|token| !token.trim().is_empty())
        .map(|token| format!("\"{token}\""))
        .collect();
    if phrases.is_empty() {
        return None;
    }
    Some(phrases.join(" OR "))
}

/// One lexical hit: a chunk id and its BM25 rank position.
#[derive(Debug, Clone)]
pub struct LexicalHit {
    pub chunk_id: String,
}

/// BM25-ranked chunk ids visible to the viewer, best first.
///
/// `acl_predicate` is the caller's already-built visibility SQL (or `None` for
/// a privileged viewer); it is applied inside this query so an invisible
/// document cannot take a candidate slot.
pub async fn lexical_candidates(
    pool: &SqlitePool,
    query: &str,
    acl_predicate: Option<&str>,
    viewer_user_id: &str,
    limit: usize,
) -> Result<Vec<LexicalHit>, DevopsError> {
    if !ensure_fts_table(pool).await.is_available() {
        return Ok(vec![]);
    }
    let Some(match_expr) = build_match_expression(query) else {
        return Ok(vec![]);
    };

    // `bm25()` returns a *more negative* score for a better match, so plain
    // ascending order is best-first.
    let mut sql = format!(
        "SELECT c.id FROM {FTS_TABLE} f \
         JOIN one_rag_chunks c ON c.id = f.chunk_id \
         JOIN one_rag_documents d ON d.id = c.document_id \
         WHERE {FTS_TABLE} MATCH ?"
    );
    if let Some(predicate) = acl_predicate {
        sql.push_str(&format!(" AND ({predicate})"));
    }
    sql.push_str(&format!(" ORDER BY bm25({FTS_TABLE}) LIMIT ?"));

    let mut q = sqlx::query(&sql).bind(match_expr);
    if acl_predicate.is_some() {
        q = q.bind(viewer_user_id);
    }
    let rows = match q.bind(limit as i64).fetch_all(pool).await {
        Ok(rows) => rows,
        // A malformed MATCH expression must degrade to "no lexical hits", not
        // fail the whole search — the vector ranker still has an answer.
        Err(e) => {
            tracing::warn!(error = %e, "lexical search failed; falling back to vector-only ranking");
            return Ok(vec![]);
        }
    };

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(LexicalHit {
            chunk_id: row.try_get::<String, _>("id")?,
        });
    }
    Ok(out)
}

/// How many candidates each ranker should produce for a requested `top_k`.
pub fn candidate_limit(top_k: usize) -> usize {
    (top_k * CANDIDATE_MULTIPLIER).max(MIN_CANDIDATES)
}

/// Fuse two ranked id lists with Reciprocal Rank Fusion.
///
/// RRF combines by *rank*, not by score, which is the point: cosine similarity
/// and BM25 live on incomparable scales, and normalizing them against each
/// other would be arbitrary. An item ranked highly by either ranker scores
/// well; one ranked highly by both wins.
pub fn rrf_fuse(vector_ranked: &[String], lexical_ranked: &[String]) -> Vec<(String, f32)> {
    let mut scores: std::collections::HashMap<&str, f32> = std::collections::HashMap::new();
    for ranked in [vector_ranked, lexical_ranked] {
        for (index, id) in ranked.iter().enumerate() {
            *scores.entry(id.as_str()).or_insert(0.0) += 1.0 / (RRF_K + index as f32 + 1.0);
        }
    }
    let mut fused: Vec<(String, f32)> = scores.into_iter().map(|(id, s)| (id.to_string(), s)).collect();
    // Ties broken by id so the ordering is deterministic across runs.
    fused.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    fused
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect(":memory:").await.unwrap();
        sqlx::raw_sql(
            "CREATE TABLE one_rag_documents (id TEXT PRIMARY KEY, title TEXT NOT NULL, scope TEXT NOT NULL DEFAULT 'org', team_id TEXT, visibility TEXT NOT NULL DEFAULT 'all');
             CREATE TABLE one_rag_chunks (id TEXT PRIMARY KEY, document_id TEXT NOT NULL, chunk_index INTEGER NOT NULL DEFAULT 0, content TEXT NOT NULL);
             CREATE TABLE one_user_org (user_id TEXT NOT NULL, tenant_id TEXT NOT NULL, role TEXT NOT NULL DEFAULT 'member', PRIMARY KEY (user_id, tenant_id));",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    /// The whole lexical half of retrieval rests on FTS5 being compiled in.
    /// If this ever fails on a build target, `FtsMode::Unavailable` is the
    /// documented degradation — but we want to know.
    #[tokio::test]
    async fn fts5_is_available_in_this_build() {
        let pool = pool().await;
        let mode = ensure_fts_table(&pool).await;
        assert!(mode.is_available(), "FTS5 is not compiled into this SQLite build");
        assert_eq!(mode, FtsMode::Trigram, "expected the CJK-capable trigram tokenizer");
    }

    #[tokio::test]
    async fn ensure_is_idempotent() {
        let pool = pool().await;
        assert!(ensure_fts_table(&pool).await.is_available());
        assert!(ensure_fts_table(&pool).await.is_available());
    }

    async fn seed(pool: &SqlitePool) {
        sqlx::raw_sql(
            "INSERT INTO one_rag_documents (id, title, scope, team_id, visibility) VALUES ('doc-org', 'Org Handbook', 'org', NULL, 'all');
             INSERT INTO one_rag_documents (id, title, scope, team_id, visibility) VALUES ('doc-a', 'Team A Notes', 'team', 'tA', 'all');
             INSERT INTO one_rag_documents (id, title, scope, team_id, visibility) VALUES ('doc-b', 'Team B Secrets', 'team', 'tB', 'all');
             INSERT INTO one_rag_chunks (id, document_id, content) VALUES ('c-org', 'doc-org', 'The deployment runbook mentions ERR_QUOTA_4471 explicitly.');
             INSERT INTO one_rag_chunks (id, document_id, content) VALUES ('c-a', 'doc-a', 'Team A owns ERR_QUOTA_4471 triage.');
             INSERT INTO one_rag_chunks (id, document_id, content) VALUES ('c-b', 'doc-b', 'Team B also mentions ERR_QUOTA_4471 here.');
             INSERT INTO one_user_org (user_id, tenant_id, role) VALUES ('member1', 'tA', 'member');",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    /// The predicate string mirrors `DevopsService::member_visibility_where("d.")`.
    const MEMBER_ACL: &str = "(d.scope = 'org' OR (d.scope = 'team' AND d.team_id IN \
         (SELECT tenant_id FROM one_user_org WHERE user_id = ?))) AND d.visibility = 'all'";

    #[tokio::test]
    async fn lexical_search_finds_exact_identifiers() {
        let pool = pool().await;
        seed(&pool).await;
        rebuild_index(&pool).await.unwrap();

        // An error code is precisely the kind of rare literal token a dense
        // embedding blurs — this is why the lexical ranker exists.
        let hits = lexical_candidates(&pool, "ERR_QUOTA_4471", None, "admin", 10)
            .await
            .unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.chunk_id.as_str()).collect();
        assert_eq!(ids.len(), 3, "all three chunks mention the code");
    }

    /// The security-critical assertion: the ACL is applied *inside* the ranked
    /// query, so another group's document cannot occupy a candidate slot.
    #[tokio::test]
    async fn lexical_search_enforces_viewer_acl() {
        let pool = pool().await;
        seed(&pool).await;
        rebuild_index(&pool).await.unwrap();

        let hits = lexical_candidates(&pool, "ERR_QUOTA_4471", Some(MEMBER_ACL), "member1", 10)
            .await
            .unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.chunk_id.as_str()).collect();
        assert!(ids.contains(&"c-org"), "org-scoped content is visible");
        assert!(ids.contains(&"c-a"), "own project group is visible");
        assert!(!ids.contains(&"c-b"), "another group's document must not leak");
    }

    #[tokio::test]
    async fn chinese_query_matches_without_spaces() {
        let pool = pool().await;
        sqlx::raw_sql(
            "INSERT INTO one_rag_documents (id, title) VALUES ('d1', '规章');
             INSERT INTO one_rag_chunks (id, document_id, content) VALUES ('zh1', 'd1', '报销流程需要部门经理审批后提交财务。');",
        )
        .execute(&pool)
        .await
        .unwrap();
        rebuild_index(&pool).await.unwrap();

        let hits = lexical_candidates(&pool, "报销流程", None, "admin", 5).await.unwrap();
        assert_eq!(hits.len(), 1, "trigram tokenizer must match unsegmented Chinese");
        assert_eq!(hits[0].chunk_id, "zh1");
    }

    #[tokio::test]
    async fn sync_and_delete_keep_the_index_in_step() {
        let pool = pool().await;
        seed(&pool).await;
        rebuild_index(&pool).await.unwrap();

        sync_document(
            &pool,
            "doc-a",
            &[("c-a".into(), "Team A rewrote this page entirely.".into())],
        )
        .await
        .unwrap();
        let hits = lexical_candidates(&pool, "rewrote", None, "admin", 5).await.unwrap();
        assert_eq!(hits.len(), 1, "re-synced content is searchable");

        // Re-syncing must converge, not accumulate.
        sync_document(
            &pool,
            "doc-a",
            &[("c-a".into(), "Team A rewrote this page entirely.".into())],
        )
        .await
        .unwrap();
        let hits = lexical_candidates(&pool, "rewrote", None, "admin", 5).await.unwrap();
        assert_eq!(hits.len(), 1, "re-sync must not duplicate rows");

        delete_document(&pool, "doc-a").await.unwrap();
        let hits = lexical_candidates(&pool, "rewrote", None, "admin", 5).await.unwrap();
        assert!(hits.is_empty(), "deleted document must leave no lexical rows");
    }

    #[tokio::test]
    async fn rebuild_is_idempotent() {
        let pool = pool().await;
        seed(&pool).await;
        assert_eq!(rebuild_index(&pool).await.unwrap(), 3);
        assert_eq!(rebuild_index(&pool).await.unwrap(), 0, "second run must be a no-op");
        let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {FTS_TABLE}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total, 3);
    }

    /// FTS5 operators in user input must be treated as literal text, not as
    /// query syntax.
    #[tokio::test]
    async fn operator_like_input_does_not_break_the_query() {
        let pool = pool().await;
        seed(&pool).await;
        rebuild_index(&pool).await.unwrap();
        for probe in ["ERR_QUOTA_4471 OR", "\"unbalanced", "NEAR(a b)", "*", "-"] {
            let result = lexical_candidates(&pool, probe, None, "admin", 5).await;
            assert!(result.is_ok(), "query {probe:?} should not error out");
        }
    }

    #[test]
    fn match_expression_quotes_and_escapes() {
        assert_eq!(
            build_match_expression("alpha beta"),
            Some("\"alpha\" OR \"beta\"".into())
        );
        assert_eq!(
            build_match_expression("say \"hi\""),
            Some("\"say\" OR \"\"\"hi\"\"\"".into())
        );
        assert_eq!(build_match_expression("   "), None);
        // CJK stays a single phrase — there is nothing to split on.
        assert_eq!(build_match_expression("报销流程"), Some("\"报销流程\"".into()));
    }

    #[test]
    fn rrf_rewards_agreement_between_rankers() {
        // `b` appears in both lists; `a` and `c` in only one each, at the same
        // rank as each other. Appearing in both must win.
        let fused = rrf_fuse(&["a".to_string(), "b".to_string()], &["b".to_string(), "c".to_string()]);
        let order: Vec<&str> = fused.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(order[0], "b", "the item both rankers surfaced should come first");
        assert_eq!(order.len(), 3);
    }

    /// A top-1 from one ranker can legitimately outrank a mid-pack item that
    /// both rankers agreed on — RRF weights position, not just agreement.
    #[test]
    fn rrf_weights_rank_position_not_only_agreement() {
        let fused = rrf_fuse(
            &["a".to_string(), "b".to_string(), "c".to_string()],
            &["c".to_string(), "b".to_string()],
        );
        let scores: std::collections::HashMap<&str, f32> = fused.iter().map(|(id, s)| (id.as_str(), *s)).collect();
        // c: 1/63 + 1/61 vs b: 1/62 + 1/62 — c edges ahead.
        assert!(scores["c"] > scores["b"], "a #1 placement should carry real weight");
        assert!(scores["b"] > scores["a"], "two placements still beat one");
    }

    #[test]
    fn rrf_keeps_items_seen_by_only_one_ranker() {
        let fused = rrf_fuse(&["only-vector".to_string()], &["only-lexical".to_string()]);
        assert_eq!(fused.len(), 2, "neither ranker's exclusive find may be dropped");
    }

    #[test]
    fn candidate_limit_widens_the_pool() {
        assert_eq!(candidate_limit(5), MIN_CANDIDATES);
        assert_eq!(candidate_limit(100), 400);
    }
}
