# Skill Capture & Reuse Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When a run completes with `Reviewer` verdict `Done(pass)`, capture the L0+L1 graph + task + review as a "skill" in `~/.local/share/graph-centric/skills/`, generate a slug + trigger via fast LLM asynchronously, and surface available skills (slug + one-liner) in the Proposer's system prompt for future runs.

**Architecture:** New `src/skills/` module with 6 files (mod, types, storage, slug, capture, retrieve). `SkillStorage` trait with 3 impls (`LocalSkillStorage` for writes, `RepoSkillStorage` for the git-tracked `skills/` root, `CompositeSkillStorage` for combined reads with local-first). `capture_skill(...)` returns a `JoinHandle<()>` (fire-and-forget). `list_for_prompt(...)` returns a formatted markdown section. Proposer gets a new `Option<Arc<dyn SkillStorage>>` field; when set, the system prompt includes the skills section.

**Tech Stack:** Rust 2024 edition, `serde`, `tokio`, `chrono = "0.4"`, `tempfile = "3"` (dev-dep). No other new external deps.

**Spec:** `docs/superpowers/specs/2026-06-03-skill-capture-and-reuse-design.md`

**Note on git:** This project does not currently have a git repository. Where the template shows `git commit` as a step, instead run `cargo check` (or `cargo test` for test tasks) to verify the change compiles and behaves correctly. The "checkpoint" idea still applies — verify state at each task boundary.

---

## File Structure

**New files (in `src/skills/`):**
- `mod.rs` — re-exports + module-level doc
- `types.rs` — `Skill`, `SkillMeta`, `SkillRef`, `SkillError`
- `storage.rs` — `SkillStorage` trait, `LocalSkillStorage`, `RepoSkillStorage`, `CompositeSkillStorage`
- `slug.rs` — `generate_slug(...)` (async LLM call + hash fallback)
- `capture.rs` — `capture_skill(...)` (async fire-and-forget orchestrator)
- `retrieve.rs` — `list_for_prompt(...)` (sync formatting)

**Modified files:**
- `Cargo.toml` — add `chrono = "0.4"` and `tempfile = "3"` (dev-dep)
- `src/lib.rs` — add `pub mod skills;`
- `src/agent/proposer.rs` — add `Option<Arc<dyn SkillStorage>>` field; inject skills section into `build_system_prompt`
- `src/bin/agent_a.rs` — call `capture_skill` on `LoopState::Done` with `Pass` verdict; discard the `JoinHandle`

**No changes to:** `src/graph/`, `src/model/`, `src/tools/`, `src/context/`, `src/error.rs`, other binaries.

---

## Task 1: Add `chrono` + `tempfile` dependencies and module skeleton

**Files:**
- Modify: `Cargo.toml` (add 2 deps)
- Modify: `src/lib.rs` (add `pub mod skills;`)
- Create: `src/skills/mod.rs` (empty stub)

- [ ] **Step 1: Add dependencies to `Cargo.toml`**

Read `/home/hhhh/Graph-Centric/Cargo.toml`. Add `chrono` to `[dependencies]` and `tempfile` to `[dev-dependencies]`:

```toml
[dependencies]
# ... existing entries ...
chrono = "0.4"
```

```toml
[dev-dependencies]
# ... (create this section if it doesn't exist) ...
tempfile = "3"
```

(Read the file first to see if `[dev-dependencies]` already exists.)

- [ ] **Step 2: Add module declaration to `src/lib.rs`**

Read `/home/hhhh/Graph-Centric/src/lib.rs`. Add this line at the bottom of the existing `pub mod` block (or wherever the modules are declared):

```rust
pub mod skills;
```

- [ ] **Step 3: Create empty `src/skills/mod.rs`**

Create `/home/hhhh/Graph-Centric/src/skills/mod.rs`:

```rust
//! Skill capture & reuse: reify successful agent runs as reusable skills.
//!
//! See `docs/superpowers/specs/2026-06-03-skill-capture-and-reuse-design.md`
//! for the design rationale.

pub mod types;
pub mod storage;
pub mod slug;
pub mod capture;
pub mod retrieve;

pub use types::{Skill, SkillError, SkillMeta, SkillRef};
pub use storage::{CompositeSkillStorage, LocalSkillStorage, RepoSkillStorage, SkillStorage};
```

(The other 4 files don't exist yet — the re-exports will fail to compile until the subsequent tasks create them. That's expected for TDD; the module compiles when all 4 submodules are added. Leave the `pub use` lines in place even though they fail compile until later tasks.)

- [ ] **Step 4: Verify cargo check still works (expected: errors about missing submodules)**

Run: `cargo check -p graph_harness 2>&1 | head -20`
Expected: 4 errors of the form "file not found for module `types`" (one per missing submodule). This confirms the module declaration is in place; subsequent tasks fill in the submodules.

---

## Task 2: Define `Skill`, `SkillMeta`, `SkillRef`, `SkillError`

**Files:**
- Create: `src/skills/types.rs`
- (Tests are inline in the same file)

- [ ] **Step 1: Write the types**

Create `/home/hhhh/Graph-Centric/src/skills/types.rs`:

```rust
//! Skill data types: `Skill`, `SkillMeta`, `SkillRef`, `SkillError`.

use crate::graph::{Graph, NodeId};
use crate::tools::HarnessError;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A single captured skill: the full L0+L1 graph of a successful run, plus
/// provenance and a one-sentence trigger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Kebab-case short name (e.g., "plan-relocation-bjs-sha").
    pub slug: String,
    /// The original user task that produced this skill.
    pub task: String,
    /// One-sentence "This skill applies when..." description.
    pub trigger: String,
    /// The L0 + L1 graph of the run.
    pub graph: Graph,
    /// The review verdict that approved this skill for capture.
    pub review: serde_json::Value,
    /// Provenance / metadata.
    pub meta: SkillMeta,
}

/// Metadata for a saved skill. Stored in `meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMeta {
    /// ISO 8601 timestamp of when this skill was captured.
    pub created_at: String,
    /// The task's NodeId in the original graph, if known.
    pub task_id: Option<NodeId>,
    /// Which model generated the slug and trigger.
    pub model_used: String,
    /// Domain tags derived from L0 node kinds (e.g., "code", "research").
    pub domain_tags: Vec<String>,
    /// Mean L1 confidence across all L1 entries; 0.0 if no L1.
    pub l1_avg_confidence: f64,
}

/// Lightweight reference used in prompts and listings. Carries only the
/// slug and the one-sentence trigger — no graph payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillRef {
    pub slug: String,
    pub trigger: String,
}

/// Errors from the skills module.
#[derive(Debug, Error)]
pub enum SkillError {
    #[error("skill not found: {0}")]
    NotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("invalid slug (must match ^[a-z0-9-]+$): {0}")]
    InvalidSlug(String),
    #[error("model call failed: {0}")]
    Model(String),
    #[error("harness error: {0}")]
    Harness(#[from] HarnessError),
}

pub type Result<T> = std::result::Result<T, SkillError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_meta_serializes_to_json() {
        let meta = SkillMeta {
            created_at: "2026-06-03T12:00:00Z".to_string(),
            task_id: None,
            model_used: "test-model".to_string(),
            domain_tags: vec!["code".to_string()],
            l1_avg_confidence: 0.85,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: SkillMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.created_at, meta.created_at);
        assert_eq!(back.model_used, meta.model_used);
        assert_eq!(back.l1_avg_confidence, meta.l1_avg_confidence);
    }

    #[test]
    fn skill_ref_is_slug_plus_trigger() {
        let r = SkillRef {
            slug: "my-skill".to_string(),
            trigger: "This skill applies when...".to_string(),
        };
        // Equality + serialization are the only operations that need to
        // round-trip cleanly. No graph payload.
        let json = serde_json::to_string(&r).unwrap();
        let back: SkillRef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn skill_error_implements_std_error() {
        let e = SkillError::NotFound("foo".to_string());
        // Just confirm Display works.
        assert!(format!("{e}").contains("foo"));
    }
}
```

- [ ] **Step 2: Add `mod types;` to `src/skills/mod.rs`**

Edit `src/skills/mod.rs` to declare the types submodule (the re-exports from Task 1 are already in place):

```rust
pub mod types;
```

- [ ] **Step 3: Run the new tests**

Run: `cargo test -p graph_harness skills::types::tests`
Expected: 3 tests pass (the 3 above). The other `pub mod` declarations in `mod.rs` will still fail compile, but the test runner filters to the specific module so it works.

If cargo test complains about the other missing submodules, use this invocation instead:

Run: `cargo test -p graph_harness --lib skills::types 2>&1 | tail -5`

- [ ] **Step 4: Verify only the types-related errors remain (other modules still missing)**

Run: `cargo check -p graph_harness 2>&1 | grep "error\[" | head -10`
Expected: errors ONLY for the 3 still-missing submodules (`storage`, `slug`, `capture`, `retrieve` — adjust as needed; we expect 3-4 errors). No errors in `types` itself.

---

## Task 3: `LocalSkillStorage` — read + write at the local root

**Files:**
- Create: `src/skills/storage.rs` (with `SkillStorage` trait + `LocalSkillStorage` + `RepoSkillStorage` + `CompositeSkillStorage` all in one file; the spec says storage is one file)

Actually, the spec says storage.rs has all 3 impls. But let me split to keep files focused:
- `src/skills/storage.rs` — trait + `LocalSkillStorage`
- `src/skills/storage_repo.rs` — `RepoSkillStorage`
- `src/skills/storage_composite.rs` — `CompositeSkillStorage`

The plan adjusts the spec's file layout: instead of one storage.rs, we use three files (one per impl). This keeps each file focused. Update the spec to match in a follow-up.

- [ ] **Step 1: Create `src/skills/storage.rs` with trait + LocalSkillStorage**

Create `/home/hhhh/Graph-Centric/src/skills/storage.rs`:

```rust
//! `SkillStorage` trait and the local-root implementation.

