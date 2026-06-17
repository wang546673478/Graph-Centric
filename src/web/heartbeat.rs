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
\n## 每轮工作流 (A→D)\
\n1. 创建 A(当前问题)和 D(优化目标),比如 A=\"graph_loop.rs:1655 处 unwrap 会 panic\" D=\"用 proper error propagation 替换\"\
\n2. Explore 扫描 src/ 或 webui/src/ 找出具体问题点,用节点 metadata 的 quality_score/unwrap_count 定位\
\n3. 如果本轮需要外部参考: Explore + web_search 搜索 GitHub 找到具体实现模式\
\n4. ProposePatch: 仅修改1-3个相关文件,用 DependsOn 建中间节点\
\n5. SubAgent执行修改(自动git commit+cargo check验证,编译失败自动回退)\
\n6. Review通过→本轮完成→自动编译重启进入下一轮\
\n\
\n## 约束\
\n- 每轮只改1-3个文件，必须编译通过\
\n- 禁止引入新unwrap/unsafe，禁止删除测试\
\n- 不改graph/mod.rs和graph/l1.rs(核心图结构)\
\n- 外部项目只参考设计模式，不照搬代码\
\n- 前端改动不引入新依赖(保持轻量)\
\n- 第10轮结束自动停止";

impl HeartBeat {
    pub fn load() -> Option<Self> {
        let path = PathBuf::from(STATE_FILE);
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(json) => match serde_json::from_str::<HeartBeat>(&json) {
                    Ok(hb) => {
                        if hb.active { return Some(hb); }
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
        };
        hb.save();
        hb
    }

    /// Mark one round complete. Returns true if more rounds remain.
    pub fn round_complete(&mut self) -> bool {
        self.completed_rounds += 1;
        self.current_run_id = None;
        if self.completed_rounds >= self.max_rounds {
            self.active = false;
            self.save();
            info!("heartbeat: all {} rounds complete; deactivating", self.max_rounds);
            false
        } else {
            self.save();
            info!(
                "heartbeat: round {}/{} complete; {} remaining",
                self.completed_rounds, self.max_rounds,
                self.max_rounds - self.completed_rounds
            );
            true
        }
    }

    /// Deactivate and clean up.
    pub fn cancel(&mut self) {
        self.active = false;
        self.current_run_id = None;
        self.save();
    }
}
