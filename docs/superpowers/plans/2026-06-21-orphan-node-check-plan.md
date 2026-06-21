# 建图阶段孤儿节点检查 + 连边引导 + 语义命名 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** 让建图阶段(Filling/Expanding)在每步 propose_patch 后检测孤儿节点(从 start 到不了的),提示模型用 LeadsTo 边把它们接进 start→…→deliverable 主链;ready_for_verify 时若仍有孤儿则退回 Filling;并修过时的提示词(A/D/DependsOn/T1 → start/deliverable/LeadsTo/语义名)。

**Architecture:** 纯 agent 核心改动,单文件 `graph_loop.rs` + prompt 字符串。复用已有的 `replay_from_anchor()`(目前只在 Task 阶段错误后调用),在建图阶段每步 patch 应用后也调它;新增 `last_orphan_hint_sig: Option<u64>` 字段防重复刷屏;ready_for_verify 入口加孤儿兜底。提示词改命名引导。

**Tech Stack:** Rust。`cargo test --lib` 回归 + 新增单测。

参考:spec `docs/superpowers/specs/2026-06-21-orphan-node-check-design.md`。

读码确认:
- patch 应用成功分支在 `step_graph` :1627,phase-transition 块 :1637-1667,auto_enrich 在 :1670-1672 —— 孤儿检查插在 enrich 之后、Ok 分支末尾。
- `replay_from_anchor()`(:3037 附近)已存在,返回 `Vec<NodeId>`(从 start 到不了的非锚点节点),已是新方向(start→node)。
- `run_verify_and_maybe_repair`(:1766)是 ready_for_verify 的处理入口。
- 字段区:`convergence_hint_sent`(:656)、`clarifying_primed`(:658);构造器初始化 :830-831。
- `build_filling_hint()` 含旧的 `A → T1 → D` + `DependsOn`。
- Seeding→Filling 转换提示 :1648 含 "between A and D"。
- `hash_string()`(graph_loop.rs 内已有,convergence 用过)可复用做孤儿集合签名。
- propose_patch 工具 example(proposer.rs:898)已是 start/deliverable/LeadsTo(seed),无需改;命名引导加在 build_filling_hint + 转换提示。

---

## File Structure
- Modify: `src/agent/graph_loop.rs` — 新增 `last_orphan_hint_sig` 字段 + 构造器初始化;建图阶段 patch 后孤儿检查;ready_for_verify 兜底;修 build_filling_hint + 转换提示 + 命名引导;单测。

---

## Task 1: 新增 `last_orphan_hint_sig` 字段

**Files:** `src/agent/graph_loop.rs`

- [ ] **Step 1: 加字段**

在结构体字段 `clarifying_primed: bool,`(:658)之后加:

```rust
    /// Signature (hash) of the last orphan-node set we hinted about, so we
    /// don't re-inject the same "connect these nodes" hint every step. None
    /// means no hint sent yet. Reset implicitly when the orphan set changes.
    last_orphan_hint_sig: Option<u64>,
```

在构造器 `Self { ... }` 里 `clarifying_primed: false,`(:831)之后加:

```rust
            last_orphan_hint_sig: None,
```

- [ ] **Step 2: 构建**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error" | head; echo exit=${PIPESTATUS[0]}`
Expected: exit=0(新字段未用会有 dead_code 警告,可接受;若 deny warnings 则下个 task 用上后消失——本仓不 deny,exit=0)。

- [ ] **Step 3: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "feat(agent): add last_orphan_hint_sig field for orphan-hint dedup"
```

---

## Task 2: 建图阶段每步 patch 后检测孤儿 + 提示连边

**Files:** `src/agent/graph_loop.rs`

在 patch 应用成功分支(:1627 `Ok(())` 内),auto_enrich(:1670-1672)**之后**、`Ok(())` 分支结束前,加孤儿检查。

- [ ] **Step 1: 加孤儿检查块**

找到(:1670-1672):
```rust
                        // L0 → L1 linkage: auto-enrich brand-new nodes.
                        if !new_node_ids.is_empty() {
                            self.auto_enrich(&new_node_ids).await;
                        }
                    }