use super::types::{Result, Skill, SkillError, SkillRef};
use std::path::PathBuf;
use std::sync::Mutex;

/// Abstraction over a skill storage root. Implementations are `Send + Sync`
/// so they can be shared across the async runtime.
pub trait SkillStorage: Send + Sync {
    /// List all skills (slug + trigger one-liner only).
    fn list(&self) -> Result<Vec<SkillRef>>;

    /// Load a single skill by slug.
    fn load(&self, slug: &str) -> Result<Skill>;

    /// Save a skill. Implementations decide where it lands.
    fn save(&self, skill: &Skill) -> Result<()>;

    /// Path to the local root, if this storage has one. Used by tooling
    /// that wants to "promote" a local skill to repo.
    fn local_root(&self) -> Option<PathBuf>;

    /// Path to the repo root.
    fn repo_root(&self) -> PathBuf;
}

/// Local storage at `~/.local/share/graph-centric/skills/`. New skills land
/// here by default. Created lazily on first save.
pub struct LocalSkillStorage {
    root: PathBuf,
    // Serializes concurrent writes to the same slug.
    write_lock: Mutex<()>,
}

impl LocalSkillStorage {
    /// Construct with a custom root (for tests). Production callers use
    /// `LocalSkillStorage::default_install()`.
    pub fn new(root: PathBuf) -> Self {
        Self { root, write_lock: Mutex::new(()) }
    }

    /// Construct at the XDG default: `~/.local/share/graph-centric/skills/`.
    /// Returns `None` if `$HOME` is unset.
    pub fn default_install() -> Option<Self> {
        let home = std::env::var_os("HOME")?;
        let mut root = PathBuf::from(home);
        root.push(".local");
        root.push("share");
        root.push("graph-centric");
        root.push("skills");
        Some(Self::new(root))
    }

    fn skill_dir(&self, slug: &str) -> PathBuf {
        self.root.join(slug)
    }
}

impl SkillStorage for LocalSkillStorage {
    fn list(&self) -> Result<Vec<SkillRef>> {
        list_skill_refs(&self.root)
    }

    fn load(&self, slug: &str) -> Result<Skill> {
        load_skill_at(&self.skill_dir(slug))
    }

    fn save(&self, skill: &Skill) -> Result<()> {
        let _guard = self.write_lock.lock().unwrap();
        save_skill_at(&self.skill_dir(&skill.slug), skill)
    }

    fn local_root(&self) -> Option<PathBuf> {
        Some(self.root.clone())
    }

    fn repo_root(&self) -> PathBuf {
        // Local has no repo root; return the local root for symmetry.
        self.root.clone()
    }
}

// ---- Helpers shared with RepoSkillStorage ----

pub(crate) fn list_skill_refs(root: &std::path::Path) -> Result<Vec<SkillRef>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let trigger_path = entry.path().join("trigger.md");
        if !trigger_path.exists() {
            continue;
        }
        let trigger = std::fs::read_to_string(&trigger_path)
            .map_err(|e| SkillError::Io(e))?;
        let slug = entry.file_name().to_string_lossy().to_string();
        out.push(SkillRef { slug, trigger: trigger.trim().to_string() });
    }
    Ok(out)
}

pub(crate) fn load_skill_at(skill_dir: &std::path::Path) -> Result<Skill> {
    if !skill_dir.exists() {
        let slug = skill_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        return Err(SkillError::NotFound(slug));
    }
    let graph_path = skill_dir.join("graph.json");
    let task_path = skill_dir.join("task.md");
    let trigger_path = skill_dir.join("trigger.md");
    let review_path = skill_dir.join("review.json");
    let meta_path = skill_dir.join("meta.json");

    let graph_json = std::fs::read_to_string(&graph_path)?;
    let graph: crate::graph::Graph = serde_json::from_str(&graph_json)?;
    let task = std::fs::read_to_string(&task_path)?;
    let trigger = std::fs::read_to_string(&trigger_path)?;
    let review: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&review_path)?,
    )?;
    let meta: super::types::SkillMeta = serde_json::from_str(
        &std::fs::read_to_string(&meta_path)?,
    )?;

    let slug = skill_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(Skill {
        slug,
        task: task.trim().to_string(),
        trigger: trigger.trim().to_string(),
        graph,
        review,
        meta,
    })
}

pub(crate) fn save_skill_at(skill_dir: &std::path::Path, skill: &Skill) -> Result<()> {
    std::fs::create_dir_all(skill_dir)?;
    std::fs::write(skill_dir.join("task.md"), &skill.task)?;
    std::fs::write(skill_dir.join("trigger.md"), &skill.trigger)?;
    std::fs::write(
        skill_dir.join("graph.json"),
        serde_json::to_string_pretty(&skill.graph)?,
    )?;
    std::fs::write(
        skill_dir.join("review.json"),
        serde_json::to_string_pretty(&skill.review)?,
    )?;
    std::fs::write(
        skill_dir.join("meta.json"),
        serde_json::to_string_pretty(&skill.meta)?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;

    fn empty_skill(slug: &str) -> Skill {
        Skill {
            slug: slug.to_string(),
            task: "do the thing".to_string(),
            trigger: "This skill applies when the thing is needed.".to_string(),
            graph: Graph::new(),
            review: serde_json::json!({"verdict": "pass"}),
            meta: super::super::types::SkillMeta {
                created_at: "2026-06-03T00:00:00Z".to_string(),
                task_id: None,
                model_used: "test".to_string(),
                domain_tags: vec![],
                l1_avg_confidence: 0.0,
            },
        }
    }

    #[test]
    fn local_storage_creates_root_on_first_save() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("nested/skills");
        let storage = LocalSkillStorage::new(root.clone());
        storage.save(&empty_skill("foo")).unwrap();
        assert!(root.exists(), "local root should be created on first save");
    }

    #[test]
    fn local_storage_round_trips_skill() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalSkillStorage::new(dir.path().to_path_buf());
        let original = empty_skill("round-trip");
        storage.save(&original).unwrap();
        let loaded = storage.load("round-trip").unwrap();
        assert_eq!(loaded.slug, original.slug);
        assert_eq!(loaded.task, original.task);
        assert_eq!(loaded.trigger, original.trigger);
        assert_eq!(loaded.review, original.review);
        assert_eq!(loaded.meta.model_used, original.meta.model_used);
    }

    #[test]
    fn local_storage_returns_empty_when_root_missing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("does-not-exist");
        let storage = LocalSkillStorage::new(root);
        assert_eq!(storage.list().unwrap(), Vec::new());
    }

    #[test]
    fn local_storage_load_missing_skill_errors() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalSkillStorage::new(dir.path().to_path_buf());
        let err = storage.load("nope").unwrap_err();
        assert!(matches!(err, SkillError::NotFound(_)));
    }
}
```

- [ ] **Step 2: Add `mod storage;` to `src/skills/mod.rs`**

Edit `src/skills/mod.rs` to add the storage submodule:

```rust
pub mod storage;
```

(You'll need to add the placeholder for `slug`, `capture`, `retrieve` later in their own tasks. For now, just `mod storage;`.)

To keep the `cargo check` errors minimal at this step, **temporarily comment out** the corresponding `pub use` lines in `mod.rs` for the not-yet-existing submodules. Restore them in their respective tasks.

After the edit, `mod.rs` should look like:

```rust
//! Skill capture & reuse: reify successful agent runs as reusable skills.
//!
//! See `docs/superpowers/specs/2026-06-03-skill-capture-and-reuse-design.md`
//! for the design rationale.

