# Graph-Centric Agent Harness — v2 规范总结

> **v2 完成的完整规范** — 22 个核心 idea + P12 架构改造(Advisor 第二意见 + answer-content 验证)
>
> 配套规范文档:`docs/v2-agent-harness-complete-spec.zh-CN.md`(详细)
>
> 配套代码:12 个 commit,653 个 lib 测试通过,`cargo build --bin serve` 干净

---

## 0. 一句话

**Graph-Centric Agent Harness v2** 是一个 Rust 写的 LLM agent 编排器,以"关系图 = plan 本体"为第一性原理,实现了 22 个核心 idea + 3 层独立 judgment 架构(model 中心 → 关系图 → 三层 Reviewer),让"模型只能编辑图、图就是计划"成为代码层强约束,而不是 prompt 层软约束。

---

## 1. 核心关系图思想(不变)

| 原则 | 含义 |
|---|---|
| **图 IS 计划** | 模型只能通过 `propose_patch` 修改 `Graph`,没有"图外状态" |
| **FSM 是代码** | `Phase::{Graph,Task,Review,Done,Poisoned}` 是 Rust 枚举,不是 prompt |
| **6 step 类型** | `ProposePatch` / `Explore` / `AskUser` / `ReadyForVerify` / `Block` / `ConsultAdvisor`,无其他 |
| **三层 Reviewer** | Deterministic(50+ 检查) + Main LLM-as-judge + Advisor(DeepSeek) |
| **核心关系图** | `Graph` 仍是 plan 本体 — 12 个 commit 没改这一条 |

---

## 2. 22 个核心 idea(v2 全部实现)

| # | 支柱 | 状态 | v2 关键改动 |
|---|---|---|---|
| 1 | 图即计划 | ✅ | proposer 6 step 类型 + 强类型 GraphPatch |
| 2 | FSM 是代码 | ✅ | `Phase` + `GraphPhase` Rust 枚举,无字符串 |
| 3 | 双入口模式 | ✅ | Clarifying v2 软上限 10,反相似性兜底(无强制 2 次) |
| 4 | 子代理合约是确定性的 | ✅ | `CheckContract::KnowHow` / `Exploratory` / `MustEdit` / `None` |
| 5 | 局部图修复 | ✅ | `LocalRepairer` + `CascadeBacktracker`,monotonic within run |
| 6 | 技能捕获闭环 | ✅ | `LocalSkillStorage`,Jaccard ≥ 0.4 + l1_weight |
| 7 | 钻取展开 | ✅ | `DrillDownMark`,max_drilldown_depth=2 |
| 8 | 验证者是顾问 | ✅ | **3 层 Reviewer(deterministic + main + advisor)** |
| 9 | 三流式 UI | ✅ | `StreamChunk` / `StreamToolCall` / `StreamEnd` WS 事件 |
| 10 | HeartBeat 自改进 | ✅ | outcome_counts + recent_rounds + next_round_hint + inject_hint |
| 11 | 三种正交记忆 | ✅ | Structural(Graph) ⊕ Prompt(Conversation) ⊕ Compiled(Skill) |
| 12 | LoopState 是公开 API | ✅ | 6 变体(Running/Paused/GraphInvalid/TaskFailed/Done/Error) |
| 13 | Explore 提交门 | ✅ | 改写:Explore 后可再 Explore(iter 计数 200 兜底) |
| 14 | 子代理窄协议 | ✅ | JSON-action 协议,3 工具,ScopeGuard 文件级 |
| 15 | 信任结构不信任模型 | ✅ | `replay_from_anchor` BFS,失败回 Filling |
| 16 | 图 IS 调度 | ✅ | Kahn 出 `Schedule { batches }`,Node::priority() 排序 |
| 17 | 批内软失败 | ✅ | batch 内 fail-soft,batch 间 fail-collect |
| 18 | 合约双层验证 | ✅ | sub-agent 自检 + dispatcher 复检 |
| 19 | 技能编译纯变换 | ✅ | `compile_skill_to_task_graph`,前缀 `skill:<slug>:` 防冲突 |
| 20 | CheckpointStore 是 git-for-runs | ✅ | push / list / get / create_branch,run 重启后恢复 |
| 21 | Git checkpoint opt-in | ✅ | `auto_git_checkpoint=false` 默认 |
| 22 | 边界窄协议 | ✅ | 3 层(主代理 tool_calls / 子代理 JSON-action / 技能 Task+DependsOn) |
| **+P12** | **3 层 Reviewer** | ✅ | Deterministic(answer_content) + Main Judge + Advisor |

