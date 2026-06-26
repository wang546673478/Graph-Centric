# Graph-Centric Agent Harness / 图认知 Agent 编排器

> **其他语言：** [English](README.md) | 简体中文

一个用 Rust 实现的、通用的 LLM agent 编排器，建立在一个核心论点之上：
**每一个 agent 任务，本质上都是对关系图（relationship graph）的操作。**
领域知识——代码、基础设施、研究、规划——通过同一个接缝注入；编排器本身保持通用。

## 核心思想

图**不是**被动数据存储,也不是事件流水。**图是 orchestrator 的计划**:大模型(作为主 agent)把图当成工作记忆来维护,整套循环是:

1. **规划(Plan)。** 主 agent 读图,然后 (a) 当任务范围清晰时,直接给图加子节点(Mode A —— 明确方案);或 (b) 当任务不清晰时,先发 `ask_user` 跟用户对齐方向(Mode B —— 探索形式),再去画节点。
2. **派发(Dispatch)。** 子 agent 各自负责一个子节点,以图为上下文执行。**子 agent 不直接改图**,只回报"成功"或"失败 + 证据"。
3. **复核(Review)。** 对每个子节点按 orchestrator 的规格做 per-node 复核。通过 → 节点标 done;不通过 → orchestrator 写一个**局部** `GraphPatch`(只动那一个子节点的规格,不是整张图),循环重派那一个子 agent。

"graph-centric" 在代码层面的含义:**每一次状态变更都是一个有明确 scope 的 `GraphPatch`**。LocalRepairer(处理 verifier 发现的问题)和 per-sub-agent-failure 重派,走的是**同一套机制**——只是触发源不同。

```
                  task description
                         |
                         v
        +---------- GRAPH ---------+ <----+
        |  propose -> verify ->   |      | repair
        |   repair (local) ->     |      |
        |   L1 enrich              |      |
        +-----------+--------------+      |
                    | verifier pass       |
                    v                     |
        +---------- TASK ----------+      |
        |  decompose -> dispatch  |      |
        |   (parallel sub-agents  |      |
        |    with tool loop)       |      |
        +-----------+--------------+      |
                    | all succeeded       |
                    v                     |
        +-- POST-EXECUTION VALIDATE -+    |
        |  (cargo check / pytest /  |    |
        |   custom; pattern-match   |    |
        |   stderr for graph hints) |    |
        +-----------+----------+----+    |
            graph    |   task         |
            issue    | issue / pass   |
                |    v                |
                |  +---------- REVIEW ----------+
                |  |  determ. checks +          |
                |  |   LLM-as-judge             |
                |  +--+----------------+--------+
                |     | pass           | fail (graph / scope)
                v     v                |
            GraphInvalid              Done                |
            (4 sources: <--------------------------------+
             VerifierStalemate,
             DuringExecution,        ^ surfaces to caller;
             PostExecutionValidation,  caller auto-repairs
             Review)                   and resumes
```

## 这是什么

- **通用的 LLM agent 编排器**，纯 Rust 实现。约 15K 行代码，单一二进制，
  零运行时依赖，**606 个单元/集成测试**。
- **模型无关（model-agnostic）**。走 OpenAI 兼容的 HTTP——支持 DeepSeek、
  vLLM、Ollama、OpenAI、OpenRouter、Anthropic-via-proxy，或者任何提供
  `/v1/chat/completions` 的服务。Reasoning-only 模型（DeepSeek-v3、
  MiniMax M3）是一等公民：每一层都有 `text_or_reasoning()` 兜底，
  当 `content` 为空时读 `reasoning_content`。
- **三层关系图**（L0 结构 / L1 语义 / L2 数据），作为主 agent、子 agent
  和用户之间的共享基底。
- **带显式状态转移的状态机**，不是自由形式的 ReAct 循环。每一个
  转移都是一次返回类型化 `LoopState` 的方法调用。

## 快速开始