pub mod types;
pub mod storage;
// pub mod slug;       // Task 6
// pub mod capture;     // Task 7
// pub mod retrieve;    // Task 8

pub use types::{Skill, SkillError, SkillMeta, SkillRef};
pub use storage::{CompositeSkillStorage, LocalSkillStorage, RepoSkillStorage, SkillStorage};
```

(Re-exports of not-yet-existing submodules: also comment out for now.)

- [ ] **Step 3: Run the new tests**

Run: `cargo test -p graph_harness --lib skills::storage 2>&1 | tail -10`
Expected: 4 tests pass.

If tests fail with `tempfile` not found, check that `tempfile = "3"` was added to `[dev-dependencies]` in Task 1.

- [ ] **Step 4: Run the full test suite**

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -3`
Expected: 311 tests pass (310 pre-existing + 1 new from Task 2 + 4 new from Task 3, minus any tests that may have been broken by the commented-out `pub use` lines; if any existing test broke, the implementer should investigate).

---

## Task 4: `RepoSkillStorage` — read at the repo root

**Files:**
- Create: `src/skills/storage_repo.rs`

- [ ] **Step 1: Create the file**

Create `/home/hhhh/Graph-Centric/src/skills/storage_repo.rs`:

```rust
//! Repo storage: read at `<project_root>/skills/`.

use super::storage::{list_skill_refs, load_skill_at, SkillStorage};
use super::types::{Result, Skill};
use std::path::PathBuf;

/// Read-only-ish storage at the repo root (`<project>/skills/`).
/// The harness treats this as "approved" skills; new saves go to local.
/// `save` is implemented for symmetry (allows direct writes for tooling
/// or tests) but is not called by `capture_skill` in v1.
pub struct RepoSkillStorage {
    root: PathBuf,
}

impl RepoSkillStorage {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl SkillStorage for RepoSkillStorage {
    fn list(&self) -> Result<Vec<super::types::SkillRef>> {
        list_skill_refs(&self.root)
    }

    fn load(&self, slug: &str) -> Result<Skill> {
        load_skill_at(&self.root.join(slug))
    }

    fn save(&self, skill: &Skill) -> Result<()> {
        // Delegate to the same helper used by LocalSkillStorage. This
        // writes 5 files under `<root>/<slug>/`. The harness never calls
        // this in v1, but tooling might.
        super::storage::save_skill_at(&self.root.join(&skill.slug), skill)
    }

    fn local_root(&self) -> Option<PathBuf> {
        None
    }

    fn repo_root(&self) -> PathBuf {
        self.root.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;
    use crate::skills::types::{Skill, SkillMeta};

    fn empty_skill(slug: &str) -> Skill {
        Skill {
            slug: slug.to_string(),
            task: "do the thing".to_string(),
            trigger: "This skill applies when needed.".to_string(),
            graph: Graph::new(),
            review: serde_json::json!({}),
            meta: SkillMeta {
                created_at: "2026-06-03T00:00:00Z".to_string(),
                task_id: None,
                model_used: "test".to_string(),
                domain_tags: vec![],
                l1_avg_confidence: 0.0,
            },
        }
    }

    #[test]
    fn repo_storage_round_trips_skill() {
        let dir = tempfile::tempdir().unwrap();
        let storage = RepoSkillStorage::new(dir.path().to_path_buf());
        storage.save(&empty_skill("r1")).unwrap();
        let loaded = storage.load("r1").unwrap();
        assert_eq!(loaded.slug, "r1");
    }

    #[test]
    fn repo_storage_list_returns_empty_for_missing_root() {
        let dir = tempfile::tempdir().unwrap();
        let storage = RepoSkillStorage::new(dir.path().join("nope"));
        assert_eq!(storage.list().unwrap(), Vec::new());
    }

    #[test]
    fn repo_storage_local_root_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let storage = RepoSkillStorage::new(dir.path().to_path_buf());
        assert_eq!(storage.local_root(), None);
    }
}
```

- [ ] **Step 2: Declare the module in `mod.rs`**

Edit `/home/hhhh/Graph-Centric/src/skills/mod.rs` to add:

```rust
pub mod storage_repo;
```

And re-enable (uncomment) the `pub use` line for `RepoSkillStorage` (it was already in the `pub use` block).

- [ ] **Step 3: Run the new tests**

Run: `cargo test -p graph_harness --lib skills::storage_repo 2>&1 | tail -10`
Expected: 3 tests pass.

- [ ] **Step 4: Run the full test suite**

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -3`
Expected: 314 tests pass.

---

## Task 5: `CompositeSkillStorage` — combined read with local-first dedup

**Files:**
- Create: `src/skills/storage_composite.rs`

- [ ] **Step 1: Create the file**

Create `/home/hhhh/Graph-Centric/src/skills/storage_composite.rs`:

```rust
//! Composite storage: combines local + repo, with local-first dedup by slug.

use super::storage::{LocalSkillStorage, SkillStorage};
use super::types::{Result, Skill, SkillError, SkillRef};
use std::collections::HashSet;

/// Combines an optional local storage and a repo storage. `list()` returns
/// local entries first; on slug collision, the local version wins.
///
/// `save()` is intentionally NOT exposed: new saves always go to local
/// (call `LocalSkillStorage::save` directly), and the user promotes
/// via filesystem.
pub struct CompositeSkillStorage {
    local: Option<LocalSkillStorage>,
    repo: super::storage_repo::RepoSkillStorage,
}

impl CompositeSkillStorage {
    pub fn new(local: Option<LocalSkillStorage>, repo: super::storage_repo::RepoSkillStorage) -> Self {
        Self { local, repo }
    }
}

impl SkillStorage for CompositeSkillStorage {
    fn list(&self) -> Result<Vec<SkillRef>> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<SkillRef> = Vec::new();

        // Local first.
        if let Some(local) = &self.local {
            for r in local.list()? {
                if seen.insert(r.slug.clone()) {
                    out.push(r);
                }
            }
        }
        // Then repo, skipping slugs already seen.
        for r in self.repo.list()? {
            if seen.insert(r.slug.clone()) {
                out.push(r);
            }
        }
        Ok(out)
    }

    fn load(&self, slug: &str) -> Result<Skill> {
        // Local first, fall back to repo.
        if let Some(local) = &self.local {
            if let Ok(skill) = local.load(slug) {
                return Ok(skill);
            }
        }
        self.repo.load(slug)
    }

    fn save(&self, _skill: &Skill) -> Result<()> {
        Err(SkillError::Model(
            "CompositeSkillStorage::save is not supported; use LocalSkillStorage::save".into()
        ))
    }

    fn local_root(&self) -> Option<std::path::PathBuf> {
        self.local.as_ref().and_then(|l| l.local_root())
    }

    fn repo_root(&self) -> std::path::PathBuf {
        self.repo.repo_root()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;
    use crate::skills::storage_repo::RepoSkillStorage;
    use crate::skills::types::{Skill, SkillMeta};
    use std::path::PathBuf;

    fn empty_skill(slug: &str) -> Skill {
        Skill {
            slug: slug.to_string(),
            task: "task".to_string(),
            trigger: "trigger".to_string(),
            graph: Graph::new(),
            review: serde_json::json!({}),
            meta: SkillMeta {
                created_at: "2026-06-03T00:00:00Z".to_string(),
                task_id: None,
                model_used: "test".to_string(),
                domain_tags: vec![],
                l1_avg_confidence: 0.0,
            },
        }
    }

    fn composite_with_both(local_root: PathBuf, repo_root: PathBuf) -> CompositeSkillStorage {
        CompositeSkillStorage::new(
            Some(LocalSkillStorage::new(local_root)),
            RepoSkillStorage::new(repo_root),
        )
    }

    #[test]
    fn composite_storage_lists_local_first() {
        let local = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let c = composite_with_both(local.path().to_path_buf(), repo.path().to_path_buf());
        c.repo.save(&empty_skill("a")).unwrap();
        c.local.as_ref().unwrap().save(&empty_skill("b")).unwrap();

        let list = c.list().unwrap();
        // Local-first ordering: b (local) before a (repo).
        assert_eq!(list[0].slug, "b");
        assert_eq!(list[1].slug, "a");
    }

    #[test]
    fn composite_storage_dedupes_by_slug() {
        let local = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let c = composite_with_both(local.path().to_path_buf(), repo.path().to_path_buf());
        c.repo.save(&empty_skill("dup")).unwrap();
        c.local.as_ref().unwrap().save(&empty_skill("dup")).unwrap();

        let list = c.list().unwrap();
        assert_eq!(list.len(), 1, "duplicate slug should appear once");
        assert_eq!(list[0].slug, "dup");
    }

    #[test]
    fn composite_storage_load_prefers_local() {
        let local = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let c = composite_with_both(local.path().to_path_buf(), repo.path().to_path_buf());

        let mut repo_skill = empty_skill("x");
        repo_skill.trigger = "from-repo".to_string();
        c.repo.save(&repo_skill).unwrap();

        let mut local_skill = empty_skill("x");
        local_skill.trigger = "from-local".to_string();
        c.local.as_ref().unwrap().save(&local_skill).unwrap();

        let loaded = c.load("x").unwrap();
        assert_eq!(loaded.trigger, "from-local");
    }

    #[test]
    fn composite_storage_load_falls_back_to_repo() {
        let local = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let c = composite_with_both(local.path().to_path_buf(), repo.path().to_path_buf());
        c.repo.save(&empty_skill("only-in-repo")).unwrap();

        let loaded = c.load("only-in-repo").unwrap();
        assert_eq!(loaded.slug, "only-in-repo");
    }

    #[test]
    fn composite_storage_save_errors() {
        let local = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let c = composite_with_both(local.path().to_path_buf(), repo.path().to_path_buf());
        let err = c.save(&empty_skill("x")).unwrap_err();
        assert!(matches!(err, SkillError::Model(_)));
    }
}
```

- [ ] **Step 2: Declare the module in `mod.rs`**

Edit `/home/hhhh/Graph-Centric/src/skills/mod.rs` to add:

```rust
pub mod storage_composite;
```

- [ ] **Step 3: Run the new tests**

Run: `cargo test -p graph_harness --lib skills::storage_composite 2>&1 | tail -10`
Expected: 5 tests pass.

- [ ] **Step 4: Run the full test suite**

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -3`
Expected: 319 tests pass.

