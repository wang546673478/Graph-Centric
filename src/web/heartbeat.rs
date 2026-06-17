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
\n\
\n## 每轮工作流 (A→D)\
\n1. 创建 A(当前问题)和 D(优化目标)，例如 A=\"graph_loop.rs unwrap密度过高\" D=\"用Result传播替代.unwrap()\"\
\n2. Explore 扫描 src/ 找出 ⚠️ 质量标记的节点(节点metadata含loc/unwrap_count/unsafe_count/quality_score)\
\n3. 可选: Explore 搜索外部优秀项目(openclaw/opencode/CodeWhale)的对应模块实现\
\n4. ProposePatch: 仅修改1-3个相关文件，按DependsOn建中间节点\
\n5. SubAgent执行修改(自动git branch+commit+cargo check验证)\
\n6. Review通过→本轮完成→自动编译重启进入下一轮\
\n\
\n## 约束\
\n- 每轮只改1-3个文件，必须cargo check通过\
\n- 禁止引入新unwrap/unsafe，禁止删除测试\
\n- 不改graph/mod.rs和graph/l1.rs(核心图结构)\
\n- 外部项目参考模式不照搬代码\
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