### 1. 配置后端

把 `.env.example` 复制成 `.env`，填入：

```bash
MODEL_BASE_URL=https://api.deepseek.com/v1   # OpenAI 兼容端点
MODEL_API_KEY=sk-...                          # bearer token（本地部署可省）
MODEL_NAME_FAST=deepseek-v4-flash             # 高频调用（Proposer, SubAgent）
MODEL_NAME_DEEP=deepseek-v4-pro               # 质量敏感调用（Enricher, Repairer, Decomposer, Reviewer）
```

或者设置 `MODEL_NAME_DEFAULT` 把两层模型都路由到同一个模型。

### 2. 验证连通性

```bash
cargo run --bin probe_model
```

对每层模型发送一次 "ping"，报告延迟和 token 用量。在跑长任务之前先用
这个抓 URL / key / 模型名不匹配的问题。

### 3. 运行主 demo

```bash
cargo run --bin agent_a -- "你的任务"
```

参数也可以省略，会被提示输入。agent 会：

1. **入口（Intake）。** 任务不清楚时，主 agent 先发 `ask_user` 跟用户对齐方向，再画图节点；任务清晰时直接进入规划（对应上面核心思想里的 Mode A vs Mode B）。
2. 通过对话构建关系图（需要时主动问澄清问题）
3. 根据图把任务拆成子任务
4. 并发派发子 agent（每个带 `bash` 工具访问，受危险命令黑名单保护）
5. 跑 `cargo check` 作为执行后校验器（可配置）
6. 用确定性检查 + LLM-as-judge 做最终验收

输出落在 `./demo_output/`：

- `agent_a_graph.json` — 最终的 L0 + L1 图
- `agent_a_transcript.txt` — 完整对话
- `agent_a_task_outcome.json` — 子 agent 结果
- `agent_a_review.json` — 验收结论

## 三层图

按 v2.0 设计，图是分层的，让结构、语义和原始内容可以**独立地**被
验证和修订：

| 层 | 名称 | 内容 | 源 | 可变？ |
|---|---|---|---|---|
| **L0** | 骨架 | 节点 + 边 | scanner + model | 是（patches） |
| **L1** | 肌肉 | 每节点 `{responsibility, implementation, design_intent, constraints}` + confidence | model 读 L2 写 L1 | 是（re-enrich） |
| **L2** | 皮肤 | 原始字节（源文件、配置、schema 等） | 按需直接读 | 永不存进图 |

L0 patch 触发新节点的 L1 自动补全；L1 条目的 confidence 偏低会触发
重新补全；L2 变化（比如子 agent 改文件）最终触发 L0 + L1 更新。

## 架构理念

前面 "核心思想" 章节讲的是 **what**（plan → dispatch → review）。
本节讲 **why**：塑造每个组件的设计决策，以及它们接受的权衡。
如果你是贡献者，下面这些决策是你要保留的。

### 图是计划、是调度、是审计日志

同一个数据结构，干三件不同的事：

- **计划（plan）** — 主 agent 把图当作工作记忆来编辑。没有独立的
  "scratchpad" 或隐藏状态；模型的每一个意图都是一个 `GraphPatch`。
- **调度（schedule）** — `DagScheduler` 在 `DependsOn` 边上跑 Kahn
  算法，产出 **wave-aligned 批**。两个独立任务自动落在同一 wave；
  依赖任务等待它的前置 wave 完成。图的**结构决定并发性**
  ——dispatcher 不发明调度，它只执行图已经编码好的调度。
- **审计日志（audit log）** — `CheckpointStore` 在每次有意义的变更
  后快照 `(round, phase, graph, transcript)`，配 `branches` 映射
  支持 fork。你可以 rewind、replay、或者从任意历史状态 fork 出一个
  探索性变体。结合每次 patch 都会 bump 的 `Graph::version`，每一步
  都可追溯。

### 确定性优先于 LLM 评判（Defense in depth）

