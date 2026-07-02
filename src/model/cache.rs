//! v2 spec §5.2: response cache.
//!
//! Wraps any `Model` and deduplicates calls by `(messages, temperature,
//! tools_schema, max_tokens)` tuple. The cache key is a hash of the
//! canonicalized request; the value is the full `ModelResponse`
//! (content + tool_calls + usage).
//!
//! ## When to use
//!
//! Caching is most useful for *idempotent* calls — repeated
//! `consult_advisor` queries on the same question, repeated
//! verifier self-checks, etc. The graph_loop's main proposer
//! path is intentionally NOT cached (each step changes the
//! graph; the prompt is unique per round).
//!
//! ## Memory cap
//!
//! A simple LRU with `max_entries` (default 256). When the cap is
//! hit, the oldest entry is evicted. Use a larger cap for long
//! sessions where many distinct queries are expected.
//!
//! ## Thread safety
//!
//! The cache state is wrapped in `Mutex`. For high-throughput
//! scenarios, swap to `DashMap` or sharded mutexes.

use super::{Model, ModelRequest, ModelResponse, Role};
use async_trait::async_trait;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct CacheEntry {
    response: ModelResponse,
    cached_at: Instant,
}

pub struct CachingModel {
    inner: Arc<dyn Model>,
    max_entries: usize,
    ttl: Option<Duration>,
    state: Mutex<CacheState>,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<u64, CacheEntry>,
    /// Monotonic counter used as a tie-breaker for LRU eviction.
    counter: u64,
    lru: HashMap<u64, u64>, // cache_key → insertion_counter
    hits: u64,
    misses: u64,
}

impl CachingModel {
    pub fn new(inner: Arc<dyn Model>) -> Self {
        Self::with_capacity(inner, 256, None)
    }

    pub fn with_capacity(
        inner: Arc<dyn Model>,
        max_entries: usize,
        ttl: Option<Duration>,
    ) -> Self {
        Self {
            inner,
            max_entries,
            ttl,
            state: Mutex::new(CacheState::default()),
        }
    }

    pub fn stats(&self) -> (usize, u64, u64) {
        let s = self.state.lock().unwrap();
        (s.entries.len(), s.hits, s.misses)
    }

    pub fn clear(&self) {
        let mut s = self.state.lock().unwrap();
        s.entries.clear();
        s.lru.clear();
        s.hits = 0;
        s.misses = 0;
    }
}

fn hash_request(req: &ModelRequest) -> u64 {
    let mut h = DefaultHasher::new();
    for m in &req.messages {
        let role_byte: u8 = match m.role {
            Role::System => 0,
            Role::User => 1,
            Role::Assistant => 2,
            Role::Tool => 3,
        };
        role_byte.hash(&mut h);
        m.content.hash(&mut h);
    }
    req.temperature.to_bits().hash(&mut h);
    req.max_tokens.unwrap_or(0).hash(&mut h);
    // Tools: hash the JSON serialization so any change in the
    // tool schema busts the cache. The schema is small (one
    // function declaration per tool) so this is cheap.
    for t in &req.tools {
        // Use to_string for stable serialization (Maps aren't
        // Hash natively, but their JSON form is canonical-ish
        // enough for our purposes).
        t.to_string().hash(&mut h);
    }
    h.finish()
}

fn entry_expired(entry: &CacheEntry, ttl: Option<Duration>) -> bool {
    if let Some(ttl) = ttl {
        entry.cached_at.elapsed() > ttl
    } else {
        false
    }
}

#[async_trait]
impl Model for CachingModel {
    fn name(&self) -> &str {
        "CachingModel"
    }