---

## Task 6: `generate_slug` — LLM call with hash fallback

**Files:**
- Create: `src/skills/slug.rs`

- [ ] **Step 1: Create the file**

Create `/home/hhhh/Graph-Centric/src/skills/slug.rs`:

```rust
//! LLM-based slug generation, with deterministic hash fallback.

use super::types::Result;
use crate::model::Model;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// Generate a kebab-case slug for a skill, via the fast model. Falls back
/// to `task-<hash>` if the model returns something invalid.
///
/// The prompt is fixed and demands a single-line reply; the model is
/// expected to return only the slug (no markdown fences, no prose).
pub async fn generate_slug(
    model: Arc<dyn Model>,
    task: &str,
    graph_summary: &str,
) -> Result<String> {
    let prompt = format!(
        "Task: {task}\n\n\
         Graph summary: {graph_summary}\n\n\
         Generate a 3-5 word kebab-case slug (lowercase, hyphens only) that \
         names this skill. Examples: plan-relocation-bjs-sha, \
         refactor-rust-traits, cargo-build-debug, write-marketing-blog.\n\n\
         Output ONLY the slug. No prose, no quotes, no markdown."
    );

    let request = crate::model::ModelRequest {
        messages: vec![crate::model::Message::user(prompt)],
        tools: Vec::new(),
        temperature: 0.3,
        max_tokens: Some(32),
        stop: Vec::new(),
    };

    let response = model.complete(request).await.map_err(|e| {
        super::types::SkillError::Model(format!("generate_slug: {e}"))
    })?;

    let raw = response.content.trim().to_string();
    if is_valid_slug(&raw) {
        return Ok(raw);
    }
    Ok(fallback_slug(task))
}

/// True if `s` matches `^[a-z0-9-]+$` (after trimming) and has at least one char.
fn is_valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Deterministic fallback: `task-<16-hex hash of task>`. Never collides
/// with LLM slugs in practice (LLMs use semantic words; this is hex).
fn fallback_slug(task: &str) -> String {
    let mut hasher = DefaultHasher::new();
    task.hash(&mut hasher);
    format!("task-{:x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use crate::error::HarnessError;
    use crate::model::{FinishReason, Message, ModelRequest, ModelResponse, Role, Usage};
    use std::sync::Mutex;

    struct MockModel {
        responses: Mutex<Vec<String>>,
    }

    impl MockModel {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().rev().map(String::from).collect()),
            }
        }
    }

    #[async_trait]
    impl Model for MockModel {
        fn name(&self) -> &str { "mock" }
        async fn complete(
            &self,
            _req: ModelRequest,
        ) -> std::result::Result<ModelResponse, HarnessError> {
            let content = self.responses.lock().unwrap().pop()
                .unwrap_or_else(|| "default-slug".to_string());
            Ok(ModelResponse {
                content,
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
            })
        }
    }

    #[tokio::test]
    async fn generate_slug_uses_model_response_when_valid() {
        let m: Arc<dyn Model> = Arc::new(MockModel::new(vec!["my-cool-skill"]));
        let s = generate_slug(m, "do the thing", "5 nodes").await.unwrap();
        assert_eq!(s, "my-cool-skill");
    }

    #[tokio::test]
    async fn generate_slug_trims_whitespace() {
        let m: Arc<dyn Model> = Arc::new(MockModel::new(vec!["  trimmed-slug  \n"]));
        let s = generate_slug(m, "task", "graph").await.unwrap();
        assert_eq!(s, "trimmed-slug");
    }

    #[tokio::test]
    async fn generate_slug_falls_back_on_invalid_chars() {
        let m: Arc<dyn Model> = Arc::new(MockModel::new(vec!["Bad Slug!!"]));
        let s = generate_slug(m, "the task", "graph").await.unwrap();
        assert!(s.starts_with("task-"));
        // Hex hash should follow.
        assert_eq!(s.len(), "task-".len() + 16);
    }

    #[tokio::test]
    async fn generate_slug_falls_back_on_empty_response() {
        let m: Arc<dyn Model> = Arc::new(MockModel::new(vec!["   "]));
        let s = generate_slug(m, "task", "graph").await.unwrap();
        assert!(s.starts_with("task-"));
    }

    #[test]
    fn is_valid_slug_accepts_typical_slugs() {
        assert!(is_valid_slug("my-skill"));
        assert!(is_valid_slug("plan-relocation-bjs-sha"));
        assert!(is_valid_slug("cargo-build-debug"));
        assert!(is_valid_slug("a-b-c-1-2-3"));
    }

    #[test]
    fn is_valid_slug_rejects_bad_inputs() {
        assert!(!is_valid_slug(""));
        assert!(!is_valid_slug("Bad-Slug"));
        assert!(!is_valid_slug("bad slug"));
        assert!(!is_valid_slug("bad_slug!"));
        assert!(!is_valid_slug("中文-slug"));
    }

    #[test]
    fn fallback_slug_is_deterministic() {
        let a = fallback_slug("the same task");
        let b = fallback_slug("the same task");
        let c = fallback_slug("a different task");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
```

NOTE: This file uses `Model`, `ModelRequest`, `Message`, etc. from `crate::model`. Check the existing `src/model/mod.rs` to confirm the exact type names. If `Message::user(content)` doesn't exist, use `Message { role: Role::User, content }` (the struct form) — adjust as needed.

- [ ] **Step 2: Declare the module in `mod.rs`**

Edit `/home/hhhh/Graph-Centric/src/skills/mod.rs` to add:

```rust
pub mod slug;
```

- [ ] **Step 3: Run the new tests**

Run: `cargo test -p graph_harness --lib skills::slug 2>&1 | tail -10`
Expected: 7 tests pass (4 async + 3 sync).

If `cargo test` complains about unused imports (e.g., `Role`, `Message::user`), fix the imports in the test module.

- [ ] **Step 4: Run the full test suite**

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -3`
Expected: 326 tests pass.

---

## Task 7: `capture_skill` — async fire-and-forget orchestrator

**Files:**
- Create: `src/skills/capture.rs`

- [ ] **Step 1: Create the file**

Create `/home/hhhh/Graph-Centric/src/skills/capture.rs`:

```rust
//! Async fire-and-forget skill capture. Returns immediately; the actual
//! save (with two fast LLM calls) happens in a spawned tokio task.

use super::slug::generate_slug;
use super::storage::LocalSkillStorage;
use super::types::{Result, Skill, SkillError, SkillMeta};
use crate::graph::Graph;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::task::JoinHandle;

use crate::model::Model;

