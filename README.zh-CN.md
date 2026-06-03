# Graph-Centric Agent Harness / 图认知 Agent 编排器

> **其他语言：** [English](README.md) | 简体中文

一个用 Rust 实现的、通用的 LLM agent 编排器，建立在一个核心论点之上：
**每一个 agent 任务，本质上都是对关系图（relationship graph）的操作。**
领域知识——代码、基础设施、研究、规划——通过同一个接缝注入；编排器本身保持通用。

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
  零运行时依赖，**310 个单元/集成测试**。
- **模型无关（model-agnostic）**。走 OpenAI 兼容的 HTTP——支持 DeepSeek、
  vLLM、Ollama、OpenAI、OpenRouter、Anthropic-via-proxy，或者任何提供
  `/v1/chat/completions` 的服务。
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

1. 通过对话构建关系图（需要时主动问澄清问题）
2. 根据图把任务拆成子任务
3. 并发派发子 agent（每个带 `bash` 工具访问，受危险命令黑名单保护）
4. 跑 `cargo check` 作为执行后校验器（可配置）
5. 用确定性检查 + LLM-as-judge 做最终验收

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
| `scheduler::` | 拓扑批调度 | `DagScheduler`, `Schedule` |
| `context::` | 子 agent 上下文组装 | `ContextBuilder`, `ContextBudget`, `SourceLoader`, `FilesystemSources`, `InMemorySources`, `NullSourceLoader` |
| `model::` | 模型抽象 + OpenAI-兼容客户端 | `Model` trait, `OpenAICompatModel`, `ModelConfig`, `Message`, `ModelRequest`, `ModelResponse` |
| `tools::` | 工具表面 + Bash 执行 + 两道护栏 | `Tool` trait, `ToolRegistry`, `ToolDef`, `ToolOutput`, `ToolContext`, `Policy` (`DangerousCommandDeny`/`ReadOnly`/`AllowAll`/`AllowList`), `BashTool`, `ScopeGuard`, `truncate_tail` |
| `agent::conversation` | 多轮对话状态 | `Conversation` |
| `agent::proposer` | 通过 model 输出的 JSON step 构建图 | `GraphProposer`, `ProposerStep` (`AskUser`, `CallTool`, `ProposePatch`, `ReadyForVerify`) |
| `agent::verifier` | 三层校验 | `Verifier`, `VerifyIssue` |
| `agent::enricher` | L1 补全（model-driven） | `L1Enricher` |
| `agent::repairer` | 局部图修复 | `LocalRepairer`, `GraphError` |
| `agent::decomposer` | 任务分解 | `Decomposer` |
| `agent::dispatcher` | 并发子 agent 派发 | `Dispatcher`, `SubAgentPool` |
| `agent::subagent` | 单次工具循环子 agent | `SubAgent`, `SubTask`, `SubAgentResult` |
| `agent::validator` | 执行后确定性检查 | `PostExecutionValidator` (`BashCheckValidator`, `AlwaysPasses`) |
| `agent::reviewer` | 最终验收 | `Reviewer`, `JudgeVerdict`, `RootCause` |
| `agent::graph_loop` | 顶层状态机 | `GraphLoop`, `GraphLoopConfig`, `LoopState`, `FinalResult` |

三个二进制：

| 二进制 | 命令 | 用途 |
|---|---|---|
| `agent_a` | `cargo run --bin agent_a` | 主 demo：完整的图→任务→验收循环（生产形态） |
| `demo` | `cargo run --bin demo` | Phase 1 确定性 scanner demo（扫 `./src` 进图） |
| `graph_harness` | `cargo run --bin graph_harness` | Phase 1 烟雾测试（构造一个最小的图并打印） |

## 测试

```bash
cargo test --lib                 # 全部 310 个单元 + 集成测试
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
- **生产级 retry / backoff 层**。`OpenAICompatModel` 每次
  `complete()` 只发一次 HTTP 请求；限流或瞬时错误以
  `HarnessError::Model` 冒上来。
- **持久化层**。图能通过 `Graph::to_json` 序列化为 JSON，但没有内建的
  session store、checkpoint 或跨进程恢复。

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