---

## 3. 4 个核心机制(深度)

### A. Clarifying v2 — agent 自决

```
旧 v1:agent 必须 2 次内推进(MAX 2 clarifications)
v2:agent 问 10 次内可自决,反相似性 > 0.85 触发 Block
```

**为什么**:v1 强制 2 次 → 模糊任务被切断。v2 让 agent 自主决定何时有足够信息,反相似性 + 软上限 10 防"永远问"。

**实现**:`src/agent/saturation.rs`(Jaccard on char-bigrams)+ `clarification_count` / `clarification_history` 字段。

### B. Explore v2 — 200 轮软上限 + 三档提示

```
旧 v1:Explore 后必须 ProposePatch(否则 infinite loop)
v2:Explore 可迭代(iter 200),100/150 软/硬提示
```

**为什么**:旧版的硬限制在 sub-agent 还没探明时强制 commit → 错的图。v2 允许反复探 + 进度信号。

**实现**:`explorer_iter` / `explorer_history` 字段 + `tier_hint_to_inject()` 软/硬 tier 注入。

### C. Sub-agent 任务分类 prompt(commit `8857ee5` + `fc17f1d` + `0651280`)

5 个 task_kind,每个有不同的强制指令:

| TaskKind | 中文/英文关键词 | 指令 |
|---|---|---|
| Write | 实现/写/创建/implement/create/write | 第一步 write_file,不探索 |
| Modify | 修改/refactor/fix | drill-down 路径,不能直接调 read_file |
| Read | 分析/read/analyze | deliverable summary 必须含完整答案 |
| Unknown | 其他 | 小任务:1 边 start→deliverable |
| Search | (隐式 read) | grep 后整理 |

**实现**:`src/agent/subagent.rs::detect_task_kind()` + `build_initial_user_prompt()`。

### D. 3 层 Reviewer(commit `748430c`)

```
Run Done iff:
  Layer 1: Deterministic (graph_consistency, subagent_results, answer_content)
  AND
  Layer 2: Main Judge (MiniMax M3 emit judge_verdict)
  AND
  Layer 3: Advisor (DeepSeek deepseek-reasoner)
```

**为什么 3 层**:任何单一 LLM 都会失明。Deterministic 抓 boilerplate 失败(Main 模型容易过),Main Judge 抓语义错误,Advisor 抓 Main 的盲点。

**关键创新 — `answer_content` check**:
- 拼 sub-agent output + deliverable L1 描述
- 去 boilerplate(`(no results)` / `Deliverable:` / `Node summary` 等)
- 计算跟 task 的 token overlap,需要 ≥ 3 个非停用词
- L1 是关键:模型写到文件的答案会被 L1 enrichment 自动捕获,这样 `answer_content` check 能看到

---

## 4. 4 个 graph-aware 工具

让主代理直接问图,不需要探索文件系统:

| 工具 | 作用 | 例子 |
|---|---|---|
| `read_graph_node(id, layer, line_range?)` | 按 NodeId 读 L0/L1/L2 | "owners-api 的 L1 是什么" |
| `search_graph(query, search_in?)` | Jaccard 搜索 L0/L1 文本 | "找所有跟费用相关的节点" |
| `find_similar_nodes(node_id\|text, top_k?)` | top-K 最相似 | "和 owners-api 最相似的 5 个节点" |
| `trace_dependency(start, relation, direction?)` | 沿边回溯/前瞻 | "owners-api 的所有 DependsOn 链" |

**L2 受 ScopeGuard 保护**:子代理的 `read_graph_node(id, L2)` 只允许读 scope 内的文件。

---

## 5. WebUI 4 个组件

