# 冗余直连边:模型删除 + 持续监控提醒 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** 当 start 与 deliverable 之间已有经过中间节点的主链时,检测并每轮提醒模型删掉 seed 残留的 `start→deliverable` 直连边(用 remove_edge_indices),让主链唯一;filling 提示词也加引导。

**Architecture:** 在 `graph_loop.rs` 的 patch 应用分支、孤儿检查块之后,加一个"冗余直连边"检测:直连边存在 + 存在绕开它的更长路径 → 注入删边提示(每轮都注入,不去重)。新增私有方法 `redundant_direct_edge_index()` 判定 + 返回边索引。filling hint 补一句。

**Tech Stack:** Rust。`cargo test --lib`。

参考:spec `docs/superpowers/specs/2026-06-21-redundant-direct-edge-design.md`(方案 A + 持续监控,不去重)。

读码确认:
- 孤儿检查块在 graph_loop.rs:1670-1697(Filling/Expanding,patch 应用后)。冗余边检测紧随其后。
- `Edge` 字段:`source: NodeId`、`target: NodeId`、`relation: RelationType`(+confidence/evidence/history)。
- `path_exists(&self, from, to)`(:3057)沿结构边 BFS;`outgoing()` 已修复(反序列化重建索引)。
- `build_filling_hint()` 在 :2375。
- anchor = `n.immutable` 的节点;goal = id "deliverable"。
- GraphPatch 已支持 `remove_edge_indices: Vec<usize>`,模型能用。

## File Structure
- Modify: `src/agent/graph_loop.rs` — 新增 `redundant_direct_edge_index()` 方法 + patch 后检测/提醒 + filling hint 补充 + 单测。

---

## Task 1: 新增冗余直连边判定方法 + 单测

**Files:** `src/agent/graph_loop.rs`

- [ ] **Step 1: 写失败测试**

在测试模块加(复用现有 helper):

```rust
    #[test]
    fn redundant_direct_edge_detected_with_longer_path() {
        use crate::graph::{Edge, Node, RelationType};
        let mut gl = build_loop_with(vec!["{}"]);
        let mut start = Node::task("start", "Start");
        start.immutable = true;
        gl.graph.add_node(start);
        gl.graph.add_node(Node::task("deliverable", "Deliverable"));
        gl.graph.add_node(Node::task("mid", "Mid"));
        // direct edge (index 0) + longer path start→mid→deliverable
        gl.graph.add_edge(Edge::new("start", "deliverable", RelationType::LeadsTo, 0.9, "")).unwrap();
        gl.graph.add_edge(Edge::new("start", "mid", RelationType::LeadsTo, 0.9, "")).unwrap();
        gl.graph.add_edge(Edge::new("mid", "deliverable", RelationType::LeadsTo, 0.9, "")).unwrap();
        // The direct start→deliverable edge is redundant (bypasses mid).
        assert_eq!(gl.redundant_direct_edge_index(), Some(0));
    }

    #[test]
    fn no_redundant_edge_when_direct_is_only_path() {
        use crate::graph::{Edge, Node, RelationType};
        let mut gl = build_loop_with(vec!["{}"]);
        let mut start = Node::task("start", "Start");
        start.immutable = true;
        gl.graph.add_node(start);
        gl.graph.add_node(Node::task("deliverable", "Deliverable"));
        gl.graph.add_edge(Edge::new("start", "deliverable", RelationType::LeadsTo, 0.9, "")).unwrap();
        // Only the direct edge exists — it's legitimate, not redundant.
        assert_eq!(gl.redundant_direct_edge_index(), None);
    }

    #[test]
    fn no_redundant_edge_when_direct_absent() {
        use crate::graph::{Edge, Node, RelationType};
        let mut gl = build_loop_with(vec!["{}"]);
        let mut start = Node::task("start", "Start");
        start.immutable = true;
        gl.graph.add_node(start);
        gl.graph.add_node(Node::task("deliverable", "Deliverable"));
        gl.graph.add_node(Node::task("mid", "Mid"));
        // chain only, no direct edge
        gl.graph.add_edge(Edge::new("start", "mid", RelationType::LeadsTo, 0.9, "")).unwrap();
        gl.graph.add_edge(Edge::new("mid", "deliverable", RelationType::LeadsTo, 0.9, "")).unwrap();
        assert_eq!(gl.redundant_direct_edge_index(), None);
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cd /home/hhhh/Graph-Centric && cargo test --lib graph_loop::tests::redundant 2>&1 | tail -10`
Expected: 编译失败 —— 无 `redundant_direct_edge_index` 方法。

- [ ] **Step 3: 实现方法**

在 `replay_from_anchor` 附近(同一 impl 块,:3037 区域)加:

```rust
    /// If a direct `start → deliverable` edge exists AND there's also a
    /// longer path from start to deliverable through ≥1 intermediate node,
    /// the direct edge is redundant (it bypasses all the steps). Returns
    /// that edge's index in `self.graph.edges`, else None.
    fn redundant_direct_edge_index(&self) -> Option<usize> {
        let anchor = self.graph.nodes.values().find(|n| n.immutable).map(|n| n.id.clone())?;
        let goal = if self.graph.nodes.contains_key(&NodeId::from("deliverable")) {
            NodeId::from("deliverable")
        } else {
            self.graph.nodes.values().find(|n| !n.immutable).map(|n| n.id.clone())?
        };
        if anchor == goal {
            return None;
        }
        // Find a direct anchor→goal edge.
        let direct_idx = self
            .graph
            .edges
            .iter()
            .position(|e| e.source == anchor && e.target == goal)?;
        // Is there a longer path anchor→…→goal that goes through an
        // intermediate node (i.e. anchor reaches some mid != goal, and that
        // mid reaches goal)? If so the direct edge is redundant.
        let has_longer_path = self.graph.nodes.keys().any(|mid| {
            *mid != anchor
                && *mid != goal
                && self.path_exists(&anchor, mid)
                && self.path_exists(mid, &goal)
        });
        if has_longer_path { Some(direct_idx) } else { None }
    }
```