```
在 `self.auto_enrich(&new_node_ids).await;` 那个 `}` 之后、外层 `}`(Ok 分支结束)之前,插入:

```rust
                        // Orphan check: in build phases, after each patch,
                        // detect nodes start can't reach (added but not wired
                        // into the start→…→deliverable chain) and prompt the
                        // model to connect them with LeadsTo. Dedup via the
                        // orphan-set signature so we don't repeat the hint
                        // every step. Skip Seeding (only start/deliverable).
                        if matches!(self.graph_phase, GraphPhase::Filling | GraphPhase::Expanding) {
                            let orphans = self.replay_from_anchor();
                            if orphans.is_empty() {
                                self.last_orphan_hint_sig = None;
                            } else {
                                let mut joined = String::new();
                                for id in &orphans {
                                    joined.push_str(id.as_str());
                                    joined.push('|');
                                }
                                let sig = hash_string(&joined);
                                if self.last_orphan_hint_sig != Some(sig) {
                                    self.last_orphan_hint_sig = Some(sig);
                                    let ids = orphans
                                        .iter()
                                        .map(|id| id.to_string())
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    self.conversation.add_user(format!(
                                        "⚠️ These nodes are NOT yet connected into the main chain \
                                         (start cannot reach them): {ids}. They are floating \
                                         orphans. Add `LeadsTo` edges to wire each one into the \
                                         flow so the path runs start → … → deliverable. Every \
                                         step node must sit on the path from start to deliverable."
                                    ));
                                }
                            }
                        }
```

- [ ] **Step 2: 构建**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error" | head; echo exit=${PIPESTATUS[0]}`
Expected: exit=0。(`hash_string` 与 `replay_from_anchor` 已在本文件,`GraphPhase` 已在作用域。)