系统里有很多"信任模型"的决策。**没有一个是硬门（hard gate）**。
每个都被至少检查两次——一次是确定性机制，一次是 LLM-as-judge 顾问：

| "信任模型"的决策 | 确定性第二线 | LLM 顾问 |
|---|---|---|
| "图结构一致" | `Graph::find_inconsistencies`（悬空边、环、重复） | （无 —— 太简单了不需要 LLM）|
| "子 agent 工作正确" | `CheckContract`（`KnowHow` 关键字 / `Exploratory` 数量上限 / `MustEdit` 写调用计数）——被**检查两次**：子 agent 一次，dispatcher 再查一次 | （无）|
| "代码能编译" | `PostExecutionValidator` 跑 `cargo check` / `tsc` 并对 stderr 做 graph vs task 错误模式匹配 | （无）|
| "L1 与 L2 一致" | 子串比较 + drift 严重度 | `l1_check_verdict`（顾问式；从不单方面判失败）|
| "子 agent 声称 done 是诚实的" | dispatcher 在子 agent 返回后**重新**评估 contract | （无）|
| "最终结果可接受" | 确定性 reviewer（图一致性、子 agent 成功、verify-phase 状态）| `judge_verdict`（顾问式；root_cause 路由到 repair）|

**不可靠的模型不能让结构上正确的图崩掉。** 这是系统最重要的安全属性。
任何新的"信任模型"决策必须配套一个确定性的第二线检查，否则不能上。

### 边界处窄协议，内部宽协议

代码库里反复出现的模式：**越深入系统，协议越窄。** 这是有意识的设计
选择，不是疏漏。

| 层 | 协议 | 宽度 | 为何要窄 |
|---|---|---|---|
| 主 agent | OpenAI `tool_calls`（6 种 step 类型）| 宽 | 编排需要灵活性 |
| 子 agent | 自定义 JSON-action（`use_tool` / `final_answer` / `report_graph_error`）| 窄 | 约束执行环境（`max_steps=8`，无直接图访问）；窄 = 容易验证 |
| Skill 编译 | `NodeKind::Task` + `DependsOn` only | 更窄 | Skill 被缓存、回放、信任；窄 = 安全的缓存 |

看到这种不一致，第一个冲动是**统一**——让子 agent 也用 `tool_calls`，
让 skill 也输出完整 `GraphPatch`。**别这么做。** 每次收窄都是
defense-in-depth 决策：边界处协议越窄，如果模型在该层失控，爆炸半径
越小。如果将来有贡献者提议跨边界统一协议，要问的问题只有一个：
**我们丢掉的安全保证是什么？**

### 三个正交的记忆层

系统有三种截然不同、互补的"记忆"：

| 层 | 存储 | 生命周期 | 内容 |
|---|---|---|---|
| **结构**（graph）| 内存中的 `Graph` + checkpoint 到磁盘 | 一次 run | L0 节点/边 + L1 描述 —— orchestrator 的计划 |
| **提示词**（conversation）| 内存中的 `Conversation` | 一次 run | LLM 对话历史 —— 模型看到过的东西，包括 `ask_user` 交换、verifier 拒绝、repair 尝试 |
| **编译后**（skills）| `LocalSkillStorage`（文件系统）| 永久 | 抽取出来的、跑成功过的任务 DAG，按 Jaccard-token 相似度索引 |

这三个**正交**：skill 不漏到 graph，graph 不漏到 conversation，conversation
不漏到 skill。新的"记忆"功能应选一个层和一条写入路径；抵制"哪里都放
一份"的诱惑。

### Skill 是结构化记忆，不是提示词记忆

当一次 run 成功到达 `ready_for_verify`，orchestrator 抽取 `propose_patch`
序列作为编译后的任务 DAG，存到本地（`LocalSkillStorage`）。下一次
有 token-Jaccard ≥ 0.25 匹配的任务，**完全跳过 decomposer**，直接
用编译后的 skill 图。这是结构化记忆：skill 是图拓扑，不是提示词
片段。成功的 run 会复利 —— agent 在已经做过的事上越来越快，速度
提升也建立在那驱动一切的同一种 artifact（`Graph`）上。