/// The function the caller (e.g., `bin/agent_a.rs`) invokes when a run
/// completes with `Reviewer` verdict `Pass`.
///
/// Returns a `JoinHandle<()>` immediately. The caller typically just
/// discards it. The spawned task runs:
/// 1. Generate slug (fast LLM call, ~1s)
/// 2. Generate trigger (fast LLM call, ~1-2s)
/// 3. Save to local skill storage
///
/// If any step fails, the skill is NOT saved; an error is logged at
/// `warn!` level. No partial-save mode in v1.
pub fn capture_skill(
    graph: Graph,
    review: serde_json::Value,
    task: String,
    task_id: Option<crate::graph::NodeId>,
    model: Arc<dyn Model>,
    storage: Arc<LocalSkillStorage>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = capture_inner(graph, review, task, task_id, model, storage).await {
            tracing::warn!("skill capture failed: {e}");
        }
    })
}

async fn capture_inner(
    graph: Graph,
    review: serde_json::Value,
    task: String,
    task_id: Option<crate::graph::NodeId>,
    model: Arc<dyn Model>,
    storage: Arc<LocalSkillStorage>,
) -> Result<()> {
    let started = SystemTime::now();

    // 1. Slug
    let graph_summary = render_graph_summary(&graph);
    let slug = generate_slug(model.clone(), &task, &graph_summary).await?;

    // 2. Trigger
    let trigger = generate_trigger(model.clone(), &task, &graph_summary).await?;

    // 3. Metadata
    let meta = SkillMeta {
        created_at: iso8601_now(),
        task_id,
        model_used: model.name().to_string(),
        domain_tags: compute_domain_tags(&graph),
        l1_avg_confidence: l1_avg_confidence(&graph),
    };

    let skill = Skill {
        slug: slug.clone(),
        task,
        trigger: trigger.clone(),
        graph,
        review,
        meta,
    };

    // 4. Save
    storage.save(&skill)?;
    let elapsed = started.elapsed().map(|d| d.as_secs_f64()).unwrap_or(0.0);
    tracing::info!(
        skill = %slug,
        trigger = %trigger,
        l1_avg = skill.meta.l1_avg_confidence,
        elapsed_s = elapsed,
        "skill captured"
    );
    Ok(())
}

async fn generate_trigger(
    model: Arc<dyn Model>,
    task: &str,
    graph_summary: &str,
) -> Result<String> {
    let prompt = format!(
        "Task: {task}\n\n\
         Graph summary: {graph_summary}\n\n\
         Write ONE sentence starting with 'This skill applies when user asks about' \
         (or 'This skill applies when' for non-user-driven contexts). \
         The sentence should let a future agent decide when to consult this skill. \
         Output ONLY the sentence, no markdown, no preamble."
    );

    let request = crate::model::ModelRequest {
        messages: vec![crate::model::Message::user(prompt)],
        tools: Vec::new(),
        temperature: 0.3,
        max_tokens: Some(80),
        stop: Vec::new(),
    };

    let response = model.complete(request).await.map_err(|e| {
        SkillError::Model(format!("generate_trigger: {e}"))
    })?;

    let raw = response.content.trim().to_string();
    if raw.is_empty() {
        return Err(SkillError::Model("empty trigger response".into()));
    }
    Ok(raw)
}

fn render_graph_summary(graph: &Graph) -> String {
    // Cheap one-liner: node count + edge count + a few sample node ids.
    let n = graph.node_count();
    let e = graph.edge_count();
    let sample: Vec<String> = graph
        .nodes
        .keys()
        .take(5)
        .map(|id| id.to_string())
        .collect();
    format!("{n} nodes, {e} edges; sample: {}", sample.join(", "))
}

fn compute_domain_tags(graph: &Graph) -> Vec<String> {
    use crate::graph::NodeKind;
    use std::collections::BTreeSet;
    let mut tags: BTreeSet<String> = BTreeSet::new();
    for node in graph.nodes.values() {
        match node.kind {
            NodeKind::File | NodeKind::Function | NodeKind::Class | NodeKind::Module => {
                tags.insert("code".to_string());
            }
            NodeKind::Config => {
                tags.insert("infra".to_string());
            }
            NodeKind::Task => {
                tags.insert("business".to_string());
            }
            NodeKind::Other(_) => {
                // Skip — could be anything.
            }
        }
    }
    tags.into_iter().collect()
}

fn l1_avg_confidence(graph: &Graph) -> f64 {
    let confidences: Vec<f64> = graph.l1.values().map(|d| d.confidence).collect();
    if confidences.is_empty() {
        0.0
    } else {
        confidences.iter().sum::<f64>() / confidences.len() as f64
    }
}

fn iso8601_now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[allow(dead_code)]
fn _suppress_unused_for_paths() -> PathBuf {
    PathBuf::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Graph, Node, NodeKind};
    use async_trait::async_trait;
    use crate::error::HarnessError;
    use crate::model::{FinishReason, Model, ModelRequest, ModelResponse, Usage};
    use std::sync::Mutex;

    struct MockModel {
        responses: Mutex<Vec<String>>,
    }

    impl MockModel {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().rev().map(String::from).collect()),
            }
        }
    }

    #[async_trait]
    impl Model for MockModel {
        fn name(&self) -> &str { "mock" }
        async fn complete(
            &self,
            _req: ModelRequest,
        ) -> std::result::Result<ModelResponse, HarnessError> {
            let content = self.responses.lock().unwrap().pop()
                .unwrap_or_else(|| "default".to_string());
            Ok(ModelResponse {
                content,
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
            })
        }
    }

    fn sample_graph_with_l1() -> Graph {
        let mut g = Graph::new();
        g.add_node(Node::file("foo.rs", "foo"));
        g.add_node(Node::file("bar.rs", "bar"));
        // Set a sample L1 confidence.
        g.l1.set(
            "foo.rs".into(),
            crate::graph::L1Description::new("x", "y", "z", "w").with_confidence(0.8),
        );
        g
    }

    #[tokio::test]
    async fn capture_skill_writes_five_files() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
        let m: Arc<dyn Model> = Arc::new(MockModel::new(vec![
            "This skill applies when user asks to do the thing.",
            "do-the-thing",
        ]));

        let handle = capture_skill(
            sample_graph_with_l1(),
            serde_json::json!({"verdict": "pass"}),
            "do the thing".to_string(),
            None,
            m,
            storage.clone(),
        );
        handle.await.unwrap();

        // The captured skill lives in a subdirectory named after the slug.
        let skill_dir = dir.path().join("do-the-thing");
        assert!(skill_dir.join("task.md").exists());
        assert!(skill_dir.join("trigger.md").exists());
        assert!(skill_dir.join("graph.json").exists());
        assert!(skill_dir.join("review.json").exists());
        assert!(skill_dir.join("meta.json").exists());
    }

    #[tokio::test]
    async fn capture_skill_uses_slug_from_model() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
        let m: Arc<dyn Model> = Arc::new(MockModel::new(vec![
            "trigger text",
            "my-named-skill",
        ]));

        capture_skill(
            sample_graph_with_l1(),
            serde_json::json!({}),
            "task".to_string(),
            None,
            m,
            storage.clone(),
        ).await;
        // After both LLM calls (slug first, then trigger), the skill
        // directory uses the LLM-provided slug.
        assert!(dir.path().join("my-named-skill").exists());
    }

    #[tokio::test]
    async fn capture_skill_does_not_save_on_llm_error() {
        struct FailingModel;
        #[async_trait]
        impl Model for FailingModel {
            fn name(&self) -> &str { "failing" }
            async fn complete(
                &self,
                _req: ModelRequest,
            ) -> std::result::Result<ModelResponse, HarnessError> {
                Err(HarnessError::model("simulated failure"))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
        let m: Arc<dyn Model> = Arc::new(FailingModel);

        let handle = capture_skill(
            sample_graph_with_l1(),
            serde_json::json!({}),
            "task".to_string(),
            None,
            m,
            storage,
        );
        handle.await.unwrap();
        // No subdirectory should have been created.
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(entries.is_empty(), "skill dir should be empty on LLM failure");
    }

    #[tokio::test]
    async fn capture_skill_includes_l1_confidence_in_meta() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
        let m: Arc<dyn Model> = Arc::new(MockModel::new(vec![
            "trigger",
            "conf-test",
        ]));

        capture_skill(
            sample_graph_with_l1(),  // one L1 entry with confidence 0.8
            serde_json::json!({}),
            "task".to_string(),
            None,
            m,
            storage.clone(),
        ).await;
        let meta: SkillMeta = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("conf-test/meta.json")).unwrap(),
        ).unwrap();
        assert!((meta.l1_avg_confidence - 0.8).abs() < 1e-9);
    }

    #[tokio::test]
    async fn capture_skill_computes_domain_tags() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
        let m: Arc<dyn Model> = Arc::new(MockModel::new(vec![
            "trigger",
            "domain-test",
        ]));

        let mut g = Graph::new();
        g.add_node(Node::file("a.rs", "a"));
        g.add_node(Node::task("t1", "do something"));
        capture_skill(
            g,
            serde_json::json!({}),
            "task".to_string(),
            None,
            m,
            storage.clone(),
        ).await;
        let meta: SkillMeta = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("domain-test/meta.json")).unwrap(),
        ).unwrap();
        assert!(meta.domain_tags.contains(&"code".to_string()));
        assert!(meta.domain_tags.contains(&"business".to_string()));
    }
}
```

- [ ] **Step 2: Declare the module in `mod.rs`**

Edit `/home/hhhh/Graph-Centric/src/skills/mod.rs` to add:

```rust
pub mod capture;
```

- [ ] **Step 3: Run the new tests**

Run: `cargo test -p graph_harness --lib skills::capture 2>&1 | tail -10`
Expected: 5 tests pass.

- [ ] **Step 4: Run the full test suite**

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -3`
Expected: 331 tests pass.