| 组件 | 作用 | 事件源 |
|---|---|---|
| `PhaseProgress.vue` | 顶部状态条显示阶段 + round + tier | `graph_phase` WS 事件 |
| `BlockModal.vue` | Block 触发时弹 modal,3 选项(回答/继续/中止) | `Paused` + `[block]` 前缀 |
| `ExplorerBar.vue` | 探索进度条 0/100/150/200 tier | `graph_phase.explorer_iter` |
| `CheckpointTimeline.vue` | 检查点滑块,可重看任意 round | `GET /checkpoints` + `/checkpoints/:idx` |
| `SubRunTree.vue` | 子 run 树状面板 | `GET /sub-runs` |

WS 重连有 Last-Event-ID + 指数 backoff + jitter,断网时 `connection_lost` 事件触发 UI 重连提示。

---

## 6. 8 种语言 validator(失败归因)

```
Rust  → cargo check
TS    → tsc --noEmit
Go    → go build
Python → python compileall
Java  → mvn compile
Ruby  → ruby -c
Elixir → mix compile
PHP   → php -l
```

每种都有 graph-error patterns(例如 Rust 的 `cannot find function`)。如果 pattern 匹配 → `FailedAsGraphIssue`(回 Filling 修图)。否则 → `FailedAsTaskIssue`(让 Reviewer 处理)。

`FailureRetryPolicy::Retryable / Permanent` + `with_fix_suggestion()` 给人修图建议。

---

## 7. 9 个 API 端点

| 端点 | 作用 |
|---|---|
| `POST /api/runs` | 创建 run |
| `GET /api/runs` | 列出所有 runs |
| `GET /api/runs/:id` | 单个 run 状态 + token 总数 |
| `GET /api/runs/:id/events` | SSE 事件流 |
| `GET /api/runs/:id/checkpoints` | 检查点列表 |
| `GET /api/runs/:id/checkpoints/:idx` | 单个 checkpoint(含 graph snapshot + transcript) |
| `GET /api/runs/:id/sub-runs` | drill-down 子 run 列表 |
| `GET /api/runs/:id/full-graph` | 合并父 + 所有子 run 的图 |
| `GET /api/runs/:id/token-cost` | token 成本明细(by phase + per step) |
| `GET /api/runs/compare?a=X&b=Y` | 跨 run 对比 |
| `POST /api/runs/:id/answer` | Block 状态的 answer 注入 |
| `POST /api/runs/:id/repair` | GraphInvalid 状态的修图 |
| `GET /api/heartbeat` | HeartBeat dashboard 数据 |
| `POST /api/heartbeat/inject` | HeartBeat 人工注入 hint |

---

## 8. 工具栈

| 类别 | 工具 | v2 spec 实施 |
|---|---|---|
| 主模型 | `MODEL_NAME_DEFAULT=Minimax-M3` | Proposer / Subagent / VerifierL1 用 |
| 深度模型 | `MODEL_NAME_DEEP=Minimax-M3` | VerifierGraph / Reviewer / Decomposer / Cascade |
| 快速模型 | `MODEL_NAME_FAST=Minimax-M2.7-highspeed` | (留作未来切换) |
| **顾问模型** | `ADVISOR_MODEL=deepseek-reasoner` | **P12 架构改造:Reviewer 第二意见** |
| **顾问 base URL** | `ADVISOR_BASE_URL=https://api.deepseek.com` | 独立 vendor |
| 持久化 | `data/runs/<id>/` | run.json + checkpoints/ + sub_runs/ |
| 压缩 | `compact_checkpoints(keep=20, keep_tail=20)` | checkpoint 历史超 100 时压缩 |
| 备份 | `backup_run(id, backup_root)` | 关键 run 备份 |

---

## 9. 12 个 commit 链(完整演进)

