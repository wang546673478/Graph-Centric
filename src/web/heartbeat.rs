//! HeartBeat — self-improving agent loop across process lifetimes.
//!
//! Stores a persistent task + round counter. On server startup, if an
//! active heartbeat is configured, creates a new run with the stored
//! prompt. After each successful Review round, increments the counter,
//! commits progress, and exits cleanly so the external launcher can
//! restart with the latest binary.
//!
//! State file: `.graph_harness_heartbeat.json`
//!
//! ## Default optimization prompt (10-round self-improvement)
//!
//! ```text
//! 对 Graph-Centric Agent 进行10轮自我优化。每轮选一个具体优化点，改完通过编译后自动重启进入下一轮。
//!
//! ## 每轮工作流 (A→D)
//! 1. 创建 A(当前状态) 和 D(优化目标)，比如 A="graph_loop.rs unwrap 密度过高" D="用 Result 传播替代 .unwrap()"
//! 2. Explore 子代理扫描 src/ 内部(质量指标已在节点 metadata: loc/unwrap_count/unsafe_count/todo_count/quality_score)，找出 ⚠️ 标记的节点
//! 3. 可选: Explore 子代理搜索外部优秀项目(openclaw/opencode/CodeWhale)的对应模块实现模式
//! 4. ProposePatch: 仅修改 1-3 个相关文件
//! 5. SubAgent 执行修改(自动 git branch + commit + cargo check 验证)
//! 6. Review 通过 → 本轮完成 → 自动编译重启进入下一轮
//!
//! ## 约束
//! - 图上永远保持 A(锚点) 和 D(本轮目标) 两个节点，中间节点逐步填充
//! - 每轮只修改 1-3 个文件，改完必须 cargo check 通过
//! - 禁止引入新的 unwrap/unsafe，禁止删除现有测试
//! - 不改 graph/mod.rs 和 graph/l1.rs(核心图结构不可变)
//! - 外部项目只是参考模式，不能照搬代码
//! - 如果没有通过编译就不算完成本轮
//! - 第10轮结束后自动停止
//! ```

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{info, warn};

const STATE_FILE: &str = ".graph_harness_heartbeat.json";
const PROMPT_FILE: &str = ".graph_harness_heartbeat_prompt.md";

/// v2 spec §5.5: how a single round ended. Used by the
/// dashboard to bucket the failure-mode count and to pick
/// the prompt hint for the next round ("stagnation" gets a
/// "try a different angle" hint, "cycle" gets a "break the
/// cycle" hint, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundOutcome {
    /// Round succeeded; ready for the next.
    Success,
    /// Stagnation — graph didn't change for N rounds.
    Stagnation,
    /// Cycle detected in the task DAG.
    Cycle,
    /// Sub-task failed; recoverable, try the next round.
    SubTaskFailed,
    /// Run was canceled or errored for an unknown reason.
    Error,
    /// Round is still in progress.
    InProgress,
}