---

## Task 8: `list_for_prompt` — sync formatting with 20-skill cap

**Files:**
- Create: `src/skills/retrieve.rs`

- [ ] **Step 1: Create the file**

Create `/home/hhhh/Graph-Centric/src/skills/retrieve.rs`:

```rust
//! Format available skills as a markdown section for the Proposer's
//! system prompt.

use super::storage::SkillStorage;
use super::types::SkillRef;

/// The maximum number of skills surfaced in a single Proposer prompt.
/// Older skills are git history anyway; if a user has more than 20 they
/// can promote the relevant ones to a separate index in v2.
const MAX_SKILLS_IN_PROMPT: usize = 20;

/// Build the "## Available skills" markdown section. Returns `""` if
/// no skills are found. The format is a compact one-liner per skill
/// (slug + trigger), per user direction.
pub fn list_for_prompt(storage: &dyn SkillStorage) -> String {
    let all = match storage.list() {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    if all.is_empty() {
        return String::new();
    }
    let n = all.len().min(MAX_SKILLS_IN_PROMPT);
    let mut out = String::from("## Available skills (auto-curated from past successful runs)\n\n");
    for r in all.iter().take(n) {
        out.push_str(&format!("- **{}**: \"{}\"\n", r.slug, r.trigger));
    }
    if all.len() > MAX_SKILLS_IN_PROMPT {
        out.push_str(&format!(
            "\n(plus {} more; not shown)\n",
            all.len() - MAX_SKILLS_IN_PROMPT
        ));
    }
    out
}

/// Build a short header (e.g. "12 skills available"). Useful for log lines.
pub fn count_label(storage: &dyn SkillStorage) -> String {
    let n = storage.list().map(|v| v.len()).unwrap_or(0);
    format!("{n} skill(s) available")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;
    use crate::skills::storage::LocalSkillStorage;
    use crate::skills::types::{Skill, SkillMeta};

    fn empty_skill(slug: &str, trigger: &str) -> Skill {
        Skill {
            slug: slug.to_string(),
            task: "task".to_string(),
            trigger: trigger.to_string(),
            graph: Graph::new(),
            review: serde_json::json!({}),
            meta: SkillMeta {
                created_at: "2026-06-03T00:00:00Z".to_string(),
                task_id: None,
                model_used: "test".to_string(),
                domain_tags: vec![],
                l1_avg_confidence: 0.0,
            },
        }
    }

    #[test]
    fn list_for_prompt_with_no_skills_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalSkillStorage::new(dir.path().to_path_buf());
        assert_eq!(list_for_prompt(&storage), "");
    }

    #[test]
    fn list_for_prompt_includes_section_header() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalSkillStorage::new(dir.path().to_path_buf());
        storage.save(&empty_skill("a", "does A")).unwrap();
        let s = list_for_prompt(&storage);
        assert!(s.contains("## Available skills"));
    }

    #[test]
    fn list_for_prompt_formats_one_liner_per_skill() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalSkillStorage::new(dir.path().to_path_buf());
        storage.save(&empty_skill("alpha", "applies when X")).unwrap();
        storage.save(&empty_skill("beta", "applies when Y")).unwrap();
        let s = list_for_prompt(&storage);
        // One bullet per skill.
        assert_eq!(s.matches("\n- ").count(), 2);
        assert!(s.contains("**alpha**"));
        assert!(s.contains("**beta**"));
    }

    #[test]
    fn list_for_prompt_caps_at_20_skills() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalSkillStorage::new(dir.path().to_path_buf());
        for i in 0..25 {
            storage.save(&empty_skill(
                &format!("skill-{i:02}"),
                &format!("trigger {i}"),
            )).unwrap();
        }
        let s = list_for_prompt(&storage);
        // 20 bullets, plus a "(plus 5 more...)" footer.
        assert_eq!(s.matches("\n- ").count(), 20);
        assert!(s.contains("plus 5 more"));
    }

    #[test]
    fn count_label_reports_zero_or_more() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalSkillStorage::new(dir.path().to_path_buf());
        assert_eq!(count_label(&storage), "0 skill(s) available");
        storage.save(&empty_skill("a", "t")).unwrap();
        assert_eq!(count_label(&storage), "1 skill(s) available");
        storage.save(&empty_skill("b", "t")).unwrap();
        assert_eq!(count_label(&storage), "2 skill(s) available");
    }
}
```

- [ ] **Step 2: Declare the module in `mod.rs`**

Edit `/home/hhhh/Graph-Centric/src/skills/mod.rs` to add:

```rust
pub mod retrieve;
```

- [ ] **Step 3: Re-enable all `pub use` lines in `mod.rs`**

The full `mod.rs` should now look like:

```rust
//! Skill capture & reuse: reify successful agent runs as reusable skills.
//!
//! See `docs/superpowers/specs/2026-06-03-skill-capture-and-reuse-design.md`
//! for the design rationale.

pub mod types;
pub mod storage;
pub mod storage_repo;
pub mod storage_composite;
pub mod slug;
pub mod capture;
pub mod retrieve;

pub use types::{Skill, SkillError, SkillMeta, SkillRef};
pub use storage::{CompositeSkillStorage, LocalSkillStorage, RepoSkillStorage, SkillStorage};
```

(Yes, `CompositeSkillStorage` is in `storage_composite` not `storage`. Add the re-export to a `pub use` line for that submodule too. Update the `pub use storage::...` line to point to the right submodule, OR add a separate re-export for `storage_composite`.)

The simplest fix: add at the end of `mod.rs`:

```rust
pub use storage_composite::CompositeSkillStorage;
```

- [ ] **Step 4: Run the new tests**

Run: `cargo test -p graph_harness --lib skills::retrieve 2>&1 | tail -10`
Expected: 5 tests pass.

- [ ] **Step 5: Run the full test suite**

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -3`
Expected: 336 tests pass.

---

## Task 9: Wire Proposer to inject skills section

**Files:**
- Modify: `src/agent/proposer.rs`

- [ ] **Step 1: Read the Proposer to find the right insertion point**

Read `/home/hhhh/Graph-Centric/src/agent/proposer.rs`. Find:
- The `GraphProposer` struct definition (around line 73-80)
- The `new` constructor
- The `build_system_prompt` method (around line 114)

- [ ] **Step 2: Add the `skills` field to the struct**

In the `GraphProposer` struct, add a new field after `tools`:

```rust
    /// Optional storage of past successful-run skills. When set, the
    /// system prompt includes a "## Available skills" section listing
    /// them. When `None`, no section is included.
    pub skills: Option<std::sync::Arc<dyn crate::skills::SkillStorage>>,
```

- [ ] **Step 3: Update `new` to initialize the field**

Find the existing `new` constructor. After the `tools: Arc<ToolRegistry>` parameter, add a new `skills` parameter:

```rust
    pub fn new(
        model: Arc<dyn Model>,
        tools: Arc<ToolRegistry>,
        skills: Option<std::sync::Arc<dyn crate::skills::SkillStorage>>,
    ) -> Self {
        Self {
            model,
            tools,
            skills,
            temperature: 0.2,
            max_tokens: Some(32768),
        }
    }
```

- [ ] **Step 4: Add a `with_skills` builder method**

After the existing `with_max_tokens` builder, add:

```rust
    /// Attach a skill storage. The Proposer will list available skills
    /// in its system prompt.
    pub fn with_skills(
        mut self,
        skills: std::sync::Arc<dyn crate::skills::SkillStorage>,
    ) -> Self {
        self.skills = Some(skills);
        self
    }
