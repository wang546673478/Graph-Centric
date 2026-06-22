# 反序列化后重建图索引 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** 让 `Graph` 反序列化后自动重建 `outgoing_idx`/`incoming_idx`,修复"checkpoint 加载后边索引为空 → `outgoing()` 返回空 → `path_exists` 全 false → replay 误报 start 不可达下游"的真实 bug;加 round-trip 回归单测防回归。

**Architecture:** 给 `Graph` 手写 `impl<'de> Deserialize<'de>`(替代 derive 的 Deserialize),用一个私有影子结构体 `GraphData` 反序列化普通字段,构造 `Graph` 后调已有的 `rebuild_indices()` 填充两个邻接索引。`Serialize` 保持 derive 不变。

**Tech Stack:** Rust + serde。`cargo test --lib`。

参考:spec `docs/superpowers/specs/2026-06-21-graph-index-rebuild-on-deserialize-design.md`(方案 C)。

读码确认:
- `Graph`(mod.rs:510)`#[derive(Debug, Clone, Serialize, Deserialize)]`。反序列化字段:`nodes: HashMap<NodeId,Node>`、`edges: Vec<Edge>`、`l1: L1Store`(`#[serde(default)]`)、`version: usize`、`status: GraphStatus`。`#[serde(skip)]` 字段:`outgoing_idx`、`incoming_idx`、`parent`。
- `rebuild_indices()`(:692)已存在:clear 两个索引并按 `edges` 重建。
- `persistence.rs`(:101/:114)反序列化 Checkpoint 时不调 rebuild。
- bug 链:空索引 → `outgoing()`(:618)空 → `path_exists`(graph_loop.rs:3057)false → `replay_from_anchor` 误判 → ready_for_verify 兜底(:1805)退回 Filling。

## File Structure
- Modify: `src/graph/mod.rs` — Graph 去掉 derive 的 Deserialize,手写 Deserialize(影子结构体 + rebuild_indices)+ round-trip 回归单测。

---

## Task 1: 给 Graph 手写 Deserialize(反序列化后重建索引)

**Files:** `src/graph/mod.rs`

- [ ] **Step 1: 写失败的回归测试(先行)**

在 `mod.rs` 的 `#[cfg(test)] mod tests` 加(round-trip 后索引应已重建、连通性成立):

```rust
    #[test]
    fn deserialized_graph_rebuilds_adjacency_index() {
        // start --LeadsTo--> mid --LeadsTo--> deliverable
        let mut g = Graph::new();
        let mut start = Node::task("start", "Start");
        start.immutable = true;
        g.add_node(start);
        g.add_node(Node::task("mid", "Mid"));
        g.add_node(Node::task("deliverable", "Deliverable"));
        g.add_edge(Edge::new("start", "mid", RelationType::LeadsTo, 0.99, "")).unwrap();
        g.add_edge(Edge::new("mid", "deliverable", RelationType::LeadsTo, 0.99, "")).unwrap();

        let json = serde_json::to_string(&g).unwrap();
        let restored: Graph = serde_json::from_str(&json).unwrap();

        // The adjacency index must be rebuilt on deserialize: outgoing(start)
        // is non-empty and start can reach deliverable. (Before the fix,
        // outgoing_idx was empty after deserialize → outgoing() returned
        // nothing → reachability was false.)
        assert_eq!(restored.outgoing(&NodeId::from("start")).count(), 1, "start should have 1 outgoing edge after deserialize");
        assert_eq!(restored.outgoing(&NodeId::from("mid")).count(), 1);
        // sanity: edges survived
        assert_eq!(restored.edges.len(), 2);
    }
```

注意:`outgoing()` 返回的迭代器是否有 `.count()` — 若 `outgoing` 返回 `impl Iterator`,`.count()` 可用。若返回 `&[usize]` 或别的,改成对应的非空断言(读 `outgoing` 签名确认;:618)。`Graph::new()`/`Node::task`/`Edge::new`/`RelationType`/`NodeId` 在测试模块已用过。

- [ ] **Step 2: 运行,确认失败**

Run: `cd /home/hhhh/Graph-Centric && cargo test --lib graph::tests::deserialized_graph_rebuilds_adjacency_index 2>&1 | tail -15`
Expected: FAIL —— `outgoing(start).count()` 为 0(反序列化后索引空),断言不通过。这证明 bug 存在。

- [ ] **Step 3: 去掉 derive 的 Deserialize**

