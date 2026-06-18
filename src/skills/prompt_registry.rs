//! Prompt Registry — dynamic, composable prompt blocks.
//!
//! Inspired by Claude Code's `SystemPromptSection` architecture:
//! - Named blocks with lazy `compute()` functions
//! - Cacheable (computed once) vs volatile (every turn)
//! - Conditional: return `None` to skip a block
//! - Ordered by priority, resolved in parallel

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// PromptContext — what each block can inspect to decide its output.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct PromptContext {
    /// e.g., "edit", "explore", "auto"
    pub role: String,
    /// Task description for context-aware prompts.
    pub task_description: String,
    /// Language preference (e.g., "Chinese", "English").
    pub language: Option<String>,
    /// Whether this is a self-improvement (heartbeat) run.
    pub is_heartbeat: bool,
    /// OS name for platform-specific instructions.
    pub platform: String,
    /// Formatted list of auto-matched skills (one "slug: trigger" per line).
    /// When non-empty, the `builtin/skill-matching` block activates.
    pub matched_skills: String,
    /// Whether auto-skill-matching is enabled.
    pub auto_apply_skills: bool,
    /// Extra context key-values.
    pub extra: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// PromptBlock trait
// ---------------------------------------------------------------------------

/// A prompt block that can produce text (or `None` to skip).
pub trait PromptBlock: Send + Sync {
    fn name(&self) -> &str;
    fn compute(&self, ctx: &PromptContext) -> Option<String>;
    /// If true, this block is recomputed every turn (never cached).
    fn is_volatile(&self) -> bool { false }
}

// ---------------------------------------------------------------------------
// Static file block
// ---------------------------------------------------------------------------

struct StaticBlock {
    name: String,
    content: String,
}

impl PromptBlock for StaticBlock {
    fn name(&self) -> &str { &self.name }
    fn compute(&self, _ctx: &PromptContext) -> Option<String> {
        if self.content.is_empty() { None } else { Some(self.content.clone()) }
    }
}

// ---------------------------------------------------------------------------
// Dynamic function block
// ---------------------------------------------------------------------------

type ComputeFn = Arc<dyn Fn(&PromptContext) -> Option<String> + Send + Sync>;

struct DynamicBlock {
    name: String,
    compute_fn: ComputeFn,
    volatile: bool,
}

impl PromptBlock for DynamicBlock {
    fn name(&self) -> &str { &self.name }
    fn compute(&self, ctx: &PromptContext) -> Option<String> {
        (self.compute_fn)(ctx)
    }
    fn is_volatile(&self) -> bool { self.volatile }
}

// ---------------------------------------------------------------------------
// PromptRegistry
// ---------------------------------------------------------------------------

pub struct PromptRegistry {
    blocks: HashMap<String, Arc<dyn PromptBlock>>,
}

