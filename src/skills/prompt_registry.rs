//! Prompt Registry — modular, composable prompt blocks.
//!
//! Each prompt block is a `.md` file under `skills/prompts/` organized by
//! category (base/, tools/, constraints/). Blocks are loaded on demand and
//! composed by category into the final system prompt.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A loaded prompt block with its category tag.
#[derive(Debug, Clone)]
pub struct PromptBlock {
    pub name: String,
    pub content: String,
}

/// Registry of prompt blocks organized by category.
#[derive(Debug, Clone, Default)]
pub struct PromptRegistry {
    blocks: HashMap<String, PromptBlock>,
}

impl PromptRegistry {
    /// Load all `.md` files under `skills/prompts/` recursively.
    pub fn load(root: &Path) -> Self {
        let prompts_dir = root.join("skills").join("prompts");
        let mut reg = Self::default();
        if !prompts_dir.exists() { return reg; }
        reg.scan_dir(&prompts_dir, "");
        tracing::info!(count = reg.blocks.len(), "prompt registry loaded");
        reg
    }

    fn scan_dir(&mut self, dir: &Path, prefix: &str) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let new_prefix = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
                    self.scan_dir(&path, &new_prefix);
                } else if path.extension().map_or(false, |e| e == "md") {
                    let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
                    let name = if prefix.is_empty() { stem } else { format!("{prefix}/{stem}") };
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        self.blocks.insert(name.clone(), PromptBlock { name, content });
                    }
                }
            }
        }
    }

    /// Get a single block by name (e.g., "tools/edit-strategy").
    pub fn get(&self, name: &str) -> Option<&PromptBlock> {
        self.blocks.get(name)
    }

    /// Compose a prompt from a list of block names. Empty names are skipped.
    pub fn compose(&self, names: &[&str]) -> String {
        let mut parts = Vec::new();
        for name in names {
            if name.is_empty() { continue; }
            if let Some(block) = self.blocks.get(*name) {
                parts.push(block.content.clone());
            }
        }
        parts.join("\n\n")
    }

    /// Compose with default blocks for a given role.
    /// Role shortcuts: "edit" = base/core + tools/edit-strategy + constraints/windows-safety
    ///               "explore" = base/core + tools/edit-strategy
    ///               "auto" = base/core + tools/edit-strategy + constraints/no-questions
    pub fn compose_role(&self, role: &str) -> String {
        let defaults: &[&str] = match role {
            "edit" => &["base/subagent-core", "base/subagent-edit", "tools/edit-strategy", "constraints/windows-safety"],
            "explore" => &["base/subagent-core", "base/subagent-explore", "tools/edit-strategy"],
            "auto" => &["base/subagent-core", "base/subagent-edit", "tools/edit-strategy", "constraints/windows-safety"],
            _ => &["base/subagent-core"],
        };
        self.compose(defaults)
    }
}