    async fn complete(
        &self,
        request: ModelRequest,
    ) -> Result<ModelResponse, crate::error::HarnessError> {
        let key = hash_request(&request);
        // Fast path: cache hit.
        {
            let mut s = self.state.lock().unwrap();
            if let Some(entry) = s.entries.get(&key).cloned() {
                if !entry_expired(&entry, self.ttl) {
                    s.hits += 1;
                    return Ok(entry.response);
                }
                s.entries.remove(&key);
                s.lru.remove(&key);
            }
        }
        // Miss: call the inner model, then store.
        let resp = self.inner.complete(request).await?;
        let mut s = self.state.lock().unwrap();
        s.misses += 1;
        if s.entries.len() >= self.max_entries {
            if let Some((&oldest_key, _)) = s.lru.iter().min_by_key(|(_, c)| **c) {
                s.entries.remove(&oldest_key);
                s.lru.remove(&oldest_key);
            }
        }
        s.counter += 1;
        let counter = s.counter;
        s.entries.insert(
            key,
            CacheEntry {
                response: resp.clone(),
                cached_at: Instant::now(),
            },
        );
        s.lru.insert(key, counter);
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FinishReason, Message, ModelRequest, ModelResponse, Usage};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Stub model that returns a canned response and counts calls.
    struct StubModel {
        calls: Arc<AtomicUsize>,
        canned: ModelResponse,
    }
    #[async_trait]
    impl Model for StubModel {
        fn name(&self) -> &str {
            "stub"
        }
        async fn complete(
            &self,
            _: ModelRequest,
        ) -> Result<ModelResponse, crate::error::HarnessError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.canned.clone())
        }
    }

    fn mk_response(text: &str) -> ModelResponse {
        ModelResponse {
            content: text.to_string(),
            reasoning_content: None,
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                prompt_cache_hit_tokens: 0,
                prompt_cache_miss_tokens: 0,
            },
        }
    }

    fn mk_request(text: &str) -> ModelRequest {
        ModelRequest {
            messages: vec![Message::user(text)],
            tools: vec![],
            temperature: 0.2,
            max_tokens: Some(100),
            stop: vec![],
        }
    }

    #[tokio::test]
    async fn dedupes_repeated_calls() {
        let stub = Arc::new(StubModel {
            calls: Arc::new(AtomicUsize::new(0)),
            canned: mk_response("hi"),
        });
        let cache = CachingModel::new(stub.clone());
        let r1 = cache.complete(mk_request("hello")).await.unwrap();
        let r2 = cache.complete(mk_request("hello")).await.unwrap();
        assert_eq!(stub.calls.load(Ordering::SeqCst), 1);
        assert_eq!(r1.content, r2.content);
        let (entries, hits, misses) = cache.stats();
        assert_eq!(entries, 1);
        assert_eq!(hits, 1);
        assert_eq!(misses, 1);
    }

    #[tokio::test]
    async fn different_requests_miss() {
        let stub = Arc::new(StubModel {
            calls: Arc::new(AtomicUsize::new(0)),
            canned: mk_response("hi"),
        });
        let cache = CachingModel::new(stub.clone());
        cache.complete(mk_request("hello")).await.unwrap();
        cache.complete(mk_request("world")).await.unwrap();
        assert_eq!(stub.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn ttl_expires_entries() {
        let stub = Arc::new(StubModel {
            calls: Arc::new(AtomicUsize::new(0)),
            canned: mk_response("hi"),
        });
        let cache = CachingModel::with_capacity(stub.clone(), 16, Some(Duration::from_millis(50)));
        cache.complete(mk_request("hello")).await.unwrap();
        cache.complete(mk_request("hello")).await.unwrap();
        assert_eq!(stub.calls.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(100)).await;
        cache.complete(mk_request("hello")).await.unwrap();
        assert_eq!(stub.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn lru_eviction_when_at_capacity() {
        let stub = Arc::new(StubModel {
            calls: Arc::new(AtomicUsize::new(0)),
            canned: mk_response("hi"),
        });
        let cache = CachingModel::with_capacity(stub.clone(), 2, None);
        cache.complete(mk_request("a")).await.unwrap();
        cache.complete(mk_request("b")).await.unwrap();
        cache.complete(mk_request("c")).await.unwrap(); // evicts "a"
        cache.complete(mk_request("a")).await.unwrap(); // miss — was evicted
        assert_eq!(stub.calls.load(Ordering::SeqCst), 4);
    }
}