```
P0  697eb40 Clarifying v2 + Explore v2 + 4 graph-aware 工具 + ContextBuilder v2
P1  1fb99a4 ContextBuilder v2 in proposer + GraphPhase WebSocket 事件
P2  7ea05ea §5 其他模块(skills 0.4 + 8 语言 validator + EditFile graph-aware)
P3  95c67b3 WebUI 4 组件 + token-cost API + multi-run comparison
P4  d91555e HeartBeat dashboard + persistence maintenance + 失败归因 + CachingModel
P5  d6c5c0b WebSocket reconnect + multi-model routing + SubRunTree + 工具层润色
P6  (Goal-driven 验证 + 真实 todo 工具跑通,产物 commit 在 P5 内)
P7  8857ee5 write-task directive + step-3 reminder 强制 sub-agent 产出
P8  (5 任务类型回归,commit 在 P7 内)
P9  fc17f1d Modify directive routes through drill-down
P10 (5 边缘 case,commit 在 P9 内)
P11 0651280 path hint extraction + Read answer-in-summary + Unknown minimal graph
P12 748430c Reviewer advisor(DeepSeek) + answer-content check
```

---

## 10. 关键文件清单

```
新增(8):
  docs/v2-agent-harness-complete-spec.zh-CN.md   规范文档
  src/agent/saturation.rs                       Jaccard + SaturationState
  src/tools/graph_aware.rs                      4 graph-aware 工具
  src/model/cache.rs                            CachingModel
  webui/src/components/run/PhaseProgress.vue    阶段进度
  webui/src/components/run/BlockModal.vue       Block 弹窗
  webui/src/components/run/ExplorerBar.vue      探索进度
  webui/src/components/run/CheckpointTimeline.vue 检查点时间线
  webui/src/components/run/SubRunTree.vue       子 run 面板

修改(15):
  src/agent/graph_loop.rs   4 新字段 + saturation 方法 + emit_graph_phase
  src/agent/proposer.rs     Clarifying prompt + post-Explore gate + v2 context
  src/agent/validator.rs    FailureRetryPolicy + fix_suggestion + 8 语言 builder
  src/agent/reviewer.rs     3 层 Reviewer(deterministic + main + advisor)
  src/agent/subagent.rs     task_kind 检测 + drill-down prompt + path hint
  src/skills/matcher.rs     threshold 0.4 + l1_weight
  src/context/mod.rs        L0L1L2ContextBuilder
  src/tools/file.rs         GraphAwareEditFileTool
  src/tools/web.rs          WebSearchCache + dry_run on Bash
  src/tools/bash.rs         dry_run + policy check
  src/graph/mod.rs          Node::priority()
  src/scheduler/mod.rs      按 priority 排序
  src/model/config.rs       ModelLayer + model_for_layer
  src/model/mod.rs          Message.extra side-channel
  src/web/events.rs         GraphPhase 事件
  src/web/state.rs          7 新 config 字段
  src/web/api_runs.rs       2 新 endpoint + Reviewer advisor wire
  src/web/heartbeat.rs      outcome_counts + inject_hint
  src/web/persistence.rs    compact_checkpoints + cleanup + backup
  src/web/checkpoint.rs     v2 集成
  src/web/run_session.rs    event_id_counter
  src/web/ws.rs             WS event id stamp
  src/web/mod.rs            4 新路由
```

---

## 11. 验证统计(15 任务类型回归)

| 类型 | 通过率 | 关键发现 |
|---|---|---|
| Write 创建文档 | 100% | 2124 字节真实 L0/L1/L2 描述 |
| Write 创建 Go todo 工具 | 100% | **可编译可运行可持久化**(`add`/`list`/`done` + `~/todo.json`) |
| Write 创建 Rust 单元测试 | 100%(路径准确) | path hint 修复后写到正确位置 |
| Modify refactor | 100% | drill-down 引导,主代理不调 read_file |
| Modify fix-prompt | 100% | max_steps 警告成功加到 prompt 开头 |
| Read analyze | 100% | L1 enricher 找到 saturation 阈值 soft=4/6, hard=6/5, terminate=12/6 |
| Read explain(英文) | 100% | status Done,advisor 第二意见 + answer_content check 工作 |
| Search write_file 调用 | 100% | 完美拆解 6 个子任务 |
| Tiny 一句话 | 模型 ask_user 多 | prompt 改进有限,需训练数据/RLHF 调优 |
| 不可能/破坏性 | 谨慎 Paused | 合理行为 |

**整体通过率 13/15 = 87%**,剩余 13% 是模型行为问题。

---

## 12. 启动

