//! Format available skills as a markdown section for the Proposer's
//! system prompt, and load skills matched by the token-based matcher.

use super::matcher::find_matching_skills;
use super::storage::SkillStorage;
use super::types::Skill;
use crate::error::HarnessError;

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

/// Find matching skills and load their full `Skill` objects.
///
/// Calls `find_matching_skills` (default config) and then `storage.load()`
/// for each match up to `max`. Load failures are traced and skipped
/// (not surfaced as errors — matching is best-effort).
pub fn find_and_load_matching_skills(
    task: &str,
    storage: &dyn SkillStorage,
    threshold: f64,
    max: usize,
) -> std::result::Result<Vec<Skill>, HarnessError> {
    let matches = find_matching_skills(task, storage, threshold)
        .map_err(|e| HarnessError::model(format!("skill matching failed: {e}")))?;
    let mut loaded = Vec::new();
    for (ref_, _score) in matches.iter().take(max) {
        match storage.load(&ref_.slug) {
            Ok(skill) => loaded.push(skill),
            Err(e) => {
                tracing::warn!(
                    slug = %ref_.slug,
                    error = %e,
                    "failed to load matched skill, skipping"
                );
            }
        }
    }
    Ok(loaded)
}

/// Config-aware variant of [`find_and_load_matching_skills`].
pub fn find_and_load_matching_skills_with(
    task: &str,
    storage: &dyn SkillStorage,
    matcher_cfg: &super::matcher::SkillMatchConfig,
    max: usize,
) -> std::result::Result<Vec<Skill>, HarnessError> {
    use super::matcher::find_matching_skills_with;
    let matches = find_matching_skills_with(task, storage, matcher_cfg, matcher_cfg.threshold)
        .map_err(|e| HarnessError::model(format!("skill matching failed: {e}")))?;
    let mut loaded = Vec::new();
    for (ref_, _score) in matches.iter().take(max) {
        match storage.load(&ref_.slug) {
            Ok(skill) => loaded.push(skill),
            Err(e) => {
                tracing::warn!(
                    slug = %ref_.slug,
                    error = %e,
                    "failed to load matched skill, skipping"
                );
            }
        }
    }
    Ok(loaded)
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