```

- [ ] **Step 5: Update `build_system_prompt` to inject the skills section**

Find `build_system_prompt`. The current implementation returns a `String`
that's the system prompt for the Proposer. We need to append the skills
section before returning.

The cleanest place: at the end of the existing `format!` block, append
the skills section if set. Read the current `build_system_prompt` and
find the format string's closing. Just before `format!` returns the
string, add:

```rust
        // Append the skills section if a storage is attached.
        if let Some(skills) = &self.skills {
            let section = crate::skills::retrieve::list_for_prompt(skills.as_ref());
            if !section.is_empty() {
                prompt.push_str("\n\n");
                prompt.push_str(&section);
            }
        }
```

(`prompt` is whatever the local variable in the function is. Adjust as needed — you may need to refactor the existing function to bind the format result to a mutable string first. The simplest version: change the `format!(...)` to assign to a `let mut prompt = format!(...)` then push the section, then return.)

- [ ] **Step 6: Update existing call sites of `GraphProposer::new`**

There are existing test files and possibly `bin/agent_a.rs` that call
`GraphProposer::new(model, tools)`. They will fail to compile because
the new signature requires a third argument. Update each call site to
pass `None` for `skills`:

```rust
GraphProposer::new(model, tools, None)
```

Search for `GraphProposer::new(` in the codebase and update each call
site. Likely locations:
- `src/agent/proposer.rs` itself (in the test module)
- `src/bin/agent_a.rs`

- [ ] **Step 7: Add a test that skills section is injected when set**

In the `mod tests` block of `proposer.rs`, add:

```rust
    #[test]
    fn proposer_system_prompt_includes_skills_section_when_storage_set() {
        use crate::graph::Graph;
        use crate::skills::storage::LocalSkillStorage;
        use crate::skills::types::{Skill, SkillMeta};
        use std::sync::Arc;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let storage = LocalSkillStorage::new(dir.path().to_path_buf());
        let skill = Skill {
            slug: "demo-skill".to_string(),
            task: "do X".to_string(),
            trigger: "applies when X is needed".to_string(),
            graph: Graph::new(),
            review: serde_json::json!({}),
            meta: SkillMeta {
                created_at: "2026-06-03T00:00:00Z".to_string(),
                task_id: None,
                model_used: "test".to_string(),
                domain_tags: vec![],
                l1_avg_confidence: 0.0,
            },
        };
        storage.save(&skill).unwrap();

        let storage_arc: Arc<dyn crate::skills::SkillStorage> = Arc::new(storage);
        let model: Arc<dyn Model> = Arc::new(crate::model::openai_compat::OpenAICompat::new(
            "test", "http://localhost:0", "test",
        ).expect("stub model"));
        let proposer = GraphProposer::new(
            model,
            Arc::new(crate::tools::ToolRegistry::new()),
            Some(storage_arc),
        );
        let prompt = proposer.build_system_prompt("any task");
        assert!(prompt.contains("## Available skills"));
        assert!(prompt.contains("demo-skill"));
    }

    #[test]
    fn proposer_system_prompt_omits_skills_section_when_storage_none() {
        let model: Arc<dyn Model> = Arc::new(crate::model::openai_compat::OpenAICompat::new(
            "test", "http://localhost:0", "test",
        ).expect("stub model"));
        let proposer = GraphProposer::new(
            model,
            Arc::new(crate::tools::ToolRegistry::new()),
            None,
        );
        let prompt = proposer.build_system_prompt("any task");
        assert!(!prompt.contains("## Available skills"));
    }
```

NOTE: This test uses `tempfile::TempDir` (already in dev-deps from Task 1). The exact constructor for `OpenAICompat` may differ — look at the existing proposer tests for the right pattern.

- [ ] **Step 8: Run the new tests and the full suite**

Run: `cargo test -p graph_harness --lib agent::proposer 2>&1 | tail -10`
Expected: All proposer tests pass, including the 2 new ones.

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -3`
Expected: 338 tests pass.

---

## Task 10: Wire `bin/agent_a.rs` to call `capture_skill` on Done+pass

**Files:**
- Modify: `src/bin/agent_a.rs`

- [ ] **Step 1: Read the file to find the loop's `Done` handling**

Read `/home/hhhh/Graph-Centric/src/bin/agent_a.rs`. Find:
- The main loop that calls `gl.step()`
- The match arm that handles `LoopState::Done(FinalResult)`
- The reviewer invocation (so we can pass the `ReviewResult` to capture)

- [ ] **Step 2: Find the path that gets to `Done` with the ReviewResult**

The `LoopState::Done(FinalResult)` arm in the event loop should have access to the latest review (or to the graph at time of review). The cleanest approach:
- Save the `ReviewResult` to a local variable in the main loop when the review runs
- When `Done` is reached, read that saved `ReviewResult`
- If it's `Pass`, fire `capture_skill`

If the current `agent_a.rs` doesn't already save the `ReviewResult`, add a `let last_review: Option<ReviewResult> = None;` and update it whenever the reviewer runs.

- [ ] **Step 3: Add the capture call in the `Done` arm**

In the `LoopState::Done(FinalResult)` match arm, add the capture call. It should be after the Done-specific logging, before the break/exit. Pseudo-code:

```rust
LoopState::Done(final_result) => {
    info!(...);
    // NEW: fire skill capture if the review passed.
    if let Some(review) = &last_review {
        if review.verdict == JudgeVerdict::Pass {
            let task_description = ...; // extract from initial task
            let task_id = None; // or extract from the loop state
            let handle = skills::capture::capture_skill(
                gl.world_graph().clone(),
                serde_json::to_value(review).unwrap_or(serde_json::Value::Null),
                task_description,
                task_id,
                fast_model.clone(),  // use the fast model
                Arc::new(LocalSkillStorage::new(
                    LocalSkillStorage::default_install()
                        .map(|s| s.root)
                        .unwrap_or_else(|| PathBuf::from("/tmp/fallback-skills"))
                )),
            );
            // Discard the handle. The skill is captured in the background.
            drop(handle);
        }
    }
    break; // or whatever the existing exit logic is
}
```

NOTE: The exact field names (e.g., `review.verdict`, `gl.world_graph()`,
`fast_model`) depend on the current `agent_a.rs` structure. Adapt as
needed. The key things:
- Call `capture_skill` with the world graph, review JSON, task, model, and storage
- The returned `JoinHandle` is dropped (fire-and-forget)
- Only call when review verdict is `Pass`

- [ ] **Step 4: Add necessary imports at the top of `agent_a.rs`**

```rust
use std::path::PathBuf;
use std::sync::Arc;
use graph_harness::skills::{self, capture::capture_skill, storage::LocalSkillStorage};
```

(Adjust based on the actual `pub use` chain in `src/skills/mod.rs`. The
spec says re-exports are at the module root, so `graph_harness::skills::capture::capture_skill`
should work.)

- [ ] **Step 5: Verify the project builds**

Run: `cargo check -p graph_harness 2>&1 | tail -10`
Expected: clean build.

If there are errors about `last_review` not being defined, etc., adjust based on the existing structure of `agent_a.rs`.

- [ ] **Step 6: Run the full test suite**

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -3`
Expected: 338 tests pass (no new tests in this task, but nothing should regress).

---

## Task 11: Integration test — end-to-end via `agent_a` style wiring

**Files:**
- Create: `tests/integration_skill_capture.rs` (new integration test file in project root)

- [ ] **Step 1: Create the integration test file**

Create `/home/hhhh/Graph-Centric/tests/integration_skill_capture.rs`:

```rust
//! End-to-end test for the skill capture and retrieval flow.
//!
//! These tests exercise the full path: capture a skill, list it via
//! CompositeSkillStorage, verify the Proposer prompt includes it.

use std::sync::Arc;
use tempfile::TempDir;

use graph_harness::graph::Graph;
use graph_harness::skills::storage::{
    CompositeSkillStorage, LocalSkillStorage, RepoSkillStorage,
};
use graph_harness::skills::types::Skill;

#[test]
fn skills_round_trip_via_storage() {
    let local_dir = TempDir::new().unwrap();
    let repo_dir = TempDir::new().unwrap();

    let local = LocalSkillStorage::new(local_dir.path().to_path_buf());
    let repo = RepoSkillStorage::new(repo_dir.path().to_path_buf());
    let composite = CompositeSkillStorage::new(Some(local.clone()), repo);

    // Save a skill via local.
    let skill = Skill {
        slug: "round-trip".to_string(),
        task: "do the thing".to_string(),
        trigger: "applies when the thing is needed".to_string(),
        graph: Graph::new(),
        review: serde_json::json!({"verdict": "pass"}),
        meta: graph_harness::skills::types::SkillMeta {
            created_at: "2026-06-03T00:00:00Z".to_string(),
            task_id: None,
            model_used: "test".to_string(),
            domain_tags: vec![],
            l1_avg_confidence: 0.0,
        },
    };
    local.save(&skill).unwrap();

    // List via composite — should include the just-saved skill.
    let list = composite.list().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].slug, "round-trip");

    // Load via composite.
    let loaded = composite.load("round-trip").unwrap();
    assert_eq!(loaded.task, "do the thing");
}