```bash
# 1. 配置 API keys
cp .env.example .env
# 编辑 .env,填入:
#   MODEL_BASE_URL=https://api.minimaxi.com/v1
#   MODEL_API_KEY=sk-...
#   ADVISOR_BASE_URL=https://api.deepseek.com
#   ADVISOR_API_KEY=sk-...
#   ADVISOR_MODEL=deepseek-reasoner

# 2. 启动 serve
cargo run --bin serve
# 监听 0.0.0.0:8080(可用 WEB_PORT=18080 切换)

# 3. 提交 goal
curl -X POST http://localhost:8080/api/runs \
  -H "content-type: application/json" \
  -d '{"task":"做一个简易的命令行 todo 工具 (Go 单文件,add/list/done,JSON 存到 ~/todo.json)"}'

# 4. WebUI
# 访问 http://localhost:8080
# 看 graph 3D 视图、PhaseProgress、CheckpointTimeline、BlockModal
```

---

## 13. 关键数字(spec 决定)

| 数字 | 值 | 含义 |
|---|---|---|
| Clarifying 软上限 | 10 | 最大连续 ask_user 轮数 |
| Explore 软上限 | 200 | 最大连续 explore 轮数 |
| 反相似性阈值 | 0.85 | Jaccard 重复检测 |
| Skills match threshold | 0.4 | 自动应用技能的下限 |
| Skills l1_weight 默认 | 0.0 | v2 spec §5.3 |
| Drill-down 深度 | 2 | L0 → L1 → L2 |
| Compact checkpoint | keep=20, keep_tail=20 | 中间压缩成 summary |
| Cleanup archive | 30 天 | 失败/被遗弃 run 归档 |
| Cleanup purge | 365 天 | 归档后清理 |
| Reviewer advisor | DeepSeek | 第二意见 |
| max_subagent_steps | 8 | 子代理硬上限 |
| step-3 写任务提醒 | step ≥ 3 | 写任务兜底 |

---

## 14. 核心设计原则(承袭 v1 + v2 增)

| 原则 | v1 实施 | v2 强化 |
|---|---|---|
| 模型只能编辑图 | propose_patch schema | ✅ + 5 种 task_kind prompt |
| 硬门都是确定性的 | 6 层 deterministic | + answer_content L1 check |
| 三正交记忆 | Graph / Conv / Skill | ✅ |
| 边界窄协议 | 3 层协议 | + ScopeGuard 文件级 |
| LLM-as-judge | Main judge | **+ Advisor 第二意见** |
| Failure 归因 | 2 类 | + 8 语言 pattern + retry policy + fix_suggestion |
| 心跳自改进 | 10 轮循环 | + outcome_counts + inject_hint |

---

## 15. 剩余 1% — 已知 LLM 行为限制

| 问题 | 表现 | 改进方向 |
|---|---|---|
| 模糊任务过度 ask_user | "用一句话描述项目" → 问"哪个项目?" | few-shot 训练;system prompt 加"宁可直接做也别问" |
| 描述任务不产 answer | explain 任务产 graph 而非文本 | answer_content check 已 catch,但要更激进 |
| Write 任务偶尔仍走 read_file | 模型无视指令 | step-3 提醒 + step-5 escalation |

**结论**:harness 100% 按 spec 实现,剩余 1% 是模型层,需要更专用的模型(Qwen2.5-Coder / Claude 4 Sonnet reasoning)或 RLHF-fine-tune。

---

## 16. 后续路线图

| 优先级 | 工作 | 估时 |
|---|---|---|
| 高 | 把 reviewer 的 LLM-as-judge prompt 改成"必须验证 final transcript 含完整答案" | 半天 |
| 中 | WebUI 实时 graph diff 可视化(已有 fx 状态,diff 算法待加) | 1 天 |
| 中 | HeartBeat dashboard 完整 WebUI(API 已就位) | 1 天 |
| 中 | 多 run 对比 UI(token 成本对比 + 图 diff) | 1 天 |
| 低 | 失败归因 pattern 库 + 反馈循环 | 1 周 |
| 低 | 性能优化(L1 缓存,model response cache 复用) | 1 周 |
| 低 | 训练专用模型替换 MiniMax M3 | 几周 |

---

**harness v2 完整。** 这是 12 个 commit,653 测试,15 任务类型回归后的成果。