impl PromptRegistry {
    /// Create registry with built-in dynamic blocks + optional static files.
    pub fn new(root: Option<&Path>) -> Self {
        let mut reg = Self { blocks: HashMap::new() };

        // ---- Built-in dynamic blocks (from Claude Code patterns) ----

        // Language section
        reg.add_dynamic("builtin/language", false, Arc::new(|ctx: &PromptContext| {
            match &ctx.language {
                Some(lang) if !lang.is_empty() => Some(format!(
                    "# Language\nAlways respond in {lang}. Use {lang} for all explanations, comments, and communications."
                )),
                _ => None,
            }
        }));

        // Platform section
        reg.add_dynamic("builtin/platform", false, Arc::new(|ctx: &PromptContext| {
            let platform = &ctx.platform;
            if platform.is_empty() { return None; }
            let note = if platform.contains("windows") {
                "This agent runs on Windows. Use `cmd /c` for shell commands. `sed`, `grep`, `cat` do not exist; use `read_file` and `edit_file` instead."
            } else {
                "This agent runs on Unix. Use `bash -c` for shell commands."
            };
            Some(format!("# Platform\n{note}"))
        }));

        // Heartbeat mode section
        reg.add_dynamic("builtin/heartbeat", true, Arc::new(|ctx: &PromptContext| {
            if ctx.is_heartbeat {
                Some("# Autonomous Mode\n\
This is an unattended automation loop. Do NOT ask questions — any question \
will be auto-answered. Directly execute: Explore → ProposePatch → SubAgent → Verify.".into())
            } else { None }
        }));

        // Code editor role
        reg.add_dynamic("builtin/role-edit", false, Arc::new(|ctx: &PromptContext| {
            if ctx.role == "edit" || ctx.role.contains("code") || ctx.role.contains("edit") {
                Some(format!(
                    "## Role: Code Editor\n\
You are a code modification specialist.\n\
Task: {}\n\
**RULES:**\n\
- Use `read_file` to read, `edit_file` to replace, `write_file` to create.\n\
- Every call MUST produce an actual file change. Do NOT just analyze.\n\
- After editing, run `cargo check --lib` to verify.\n\
- Do NOT use bash for file I/O — use the dedicated file tools.",
                    ctx.task_description
                ))
            } else { None }
        }));

        // Explorer role
        reg.add_dynamic("builtin/role-explore", false, Arc::new(|ctx: &PromptContext| {
            if ctx.role == "explore" || ctx.role.contains("explore") {
                Some(format!(
                    "## Role: Explorer\n\
Task: {}\n\
Use `read_file` and `bash ls/find` to discover structure, patterns, and key files.\n\
Produce a report with file paths, functions/classes, and relationships.",
                    ctx.task_description
                ))
            } else { None }
        }));

        // Tool strategy
        reg.add_dynamic("builtin/tool-strategy", false, Arc::new(|ctx: &PromptContext| {
            Some(r#"## Available Tools
- `read_file(path, offset?, limit?)` — read any file
- `edit_file(path, old_string, new_string)` — replace a unique string
- `write_file(path, content)` — create or overwrite a file
- `bash` — run shell commands (for `cargo check`, `ls`, `find`, `grep`)
- `web_search(query)` — search the web
- `web_fetch(url)` — fetch a URL"#.into())
        }));

        // Skill matching section (volatile — task changes every turn).
        reg.add_dynamic("builtin/skill-matching", true, Arc::new(|ctx: &PromptContext| {
            if !ctx.auto_apply_skills || ctx.matched_skills.is_empty() {
                return None;
            }
            Some(format!(
                "# Auto-Matched Skills\n\
The following skills from past successful runs were matched to your current task:\n\
{}\n\
These skills have been automatically applied. Their compiled task graphs have \
been injected into the task plan as `skill:<slug>:<node-id>` nodes. These nodes \
behave like regular Task nodes — you may add edges to/from them or re-plan them \
if needed.",
                ctx.matched_skills
            ))
        }));

        // Load static .md files from disk if root provided.
        if let Some(root) = root {
            let prompts_dir = root.join("skills").join("prompts");
            if prompts_dir.exists() {
                reg.scan_dir(&prompts_dir, "");
            }
        }

        tracing::info!(count = reg.blocks.len(), "prompt registry initialized");
        reg
    }

    fn add_dynamic<N: Into<String>>(&mut self, name: N, volatile: bool, f: ComputeFn) {
        let name = name.into();
        self.blocks.insert(name.clone(), Arc::new(DynamicBlock { name, compute_fn: f, volatile }));
    }

    fn scan_dir(&mut self, dir: &Path, prefix: &str) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let n = entry.file_name().to_string_lossy().into_owned();
                    let np = if prefix.is_empty() { n } else { format!("{prefix}/{n}") };
                    self.scan_dir(&path, &np);
                } else if path.extension().map_or(false, |e| e == "md") {
                    let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
                    let name = if prefix.is_empty() { stem } else { format!("{prefix}/{stem}") };
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        self.blocks.insert(name.clone(), Arc::new(StaticBlock { name, content }));
                    }
                }
            }
        }
    }

    /// Compose a prompt from named blocks + role-based defaults.
    /// Blocks returning `None` are skipped. Volatile blocks always recompute;
    /// static blocks are cached after first compute.
    pub fn compose(&self, block_names: &[&str], ctx: &PromptContext) -> String {
        let mut parts = Vec::new();

        // Always include tool strategy.
        if let Some(b) = self.blocks.get("builtin/tool-strategy") {
            if let Some(text) = b.compute(ctx) { parts.push(text); }
        }

        // Role block: match "edit", "explore", or generic.
        let role_block = if ctx.role.contains("edit") || ctx.role.contains("code") {
            "builtin/role-edit"
        } else if ctx.role.contains("explore") {
            "builtin/role-explore"
        } else {
            ""
        };
        if !role_block.is_empty() {
            if let Some(b) = self.blocks.get(role_block) {
                if let Some(text) = b.compute(ctx) { parts.push(text); }
            }
        }

        // Platform block.
        if let Some(b) = self.blocks.get("builtin/platform") {
            if let Some(text) = b.compute(ctx) { parts.push(text); }
        }

        // Language block.
        if let Some(b) = self.blocks.get("builtin/language") {
            if let Some(text) = b.compute(ctx) { parts.push(text); }
        }

        // Heartbeat block (volatile).
        if let Some(b) = self.blocks.get("builtin/heartbeat") {
            if let Some(text) = b.compute(ctx) { parts.push(text); }
        }

        // Skill matching block (volatile — only when auto_apply + matches exist).
        if let Some(b) = self.blocks.get("builtin/skill-matching") {
            if let Some(text) = b.compute(ctx) { parts.push(text); }
        }

        // Named blocks from the task.
        for name in block_names {
            if name.is_empty() { continue; }
            if let Some(b) = self.blocks.get(*name) {
                if let Some(text) = b.compute(ctx) { parts.push(text); }
            }
        }

        parts.join("\n\n")
    }

    /// Quick compose with role shortcut.
    pub fn compose_role(&self, role: &str, task_desc: &str, is_hb: bool) -> String {
        let ctx = PromptContext {
            role: role.to_string(),
            task_description: task_desc.to_string(),
            language: Some("Chinese".into()),
            is_heartbeat: is_hb,
            platform: if cfg!(target_os = "windows") { "windows".into() } else { "linux".into() },
            ..Default::default()
        };
        self.compose(&[], &ctx)
    }
}