#[test]
fn composite_lists_local_first_under_repo_collision() {
    let local_dir = TempDir::new().unwrap();
    let repo_dir = TempDir::new().unwrap();

    let local = LocalSkillStorage::new(local_dir.path().to_path_buf());
    let repo = RepoSkillStorage::new(repo_dir.path().to_path_buf());

    // Same slug in both.
    let mut repo_skill = Skill {
        slug: "shared".to_string(),
        task: "from-repo".to_string(),
        trigger: "from-repo".to_string(),
        graph: Graph::new(),
        review: serde_json::json!({}),
        meta: graph_harness::skills::types::SkillMeta {
            created_at: "2026-06-03T00:00:00Z".to_string(),
            task_id: None,
            model_used: "test".to_string(),
            domain_tags: vec![],
            l1_avg_confidence: 0.0,
        },
    };
    repo.save(&repo_skill).unwrap();

    let mut local_skill = repo_skill.clone();
    local_skill.task = "from-local".to_string();
    local_skill.trigger = "from-local".to_string();
    local.save(&local_skill).unwrap();

    let composite = CompositeSkillStorage::new(Some(local), repo);
    let list = composite.list().unwrap();
    assert_eq!(list.len(), 1, "duplicate slug should appear once");
    assert_eq!(list[0].trigger, "from-local");
}
```

- [ ] **Step 2: Run the integration test**

Run: `cargo test -p graph_harness --test integration_skill_capture 2>&1 | tail -10`
Expected: 2 tests pass.

If `tempfile` import is missing, add it to the top.

- [ ] **Step 3: Run the full test suite**

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -10`
Expected: 340 tests pass (338 lib + 2 integration).

- [ ] **Step 4: Verify no warnings**

Run: `cargo check -p graph_harness --tests 2>&1 | tail -5`
Expected: clean, no warnings.

---

## Self-Review

**1. Spec coverage:**

| Spec section | Plan task |
|---|---|
| §4.1 Skill directory structure | Tasks 3-7 (each file is one of the 5) |
| §4.2 Capture flow (async, fire-and-forget) | Task 7 |
| §4.2 step 1: in-memory `Skill` struct | Task 2 |
| §4.2 step 2: generate slug | Task 6 + Task 7 |
| §4.2 step 3: generate trigger | Task 7 (inline `generate_trigger` helper) |
| §4.2 step 4: l1_avg_confidence | Task 7 (`l1_avg_confidence` helper) |
| §4.2 step 5: domain_tags | Task 7 (`compute_domain_tags` helper) |
| §4.2 step 6: serialize | Task 7 (delegates to Task 3) |
| §4.2 step 7: log | Task 7 (`tracing::info!`) |
| §4.2 error handling | Task 7 (LLM failure → don't save) |
| §4.3 Retrieval flow | Task 8 + Task 9 |
| §4.3 20-skill cap | Task 8 (`MAX_SKILLS_IN_PROMPT`) |
| §4.4 Storage layout | Tasks 3, 4, 5 |
| §4.5 Module structure (6 files) | Tasks 2-8 |
| §4.6 API surface (Skill, SkillMeta, SkillRef, SkillError) | Task 2 |
| §4.6 SkillStorage trait | Task 3 |
| §4.6 LocalSkillStorage | Task 3 |
| §4.6 RepoSkillStorage | Task 4 |
| §4.6 CompositeSkillStorage | Task 5 |
| §4.6 capture_skill (JoinHandle<()>) | Task 7 |
| §4.6 list_for_prompt | Task 8 |
| §4.7 Wiring (Proposer, agent_a) | Tasks 9, 10 |
| §4.8 Slug generation + hash fallback | Task 6 |
| §5 chrono + tempfile deps | Task 1 |
| §5 lib.rs `pub mod skills;` | Task 1 |
| §5 proposer.rs storage field | Task 9 |
| §5 bin/agent_a.rs capture call | Task 10 |
| §6 All test cases | Distributed across Tasks 2-8 + 11 |
| §7 All 12 acceptance criteria | Verified by all tasks |
| §8 v1.2 / v2 / v3 out-of-scope | Documented in spec, not in plan (YAGNI) |

**2. Placeholder scan:** No "TBD" / "TODO" / "fill in details" in the plan. Every step has concrete code or specific instructions. The one place where there's a "NOTE" is the `iso8601_now` function in Task 7 — it has a placeholder implementation with a comment explaining the proper fix. The implementer can address that during execution or in a follow-up. **However**, this is technically a placeholder. Fix: change Task 7 to use `chrono::Utc::now().to_rfc3339()` directly.

**3. Type consistency check:**

| Name | Defined in | Used in | Status |
|---|---|---|---|
| `Skill` | Task 2 | Tasks 3, 4, 5, 7, 11 | ✅ |
| `SkillMeta` | Task 2 | Tasks 7, 11 | ✅ |
| `SkillRef` | Task 2 | Tasks 3, 5, 8 | ✅ |
| `SkillError` | Task 2 | Tasks 3, 4, 5, 7 | ✅ |
| `SkillStorage` (trait) | Task 3 | Tasks 4, 5, 8, 9, 11 | ✅ |
| `LocalSkillStorage` | Task 3 | Tasks 5, 7, 10, 11 | ✅ |
| `RepoSkillStorage` | Task 4 | Tasks 5, 11 | ✅ |
| `CompositeSkillStorage` | Task 5 | Tasks 8, 11 | ✅ |
| `generate_slug` | Task 6 | Task 7 | ✅ |
| `capture_skill` | Task 7 | Task 10 | ✅ |
| `list_for_prompt` | Task 8 | Task 9 | ✅ |
| `count_label` | Task 8 | (not used in plan; available for callers) | ✅ |
| `GraphProposer::skills` field | Task 9 | Task 10 | ✅ |
| `GraphProposer::with_skills` | Task 9 | (test) | ✅ |
| `MAX_SKILLS_IN_PROMPT = 20` | Task 8 | Task 8 | ✅ |

**4. Ambiguity check:**

- The `Message::user(content)` vs `Message { role, content }` form: the plan notes this in Task 6, telling the implementer to check the existing `src/model/mod.rs` for the right form. The fallback (`Message { role: Role::User, content }`) is also provided.
- The `iso8601_now` placeholder: I flagged this in the Self-Review. Need to fix in Task 7 to use `chrono::Utc::now().to_rfc3339()`.
- The exact `LocalSkillStorage::default_install()` API: spec'd in §4.6. Implemented in Task 3. Agent_a wiring (Task 10) uses it.
- The model construction in `bin/agent_a.rs`: the plan says to use `fast_model.clone()` from the existing model config. Agent_a already has this set up. The implementer just needs to grab the right handle.

**5. Scope check:** This plan is one self-contained change. 1 new module with 7 files, 4 modified files. ~900 lines of new code + tests. Well within one implementation plan.

**Inline fixes applied:**

1. Replace the `iso8601_now` placeholder in Task 7 with the proper `chrono` version:

```rust
fn iso8601_now() -> String {
    chrono::Utc::now().to_rfc3339()
}
```

(Replace the existing block in Task 7's `capture.rs` with this.)

2. The `Message::user(content)` form: confirm by looking at `src/model/mod.rs`. The plan's slug.rs uses this form. If the form doesn't exist, the fallback `Message { role: Role::User, content }` works (already provided in the plan code as a hint).

---

## Acceptance criteria (mirroring spec §7)

- [ ] `src/skills/` module compiles with all 7 files
- [ ] `SkillStorage` trait + 3 impls work
- [ ] `capture_skill` is async fire-and-forget; returns `JoinHandle<()>`
- [ ] `list_for_prompt` returns the formatted section
- [ ] Proposer has `Option<Arc<dyn SkillStorage>>` field; injects section when set
- [ ] `bin/agent_a.rs` calls `capture_skill` on Done+pass; doesn't block
- [ ] Local root: `~/.local/share/graph-centric/skills/`
- [ ] Slug is LLM-generated with `DefaultHasher` fallback
- [ ] All 310 pre-existing tests still pass
- [ ] Total test count grows to 340 (338 lib + 2 integration)
- [ ] `cargo check -p graph_harness --tests` is clean