注:`path_exists` 走结构边 BFS、含直连边也含其它边;只要存在任一中间节点 mid 满足 anchor→mid 且 mid→goal,就说明有绕开直连的更长路径。`NodeId` 在作用域。

- [ ] **Step 4: 测试通过**

Run: `cd /home/hhhh/Graph-Centric && cargo test --lib graph_loop::tests::redundant 2>&1 | tail -10` 以及 `... no_redundant 2>&1`
Expected: 3 个测试全过(用 `cargo test --lib graph_loop:: 2>&1 | tail` 跑全部确认)。

- [ ] **Step 5: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "feat(agent): detect redundant start→deliverable direct edge"
```

---

## Task 2: patch 后每轮提醒删冗余边 + filling hint

**Files:** `src/agent/graph_loop.rs`

- [ ] **Step 1: 孤儿检查块之后加冗余边提醒**

在 :1697 孤儿检查块的闭合 `}` 之后(仍在 Filling/Expanding 的 patch 应用分支内),加:

```rust
                        // Redundant direct-edge monitor: once steps exist
                        // between start and deliverable, the seed's direct
                        // start→deliverable edge bypasses them. Remind the
                        // model to delete it EVERY round (no dedup) until it's
                        // gone — the main chain must be the single path.
                        if matches!(self.graph_phase, GraphPhase::Filling | GraphPhase::Expanding) {
                            if let Some(idx) = self.redundant_direct_edge_index() {
                                self.conversation.add_user(format!(
                                    "⚠️ A direct start→deliverable edge (index {idx}) still \
                                     exists and bypasses all the intermediate steps. The main \
                                     chain must be the single path start → step → … → \
                                     deliverable. Emit a `propose_patch` with \
                                     `remove_edge_indices: [{idx}]` to delete this direct edge."
                                ));
                            }
                        }
```

(注:这是独立的 `if`,紧跟孤儿检查的 `if` 之后,同一 patch 应用分支内。两者都在 Filling/Expanding 时各自判断。)

- [ ] **Step 2: build_filling_hint 补一句**

在 `build_filling_hint()` 返回的提示字符串末尾(rules 列表里)加一条关于删直连边的引导。找到 `build_filling_hint` 里 format! 的 rules 文本,在"every step node MUST sit on the path"那条之后加:
```
             - When steps are wired between start and deliverable, delete the \
             original direct start→deliverable edge via `remove_edge_indices` \
             so the main chain is the single path.\n\
```
(读 build_filling_hint 现有 format! 结构,把这条插进 rules 列表中,保持 `\` 续行风格一致。)

- [ ] **Step 3: 构建 + 全量测试**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error"; echo "exit=${PIPESTATUS[0]}"; cargo test --lib 2>&1 | tail -3`
Expected: 构建 exit=0;测试全绿。

- [ ] **Step 4: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "feat(agent): persistently remind model to delete redundant direct edge"
```

---

## Task 3: 重建 + 重启 + 端到端 + 推送

- [ ] **Step 1: 重启 serve**

```bash
cd /home/hhhh/Graph-Centric
pid=$(pgrep -f "target/debug/serve" | head -1); [ -n "$pid" ] && kill "$pid"; sleep 1
WEB_PORT=8090 setsid ./target/debug/serve > /tmp/graph-serve.log 2>&1 < /dev/null & disown
sleep 4; curl -s -o /dev/null -w "HTTP %{http_code}\n" http://localhost:8090/
```
Expected: HTTP 200。

- [ ] **Step 2: 端到端(pinchtab)**

跑一个会产生多步骤的任务(如"写一篇介绍 X 的短文"),走澄清→建图→填出 start→步骤→…→deliverable 主链。确认:① 当直连边与主链并存时,对话出现删直连边的提示(给出索引);② 模型据提示发 remove_edge_indices 删边;③ 最终图里 `start→deliverable` 直连边消失,只剩 start→步骤→…→deliverable 唯一主链。可用 pinchtab eval 查 `cy.edges()` 确认无 start→deliverable 直连边。

- [ ] **Step 3: 推送**

```bash
cd /home/hhhh/Graph-Centric
git push origin main
```

---

## 验收
- `redundant_direct_edge_index` 单测 3 个全过(有更长路径→返回索引;只有直连→None;无直连→None)。
- `cargo test --lib` 全绿;`cargo build --bin serve` exit=0。
- 建图阶段直连边冗余时,每轮提醒删除(不去重),直到删掉。
- filling hint 含删直连边引导。
- 端到端:最终主链唯一,无绕过步骤的 start→deliverable 直连边。

## 不做(YAGNI)
- 不系统自动删边(交模型删 + 监控)。
- 不查自环/重复边等其它形态(留待后续)。
- 不改 seed(直连边仍在 seed 建,靠模型 + 监控清)。