### 子 agent 是被约束的，不是被信任的

子 agent 跑的时候有三层独立约束，**全部**用代码强制：

1. **`max_steps`**（默认 8）—— 每个子 agent 的模型调用次数硬上限。
2. **`ScopeGuard`** —— 每个 `use_tool` action 在调用**前**对照允许路径
   策略做检查。一个被派去"修 `auth.rs`"的子 agent 不能写 `/var/log`
   或 `~/.ssh/`。bounded context 在**文件系统层**强制，不只在
   认知层。
3. **`CheckContract`** —— 子 agent 的 `final_answer` 对照一个确定性
   谓词做验证（必须提到期望短语、必须留在 region 内、"must-edit"
   任务必须有过写工具调用）。检查**跑两遍**——子 agent 自己一遍，
   dispatcher 再查一遍。任何一层都能让 run 失败。

再加一个 `report_graph_error` action，让子 agent 在发现图本身有问题
时**把 `GraphError` 冒泡**到主循环——这是子 agent 在 repair 流程中的
发言权。

### 两种 intake 模式，代码门控

Round 0（一次新对话的第一轮）有一道门。模糊任务（启发式：EN+ZH
模糊起点短语、很短且无动词、单词）必须先发 `ask_user` 再画任何
图节点。清晰任务可以直接发 `propose_patch`。这道门是第二道防线
——system prompt 也教 Mode A vs Mode B，但仅靠 prompt 不构成
load-bearing 约束。**倾向于放行**：假阳性只是一个烦人的 `ask_user`；
假阴性是拿一次 run 浪费在一张从模糊意图上建出来的图上。

### 图是公开 API

虽然 `graph_loop.rs` 有大约 6.7K 行，但 run loop 的整个公开 API 只是
`LoopState` 里的 **5 个变体**：`Running`（继续 stepping）、`Paused`
（等 `ask_user` 回答）、`GraphInvalid`（调用方要修复）、`Done`（终态
成功）、`Error`（终态失败）。web gateway 只看到这 5 个；循环内部
一切都是私有的。正是这种纪律，让核心层可以自由重构而不破坏
gateway。

## 状态机

`GraphLoop::step()` 推进一步并返回 `LoopState`：

```rust
pub enum LoopState {
    Running,                                          // 继续 stepping
    Paused { question, rationale },                   // 问用户，然后 resume(answer)
    GraphInvalid { source, errors, snapshot },        // 调用方修复，resume_with_repaired_graph(g)
    TaskFailed { failures },                          // 子 agent 在代码层失败
    Done(FinalResult),                                // 终态：通过
    Error(String),                                    // 终态：毒化
}
```

`GraphInvalid` 是核心的恢复状态。它可以从四个源触发，调用方都通过同一
对 `resume_*` 方法处理：

| `ErrorSource` | 起源 | 触发条件 |
|---|---|---|
| `VerifierStalemate` | Graph 阶段内 | LocalRepairer 的修复预算耗尽 |
| `DuringExecution` | Task 阶段内 | 子 agent 的 JSON action 是 `report_graph_error` |
| `PostExecutionValidation` | Task 与 Review 之间 | 配置的 validator（如 `cargo check`）在失败输出里看到图错误模式 |
| `Review` | Review 阶段内 | LLM judge 返回 `verdict: fail` 且 `root_cause: graph` 或 `scope` |

四种情况下，调用方都遍历 errors，挨个调用
`LocalRepairer::repair_from_error`，把 patch 应用到 snapshot，然后调
`gl.resume_with_repaired_graph(repaired)`。Demo A 把这个包成最多
3 轮的自动修复循环；生产端的调用方可以挂人审或升级策略。