- [ ] **Step 3: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "feat(agent): build-phase orphan check + connect-edge hint (deduped)"
```

---

## Task 3: ready_for_verify 孤儿兜底

**Files:** `src/agent/graph_loop.rs`

`run_verify_and_maybe_repair`(:1766)是 ready_for_verify 的入口。进验证前先查孤儿;有则退回 Filling、要求连边。

- [ ] **Step 1: 在 verify 入口加兜底**

把 `run_verify_and_maybe_repair` 开头:
```rust
    async fn run_verify_and_maybe_repair(&mut self) -> Result<LoopState> {
        let result = self
            .verifier
            .verify(&self.graph, &self.task, Some(&self.conversation))
            .await?;
```
改为(先查孤儿):
```rust
    async fn run_verify_and_maybe_repair(&mut self) -> Result<LoopState> {
        // Backstop: don't hand off to verification with orphan nodes (steps
        // start can't reach). Bounce back to Filling and require the model to
        // wire them into the start→…→deliverable chain first.
        let orphans = self.replay_from_anchor();
        if !orphans.is_empty() {
            let ids = orphans
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            warn!(orphans = %ids, "ready_for_verify blocked: orphan nodes not on the chain");
            self.graph_phase = GraphPhase::Filling;
            self.conversation.add_user(format!(
                "Cannot verify yet: these nodes are not connected into the main chain \
                 (start cannot reach them): {ids}. Add `LeadsTo` edges to put each on the \
                 path start → … → deliverable, then emit `ready_for_verify` again."
            ));
            return Ok(LoopState::Running);
        }
        let result = self
            .verifier
            .verify(&self.graph, &self.task, Some(&self.conversation))
            .await?;
```

- [ ] **Step 2: 构建**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error" | head; echo exit=${PIPESTATUS[0]}`
Expected: exit=0。(`warn!` 已在本文件用过。)

- [ ] **Step 3: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "feat(agent): ready_for_verify bounces back to Filling if orphans exist"
```

---

## Task 4: 修过时提示词 + 语义命名引导

**Files:** `src/agent/graph_loop.rs`(build_filling_hint + 转换提示)

- [ ] **Step 1: 重写 build_filling_hint**

把 `build_filling_hint()` 整体替换为(去掉 A/T1/D + DependsOn,改 start/语义名/LeadsTo,强调连边):

```rust
    fn build_filling_hint(&self) -> String {
        let node_info: Vec<String> = self.graph.nodes.values().map(|n| {
            format!("- {} (kind={:?}, summary=\"{}\")", n.id.as_str(), n.kind, n.summary)
        }).collect();
        format!(
            "🔧 You've spent several rounds without adding connected intermediate \
             steps between start and deliverable. Based on what you know, NOW add \
             step nodes AND wire them into the flow. Rules:\n\
             - Use semantic ids (e.g. `outline`, `draft-intro`, `code-examples`, \
             `proofread`), NOT letter+number ids like B1/B2/T1.\n\
             - Every step node MUST sit on the path: connect with `LeadsTo` edges so \
             it reads start → step → … → deliverable. Do not add a node without an \
             edge wiring it in.\n\
             - Emit a `propose_patch` now with the step node(s) AND their LeadsTo \
             edges. Do NOT explore again — you have enough information.\n\n\
             Current graph:\n{node_info}",
            node_info = node_info.join("\n")
        )
    }
```

- [ ] **Step 2: 修 Seeding→Filling 转换提示**

找到 :1648 的转换提示(Seeding→Filling 分支里的 `self.conversation.add_user(...)`),把:
```rust
                                    self.conversation.add_user(
                                        "✅ Start→Goal established. Now explore to understand \
                                         what intermediate steps are needed between A and D. \
                                         Use `explore` to read relevant files, then \
                                         `propose_patch` to insert intermediate Task nodes."
                                    );
```
替换为:
```rust
                                    self.conversation.add_user(
                                        "✅ start→deliverable established. Now work out the \
                                         intermediate steps needed BETWEEN start and deliverable. \
                                         Explore if needed, then `propose_patch` to insert step \
                                         nodes — give them semantic ids (e.g. `outline`, \
                                         `draft-intro`), NOT B1/B2/T1 — and connect each with \
                                         `LeadsTo` edges so the path runs start → … → deliverable."
                                    );
```

- [ ] **Step 3: 构建**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error" | head; echo exit=${PIPESTATUS[0]}`
Expected: exit=0。

- [ ] **Step 4: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "feat(agent): filling hints teach semantic ids + mandatory LeadsTo wiring"
```

---

## Task 5: 单元测试

**Files:** `src/agent/graph_loop.rs`(测试模块)

复用现有测试 helper(`build_loop_with`、`Node::task`、`Edge::new`、`RelationType`、`NodeId`)。测试直接操作 `gl.graph` 构造场景,调被测方法。

- [ ] **Step 1: 写测试**

在测试模块加(放孤儿/replay 相关测试附近):

```rust
    #[test]
    fn orphan_nodes_detected_when_not_wired() {
        use crate::graph::{Edge, Node, NodeId, RelationType};
        let mut gl = build_loop_with(vec!["{}"]);
        // start --LeadsTo--> deliverable, plus two unwired step nodes.
        let mut start = Node::task("start", "Start");
        start.immutable = true;
        gl.graph.add_node(start);
        gl.graph.add_node(Node::task("deliverable", "Deliverable"));
        gl.graph.add_node(Node::task("outline", "Outline"));
        gl.graph.add_node(Node::task("draft", "Draft"));
        gl.graph.add_edge(Edge::new("start", "deliverable", RelationType::LeadsTo, 0.9, "")).unwrap();
        let orphans = gl.replay_from_anchor();
        // outline + draft are unreachable from start.
        assert!(orphans.contains(&NodeId::from("outline")));
        assert!(orphans.contains(&NodeId::from("draft")));
        assert!(!orphans.contains(&NodeId::from("deliverable")));
    }

    #[test]
    fn no_orphans_when_steps_wired_into_chain() {
        use crate::graph::{Edge, Node, NodeId, RelationType};
        let mut gl = build_loop_with(vec!["{}"]);
        let mut start = Node::task("start", "Start");
        start.immutable = true;
        gl.graph.add_node(start);
        gl.graph.add_node(Node::task("deliverable", "Deliverable"));
        gl.graph.add_node(Node::task("outline", "Outline"));
        // start → outline → deliverable (fully wired)
        gl.graph.add_edge(Edge::new("start", "outline", RelationType::LeadsTo, 0.9, "")).unwrap();
        gl.graph.add_edge(Edge::new("outline", "deliverable", RelationType::LeadsTo, 0.9, "")).unwrap();
        let orphans = gl.replay_from_anchor();
        assert!(orphans.is_empty(), "wired chain should have no orphans, got {orphans:?}");
        let _ = NodeId::from("x");
    }

    #[tokio::test]
    async fn ready_for_verify_bounces_back_when_orphans() {
        use crate::graph::{Edge, Node, RelationType};
        let mut gl = build_loop_with(vec!["{}"]);
        let mut start = Node::task("start", "Start");
        start.immutable = true;
        gl.graph.add_node(start);
        gl.graph.add_node(Node::task("deliverable", "Deliverable"));
        gl.graph.add_node(Node::task("orphan", "Orphan step"));
        gl.graph.add_edge(Edge::new("start", "deliverable", RelationType::LeadsTo, 0.9, "")).unwrap();
        // ready_for_verify with an orphan present → should NOT verify, returns Running, phase Filling.
        let state = gl.run_verify_and_maybe_repair().await.unwrap();
        assert!(matches!(state, LoopState::Running));
        assert_eq!(gl.graph_phase, GraphPhase::Filling);
    }
```

注:`run_verify_and_maybe_repair` 是私有 async 方法,测试在同模块内可调。若 `Verifier::structural_only()` 在无孤儿时会真的跑验证(可能需要 model),本测试构造了孤儿,会在验证前就 return,不触发 verifier——安全。

- [ ] **Step 2: 跑测试**

Run: `cd /home/hhhh/Graph-Centric && cargo test --lib graph_loop:: 2>&1 | tail -6`
Expected: 全绿,含 3 新测试。

- [ ] **Step 3: 全量测试**

Run: `cd /home/hhhh/Graph-Centric && cargo test --lib 2>&1 | tail -3`
Expected: 全绿。

- [ ] **Step 4: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "test(agent): orphan detection + ready_for_verify bounce-back"
```

---

## Task 6: 重建 + 重启 + 端到端验证 + 推送

- [ ] **Step 1: 重建后端**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error"; echo "exit=${PIPESTATUS[0]}"`
Expected: exit=0。

- [ ] **Step 2: 重启 serve**

```bash
cd /home/hhhh/Graph-Centric
pid=$(pgrep -f "target/debug/serve" | head -1); [ -n "$pid" ] && kill "$pid"; sleep 1
WEB_PORT=8090 setsid ./target/debug/serve > /tmp/graph-serve.log 2>&1 < /dev/null & disown
sleep 4; curl -s -o /dev/null -w "HTTP %{http_code}\n" http://localhost:8090/
```
Expected: HTTP 200。

- [ ] **Step 3: 端到端验证(pinchtab)**

跑"写一篇介绍 Rust 所有权的短文"任务,走澄清→确认→建图;等模型填充几步后查图的边结构:中间步骤节点应通过 `LeadsTo` 边串在 `start → … → deliverable` 上(非孤儿),且节点 id 是语义名(如 outline/draft-intro)而非 B1/B2。若模型加了孤儿,下一步应看到连边提示;带孤儿发 ready_for_verify 应被退回 Filling。

- [ ] **Step 4: 推送**

```bash
cd /home/hhhh/Graph-Centric
git push origin main
```

---

## 验收(整体)
- `cargo test --lib` 全绿(含孤儿检测 / 连通无孤儿 / ready_for_verify 退回 三测)。
- 建图阶段加了不连边的节点 → 下一步注入连边提示;同一批孤儿不重复刷屏。
- 带孤儿 ready_for_verify → 退回 Filling,不进验证。
- 提示词教语义 id(outline/draft-intro)+ 强制 LeadsTo 连边,无 A/D/T1/DependsOn 残留。
- 端到端:中间节点串进 start→…→deliverable 主链,id 语义化。

## 不做(YAGNI)
- 不自动替模型连边(只检测 + 提示)。
- 不改 replay_from_anchor 算法。
- 不为孤儿提示设计多级升级(去重 + 收口兜底足够)。