把 `Graph` 的 derive(:510):
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Graph {
```
改为(移除 `Deserialize`,保留其余):
```rust
#[derive(Debug, Clone, Serialize)]
pub struct Graph {
```

- [ ] **Step 4: 手写 Deserialize(影子结构体 + rebuild_indices)**

在 `Graph` struct 定义之后(`impl Default for Graph` 之前或之后均可,放紧邻处)加:

```rust
impl<'de> serde::Deserialize<'de> for Graph {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Shadow struct mirrors Graph's serialized fields (the #[serde(skip)]
        // adjacency indexes + parent are NOT serialized). After loading the
        // plain fields we rebuild the adjacency index, so any deserialized
        // Graph (checkpoint / persistence / branch restore) has a populated
        // outgoing_idx/incoming_idx — otherwise outgoing() returns nothing
        // and reachability checks falsely report nodes unreachable.
        #[derive(serde::Deserialize)]
        struct GraphData {
            nodes: std::collections::HashMap<NodeId, Node>,
            edges: Vec<Edge>,
            #[serde(default)]
            l1: L1Store,
            version: usize,
            status: GraphStatus,
        }
        let d = GraphData::deserialize(deserializer)?;
        let mut g = Graph {
            nodes: d.nodes,
            edges: d.edges,
            l1: d.l1,
            outgoing_idx: std::collections::HashMap::new(),
            incoming_idx: std::collections::HashMap::new(),
            version: d.version,
            status: d.status,
            parent: None,
        };
        g.rebuild_indices();
        Ok(g)
    }
}
```

注意:
- 字段名/类型必须与 `Graph` 实际一致(nodes/edges/l1/version/status)。`l1` 带 `#[serde(default)]` 与原 derive 行为一致。
- 若 `rebuild_indices` 是私有 `fn`,在同模块内可调,无需改可见性。
- `HashMap` 路径按文件顶部 import 写(若已 `use std::collections::HashMap;` 则直接 `HashMap::new()`)。读文件顶部确认,用一致写法。

- [ ] **Step 5: 运行测试,确认通过**

Run: `cd /home/hhhh/Graph-Centric && cargo test --lib graph::tests::deserialized_graph_rebuilds_adjacency_index 2>&1 | tail -10`
Expected: PASS —— 反序列化后 `outgoing(start).count()==1`,索引已重建。

- [ ] **Step 6: 全量测试 + 构建**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error"; echo "build exit=${PIPESTATUS[0]}"; cargo test --lib 2>&1 | tail -3`
Expected: 构建 exit=0;测试全绿(手写 Deserialize 不破坏其它图测试)。

- [ ] **Step 7: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/graph/mod.rs
git commit -m "fix(graph): rebuild adjacency index on deserialize (fixes false start-unreachable)"
```

---

## Task 2: 端到端验证 + 推送

- [ ] **Step 1: 重启 serve**

```bash
cd /home/hhhh/Graph-Centric
pid=$(pgrep -f "target/debug/serve" | head -1); [ -n "$pid" ] && kill "$pid"; sleep 1
WEB_PORT=8090 setsid ./target/debug/serve > /tmp/graph-serve.log 2>&1 < /dev/null & disown
sleep 4; curl -s -o /dev/null -w "HTTP %{http_code}\n" http://localhost:8090/
```
Expected: HTTP 200。

- [ ] **Step 2: 验证旧 run 不再误报 / 新 run 可收口**

若 fb95e74a 那个 run 还在(它带完整主链 checkpoint),可对它的恢复路径验证:加载后 `replay_from_anchor` 应为空(不再报 start 不可达)。最简方式——跑一个新任务走到填满主链 + ready_for_verify:确认模型发 ready_for_verify 后**不再被退回 Filling**报"start 不可达"(因为索引现在重建正确,path_exists 走得通)。用 pinchtab 跑"写一篇短文"任务,填出 start→…→deliverable 链后看是否能进验证、状态推进到 Review/Done 而非卡在"start 不可达"。

- [ ] **Step 3: 推送**

```bash
cd /home/hhhh/Graph-Centric
git push origin main
```

---

## 验收
- 反序列化的 Graph 邻接索引已重建:回归单测 `deserialized_graph_rebuilds_adjacency_index` 通过。
- `cargo test --lib` 全绿;`cargo build --bin serve` exit=0。
- 端到端:带完整主链的 run 走 ready_for_verify 不再误报"start 不可达下游"、能进验证。
- 任何加载路径(checkpoint/persistence/branch/`from_*`)反序列化出的 Graph 都有正确索引(根治)。

## 不做(YAGNI)
- 不改 JSON 序列化格式(索引仍 skip,不落盘)。
- 不重建 `parent`(超范围,现有 Contains 重walk 机制不变)。
- 不改 Verifier(根因不在它)。