## 组件

| 模块 | 职责 | 关键类型 |
|---|---|---|
| `graph::` | L0 存储 + 遍历 + 校验 | `Graph`, `Node`, `Edge`, `NodeId`, `NodeKind`, `RelationType`, `GraphPatch`, `Inconsistency`, `L1Description`, `L1Store` |
| `scheduler::` | 拓扑批调度（Kahn 风格的 wave） | `DagScheduler`, `Schedule` |
| `context::` | 子 agent 上下文组装 | `ContextBuilder`, `ContextBudget`, `SourceLoader`, `FilesystemSources`, `InMemorySources`, `NullSourceLoader` |
| `model::` | 模型抽象 + OpenAI-兼容客户端 | `Model` trait, `OpenAICompatModel`, `ModelConfig`, `Message`, `ModelRequest`, `ModelResponse`, `StreamDelta` |
| `model::text_or_reasoning()` | 推理内容兜底（DeepSeek / M3） | `ModelResponse` 上的方法 |
| `tools::` | 工具表面 + Bash 执行 + 两道护栏 | `Tool` trait, `ToolRegistry`, `ToolDef`, `ToolOutput`, `ToolContext`, `Policy` (`DangerousCommandDeny`/`ReadOnly`/`AllowAll`/`AllowList`), `BashTool`, `ScopeGuard`, `truncate_tail` |
| `agent::conversation` | 多轮对话状态 | `Conversation` |
| `agent::intake` | Mode A/B intake gate（模糊 → ask_user） | `classify_task_clarity`, `check_intake_compliance` |
| `agent::proposer` | 主 agent 步进引擎（6 种 step 类型，OpenAI tool_calls） | `GraphProposer`, `ProposerStep` (`AskUser` / `Explore` / `ProposePatch` / `ReadyForVerify` / `Block` / `ConsultAdvisor`) |
| `agent::verifier` | 三层校验（结构 + model 自检 + L1 抽样） | `Verifier`, `VerifyIssue`, `VerificationResult`, `Severity` |
| `agent::enricher` | L1 补全（model-driven） | `L1Enricher` |
| `agent::repairer` | 局部图修复（L0Structural / L1Semantic / ScopeGap） | `LocalRepairer` |
| `agent::decomposer` | 任务分解 | `Decomposer` |
| `agent::dispatcher` | wave-aligned 并发批执行 | `Dispatcher`, `SubAgentPool`, `DispatchOutcome` |
| `agent::subagent` | JSON-action 子任务执行器 + ScopeGuard | `SubAgent`, `SubTask`, `SubAgentResult` |
| `agent::validator` | 执行后确定性检查 | `PostExecutionValidator`, `ValidationVerdict`, `BashCheckValidator` |
| `agent::reviewer` | 最终验收 | `Reviewer`, `JudgeVerdict`, `RootCause` |
| `agent::cascade` | 子 agent 失败时级联回溯 | `CascadeBacktracker`, `PredecessorVerdict` |
| `agent::cascade_expand` | Task 阶段 L0→L1→L2 级联展开 | `expand_graph` |
| `agent::contract` | 子 agent 派发契约（确定性） | `CheckContract`（`KnowHow` / `Exploratory` / `MustEdit` / `None`）|
| `agent::graph_loop` | 顶层状态机 | `GraphLoop`, `GraphLoopConfig`, `LoopState`, `FinalResult` |
| `skills::` | Skill 捕获、匹配、编译、存储 | `matcher`（Jaccard）、`capture`、`compiler`（纯函数）、`retrieve`、`LocalSkillStorage` |
| `web::` | HTTP/WS gateway | `api_runs`, `ws`, `events`, `heartbeat`, `persistence`, `checkpoint`（CheckpointStore + branching），`state` |
| `domain::` | 领域接缝（scanner、tool 注册、validator） | `Domain`, `Scanner`, `ToolRegistry` trait, `DomainValidator`, `TaskNeeds` |
| `domain::code::` | code 域的实例 | `CodeScanner` |