impl RoundOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Stagnation => "stagnation",
            Self::Cycle => "cycle",
            Self::SubTaskFailed => "sub_task_failed",
            Self::Error => "error",
            Self::InProgress => "in_progress",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartBeat {
    /// The driving prompt for the optimization loop.
    pub prompt: String,
    /// Total rounds to run.
    pub max_rounds: usize,
    /// Rounds completed so far.
    pub completed_rounds: usize,
    /// Whether heartbeat is active.
    pub active: bool,
    /// The current run ID (set after creating a run).
    pub current_run_id: Option<String>,
    /// v2 spec §5.5: how many rounds ended in each outcome.
    /// `success` + (stagnation + cycle + sub_task_failed + error)
    /// == `completed_rounds`.
    #[serde(default)]
    pub outcome_counts: OutcomeCounts,
    /// v2 spec §5.5: history of the last N rounds. Powers the
    /// dashboard's "what happened in the last 5 rounds?" panel.
    #[serde(default)]
    pub recent_rounds: Vec<RoundRecord>,
    /// v2 spec §5.5: when this heartbeat was started (unix ms).
    #[serde(default)]
    pub started_at_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutcomeCounts {
    pub success: u32,
    pub stagnation: u32,
    pub cycle: u32,
    pub sub_task_failed: u32,
    pub error: u32,
}

impl OutcomeCounts {
    pub fn total(&self) -> u32 {
        self.success + self.stagnation + self.cycle + self.sub_task_failed + self.error
    }
    pub fn success_rate(&self) -> f64 {
        let t = self.total();
        if t == 0 {
            0.0
        } else {
            self.success as f64 / t as f64
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundRecord {
    pub round: u32,
    pub outcome: RoundOutcome,
    pub run_id: Option<String>,
    pub duration_ms: u64,
    pub note: String,
    pub at_ms: u64,
}

/// Default 10-round self-optimization prompt.
pub const DEFAULT_OPTIMIZATION_PROMPT: &str = "\
对 Graph-Centric Agent 进行10轮自我优化。每轮选一个具体优化点，改完通过编译后自动重启进入下一轮。\
\n优化范围包括后端Rust代码(src/)和前端Vue3界面(webui/src/)。\
\n\
\n## 优化方向\
\n### 后端 (src/)\
\n- 降低unwrap/unsafe密度，提升代码健壮性\
\n- 优化模块边界，减少大文件(>500行)\
\n- 改善错误处理，用结构化错误替代字符串\
\n- 参考 openclaw/opencode/CodeWhale 中的模式但不照搬\
\n\
\n### 前端 (webui/src/)\
\n- 参考 GitHub 上优秀AI agent项目的Web界面设计\
\n- 优化现有Vue3组件的排版、配色、交互体验\
\n- 改进对话区域的视觉层次感和可读性\
\n- 增强3D关系图面板的可用性(标签、动画、布局)\
\n- 设置页面和信息页面的信息架构优化\
\n\
\n## 搜索外部项目 (Explore + WebSearch)\
\n- 用 Explore 派子代理去 GitHub 搜索关键词,如: \"openclaw agent harness architecture\" \"CodeWhale persistence implementation\" \"OpenCode UI design patterns\" \"CrewAI web interface\" \"Langflow frontend\"\
\n- 子代理有 web_search 工具可直接搜索 GitHub,然后用 bash cat/grep 读返回的代码文件\
\n- 搜索目标: 找到具体实现模式(不是概念描述),带回文件路径和代码片段\
\n\
\n## 子任务角色注入 (role_prompt)\
\n- 对代码编辑类子任务,设置 `role_prompt`: \"你是代码编辑专家。用 read_file 读代码, edit_file 替换, write_file 创建文件。每步必须产生实际文件更改,禁止只分析不修改。\"\
\n- 对探索调研类子任务,设置 `role_prompt`: \"你是探索专家。用 read_file 和 web_search 全面了解目标,产出带文件路径的报告。\"\
\n\
\n## 每轮工作流 (A→D)\
\n1. 创建 A(当前问题)和 D(优化目标),比如 A=\"graph_loop.rs:1655 处 unwrap 会 panic\" D=\"用 proper error propagation 替换\"\
\n2. Explore 扫描 src/ 或 webui/src/ 找出具体问题点,用节点 metadata 的 quality_score/unwrap_count 定位\
\n3. 如果本轮需要外部参考: Explore + web_search 搜索 GitHub 找到具体实现模式\
\n4. ProposePatch: 仅修改1-3个相关文件,用 DependsOn 建中间节点\
\n5. SubAgent执行修改(自动git commit+cargo check验证,编译失败自动回退)\
\n6. Review通过→本轮完成→自动编译重启进入下一轮\
\n\
\n## 语言\n- 所有输出(分析/提问/回答)必须使用中文\
\n\
\n## 重要: 不要提问，直接执行！\
\n- 这是无人值守自动化循环，任何问题都会自动回复\"yes, proceed\"\
\n- 禁止使用 ask_user 或 block 步骤\
\n- 直接 Explore→ProposePatch→SubAgent 执行，跳过确认\
\n- 不确定就选最合理方案直接执行\
\n\
\n## 约束\
\n- 每轮只改1-3个文件，必须编译通过\
\n- 禁止引入新unwrap/unsafe，禁止删除测试\
\n- 不改graph/mod.rs和graph/l1.rs(核心图结构)\
\n- 外部项目只参考设计模式，不照搬代码\
\n- 前端改动不引入新依赖(保持轻量)\
\n- 第10轮结束自动停止";

impl HeartBeat {
    fn prompt_from_file() -> Option<String> {
        std::fs::read_to_string(PROMPT_FILE)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Keep the persisted heartbeat aligned with the repository prompt file.
    /// This lets the self-optimization task evolve through normal file edits
    /// instead of being trapped in an older JSON state snapshot.
    pub fn sync_prompt_from_file(&mut self) -> bool {
        if let Some(prompt) = Self::prompt_from_file() {
            if self.prompt != prompt {
                self.prompt = prompt;
                return true;
            }
        }
        false
    }

    pub fn load() -> Option<Self> {
        let path = PathBuf::from(STATE_FILE);
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(json) => match serde_json::from_str::<HeartBeat>(&json) {
                    Ok(mut hb) => {
                        if hb.active {
                            if hb.sync_prompt_from_file() {
                                hb.save();
                            }
                            return Some(hb);
                        }
                    }
                    Err(e) => warn!(error = %e, "heartbeat: corrupt state, skipping"),
                },
                Err(e) => warn!(error = %e, "heartbeat: cannot read state"),
            }
        }
        None
    }

    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(STATE_FILE, json);
        }
    }

    /// Set a new heartbeat task.
    pub fn start(prompt: String, max_rounds: usize) -> Self {
        let hb = HeartBeat {
            prompt,
            max_rounds,
            completed_rounds: 0,
            active: true,
            current_run_id: None,
            outcome_counts: OutcomeCounts::default(),
            recent_rounds: Vec::new(),
            started_at_ms: now_ms(),
        };
        hb.save();
        hb
    }

    /// Mark one round complete with an explicit outcome. Returns
    /// true if more rounds remain. v2 spec §5.5: drives the
    /// dashboard's per-outcome counts + the prompt hint selection
    /// for the next round.
    pub fn round_complete(&mut self, outcome: RoundOutcome, run_id: Option<String>, note: String) -> bool {
        self.completed_rounds += 1;
        let prev_run = self.current_run_id.clone().or(run_id.clone());
        let record = RoundRecord {
            round: self.completed_rounds as u32,
            outcome,
            run_id: prev_run,
            duration_ms: 0, // duration is recorded by the caller (api_runs) — heart-beat doesn't time it
            note,
            at_ms: now_ms(),
        };
        match outcome {
            RoundOutcome::Success => self.outcome_counts.success += 1,
            RoundOutcome::Stagnation => self.outcome_counts.stagnation += 1,
            RoundOutcome::Cycle => self.outcome_counts.cycle += 1,
            RoundOutcome::SubTaskFailed => self.outcome_counts.sub_task_failed += 1,
            RoundOutcome::Error => self.outcome_counts.error += 1,
            RoundOutcome::InProgress => {}
        }
        self.recent_rounds.push(record);
        // Keep at most the last 50 rounds in memory.
        if self.recent_rounds.len() > 50 {
            let drop = self.recent_rounds.len() - 50;
            self.recent_rounds.drain(0..drop);
        }
        self.current_run_id = None;
        if self.completed_rounds >= self.max_rounds {
            self.active = false;
            self.save();
            info!("heartbeat: all {} rounds complete; deactivating", self.max_rounds);
            false
        } else {
            self.save();
            info!(
                "heartbeat: round {}/{} complete (outcome={:?}); {} remaining",
                self.completed_rounds, self.max_rounds, outcome,
                self.max_rounds - self.completed_rounds
            );
            true
        }
    }

    /// v2 spec §5.5: select a prompt hint for the next round
    /// based on the most recent outcome. Returns a short hint
    /// string the caller can inject into the optimization prompt.
    /// Returns `None` when no hint is needed.
    pub fn next_round_hint(&self) -> Option<String> {
        let last = self.recent_rounds.last()?;
        let hint = match last.outcome {
            RoundOutcome::Success => return None,
            RoundOutcome::Stagnation => {
                "⚠️ 上轮 stagnation(模型没让图变化)。本轮换个优化点,先 Explore 再 ProposePatch。"
            }
            RoundOutcome::Cycle => "🔁 上轮 cycle(检测到依赖环)。本轮必须引入新节点打破 cycle,或换目标文件。",
            RoundOutcome::SubTaskFailed => "🛠 上轮子任务失败。本轮先查 dispatch 错误日志,再决定继续或重试。",
            RoundOutcome::Error => "❌ 上轮 Error。本轮调小范围(只改 1 个文件),先验证 cargo build。",
            RoundOutcome::InProgress => return None,
        };
        Some(hint.to_string())
    }

    /// v2 spec §5.5: human-in-the-loop override. Allows the
    /// operator to inject a hint into the current round's
    /// prompt without canceling the loop. Returns true on
    /// success.
    pub fn inject_hint(&mut self, hint: String) {
        if !hint.trim().is_empty() {
            self.prompt.push_str(&format!("\n\n## 人工注入提示 ({})\n{}\n", now_ms(), hint));
            self.save();
        }
    }

    /// Deactivate and clean up.
    pub fn cancel(&mut self) {
        self.active = false;
        self.current_run_id = None;
        self.save();
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