三个二进制：

| 二进制 | 命令 | 用途 |
|---|---|---|
| `agent_a` | `cargo run --bin agent_a` | 主 demo：完整的图→任务→验收循环（生产形态） |
| `demo` | `cargo run --bin demo` | Phase 1 确定性 scanner demo（扫 `./src` 进图） |
| `graph_harness` | `cargo run --bin graph_harness` | Phase 1 烟雾测试（构造一个最小的图并打印） |

## 测试

```bash
cargo test --lib                 # 全部 606 个单元 + 集成测试
cargo test --lib agent::         # 只跑 agent 层
cargo test --lib tools::bash::   # 只跑 bash 工具
cargo test --lib tools::scope_guard::  # 只跑 scope guard
cargo test --lib graph::         # 只跑图类型
```

Live model 测试用 `LIVE_MODEL_TEST=1` 闸：

```bash
LIVE_MODEL_TEST=1 cargo test --lib model::openai_compat
```

## 文档

- **[English](README.md)** (本文件) — 快速开始、功能、诚实范围
- **[简体中文](README.zh-CN.md)** — 中文版
- **[ARCHITECTURE.md](ARCHITECTURE.md)** — 设计理念、否决过的备选、权衡
- **[ARCHITECTURE.zh-CN.md](ARCHITECTURE.zh-CN.md)** — 中文版架构文档
- `docs/superpowers/specs/` — 设计 spec（英文），两个已完成的工具系统特性
- `docs/superpowers/plans/` — 实施 plan（英文），同上

## 仓库布局

```
src/
├── lib.rs                     # crate 根 + re-exports
├── main.rs                    # 旧版 Phase 1 烟雾二进制
├── error.rs                   # HarnessError + Result alias
├── graph/                     # L0/L1 存储：nodes, edges, patches, validation, traversal
├── model/                     # Model trait + OpenAI 兼容 HTTP 客户端
├── context/                   # 子 agent 的 SourceLoader + 上下文组装
├── scheduler/                 # 拓扑波次调度
├── domain/                    # 领域接缝（scanner、tool 注册、validator）
│   └── code/                  #   code 域的实例
├── tools/                     # 工具：Tool trait、Policy、ToolRegistry、bash、deny_list、ScopeGuard
├── agent/                     # 核心编排：graph_loop、proposer、verifier、repairer、
│                              #   enricher、decomposer、dispatcher、subagent、
│                              #   validator、reviewer、conversation
└── bin/
    ├── agent_a.rs             # 主 demo 二进制
    ├── demo.rs                # Phase 1 scanner demo
    └── probe_model.rs         # 连通性探针
tests/
└── integration_tool_guards.rs # 端到端护栏测试（rm -rf 拦截、scope 越界等）
```

## 设计原则

这些原则塑造每个组件。**架构理念** 章节有详细解释；这里是
TL;DR 列表。

1. **模型无关**。源代码里不写死模型名；所有模型选择走 `ModelConfig`
   读环境变量。Reasoning-only 模型（DeepSeek-v3、MiniMax M3）是一等
   公民——每一层都走 `ModelResponse::text_or_reasoning()` 兜底。
2. **图是计划、是调度、是审计日志**。三件事，同一个数据结构。详见
   *架构理念*。
3. **确定性优先于 LLM 评判**。每个"信任模型"的决策都有确定性第二
   线检查。不可靠的模型不能拖垮结构上正确的图。详见 *架构理念*。
4. **边界处窄协议，内部宽协议**。主 agent 用 OpenAI `tool_calls`；
   子 agent 用自定义 JSON-action；Skill 编译用 Task + DependsOn。
   每次收窄都是 defense-in-depth 决策。详见 *架构理念*。
5. **三个正交的记忆层**。结构（graph）、提示词（conversation）、
   编译后（skills）——三者之间不互相泄漏。详见 *架构理念*。
6. **Skill 是结构化记忆，不是提示词记忆**。成功的 run 抽取
   `propose_patch` 序列作为编译后的任务 DAG，Jaccard ≥ 0.25 时复用。
   详见 *架构理念*。
7. **子 agent 是被约束的，不是被信任的**。`max_steps` + `ScopeGuard`
   + `CheckContract`（双重检查）+ `report_graph_error` 冒泡。详见
   *架构理念*。
8. **时间换精度**。很多小而精确的修正胜过少量批量修正。每次执行
   中捕捉的错误都是一个精度信号——永远不要为了"效率"批量处理错误。
9. **局部图修复，不批量**。当 verifier 找到问题时，一次修一个，
   用 subgraph-scoped patch。全局重建是显式 opt-in，不是错误路径。
10. **通用性在模型里，结构在图里**。harness 跨领域通用；领域相关
    的判断委托给模型。不要把领域 enum 塞进共享类型。
11. **Reviewer 需要确定性后盾**。LLM-as-judge 单独不可靠。在信任
    模型裁决**之前**叠加多层确定性检查（图一致性、子 agent 成功、
    执行后校验）。
12. **Scanner 是种子，不是产品**。代码/数据/基础设施 scanner 产出
    低置信度（≤ 0.6）的种子图。模型才是真正的图构造器；不要在
    scanner 巧妙性上过度投入。

## 诚实范围（Honest scope）

**还不是（yet）：**

- **完整的代码编辑 agent**。子 agent 用默认的
  `DangerousCommandDeny` 策略（精确的高危命令黑名单，比如
  `rm -rf /`、`kubectl delete`、`git push --force`）加上自动派生的
  `ScopeGuard`（写操作限制在 `task.involved_nodes` 能到达的路径）。
  想突破这些边界——比如放开破坏性命令、扩大写范围——需要显式的
  `with_pattern` / `without_pattern` / 自定义 guard 配置。

- **完整的多 agent 框架**（带命名角色、持久记忆等）。子 agent 是单次
  工具循环；嵌套 GraphLoop 留给未来。

### Build tool caveats

bash 护栏把常见 build 工具（`cargo`、`npm`、`pip`、`make`、`go`、
`python`、`node`、`rustc`）识别为"隐式 cwd 写"：当命令没有显式的
`--target-dir` 类参数时，scope 检查被跳过（我们假设工具写到
cwd 下的子目录，比如 `target/` 或 `node_modules/`）。这可以通过
`ScopeGuard::with_implicit_cwd_verb` / `without_implicit_cwd_verb`
配置。

**三个已知的 v1.1 限制：**

1. **系统级 install 命令是被允许的。** `cargo install foo`、
   `pip install foo`、`npm install -g foo` 落入同样的规则且被放行。
   它们实际写到 `~/.cargo/`、site-packages 或全局 node_modules —
   这些通常**不在** agent 允许的 scope 内。**Mitigation：** 在
   dispatcher 配置里调
   `ScopeGuard::without_implicit_cwd_verb("cargo")`（或 `pip`、`npm`）
   来在更严的环境下禁用。

2. **build 工具检测只看第一个 token。** 一个 shell 别名叫 `cargo`
   写 `/etc/` 会通过 verb 检查。`DangerousCommandDeny` 会拦破坏性
   payload；scope 检查会拦显式出 scope 的路径。但两者都不拦巧妙
   的别名。请相应地信任模型。

3. **`cargo run`、`cargo test`、`cargo bench` 是被允许的。** 它们
   可能执行任意代码。黑名单抓不到。**Mitigation：** 调
   `ScopeGuard::without_implicit_cwd_verb("cargo")` 禁用所有 cargo
   调用，或者挂一个自定义 `Policy`。

## 许可协议

双协议 MIT OR Apache-2.0（见 `Cargo.toml`）。
