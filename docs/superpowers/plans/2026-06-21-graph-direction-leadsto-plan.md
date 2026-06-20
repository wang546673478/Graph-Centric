# 图方向翻转 + LeadsTo 关系(第二期)实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** 把图的主轴从 `D--DependsOn-->A`(目标依赖开始、A 是汇点)翻成 `start--LeadsTo-->deliverable`(start 是源点、沿出边流向交付物);新增 `LeadsTo` 关系类型,锚点改名 start/deliverable;连通性/replay/cascade 方向全部校正为"从 start 流出";无环只卡 DependsOn/Contains,LeadsTo 允许环;中间步骤的边类型交给模型判断。

**Architecture:** 纯 agent/graph 核心改动。新增 `RelationType::LeadsTo` + `is_structural()`。seed 建 `start--LeadsTo-->deliverable`。`path_exists`/`replay_from_anchor`/`anchor_goal_connected` 沿结构边、方向从 start 出发。cascade 找上游=入边来源。无环集合加 Contains、用 LeadsTo 不查环。prompt 教模型按任务挑边。

**Tech Stack:** Rust(graph + agent)。`cargo test --lib` 回归兜底,逐文件核对方向。前端轻量(节点名/箭头随数据自动正确)。

参考:spec `docs/superpowers/specs/2026-06-21-graph-direction-leadsto-design.md`。**唯一不变量**:只有 `start--LeadsTo-->deliverable` 主轴固定;中间步骤连法全交模型。

读码确认的现状:
- `RelationType`(mod.rs:196)枚举 + `as_wire`(:218)/`parse_wire`(:235);`Serialize` 自定义(:253)。
- seed 建边在 step_graph(:1503-1545,用 `"A"`/`"D"` + DependsOn + "goal depends on start")和 `auto_seed_start_goal`(:2350-2363)。
- anchor 自动识别(:1553)匹配 `"a"`/`anchor`/`a-`…
- `anchor_goal_connected`(:2977)= `path_exists(d,a)||path_exists(a,d)`(双向,已宽容);goal 找 `"D"` 或任意非 immutable。
- `path_exists`(:3006)= 通用 BFS 沿 `outgoing`,无 relation 过滤。
- `replay_from_anchor`(:3037)= 节点 `path_exists(node, anchor)` 失败即孤儿(**方向要翻**)。
- cascade `dependency_predecessors_of`(:282)= `DependsOn && source==node` → target(**方向+关系要改**)。
- `outgoing`(mod.rs:600)/`incoming`(:609)都在。
- 无环:`validation.rs` `ACYCLIC_RELATIONS=&[DependsOn]`;decomposer `find_cycle_in_relation(DependsOn)`。
- `api_runs.rs:593` graph_schema `required_edge_relation: Some(DependsOn)`。
- decomposer(:313)/cascade_expand(:291)用 DependsOn 串子任务。

## File Structure
- `src/graph/mod.rs` — 加 `LeadsTo` 变体 + wire/serialize + `is_structural()`
- `src/graph/validation.rs` — `ACYCLIC_RELATIONS` 加 Contains(LeadsTo 不列)
- `src/agent/graph_loop.rs` — seed 改 start/deliverable+LeadsTo;path_exists 沿结构边;replay/anchor_goal_connected 方向翻;anchor 识别加 start;单测
- `src/agent/cascade.rs` — `dependency_predecessors_of` 改入边来源 + 认结构边
- `src/agent/decomposer.rs` / `src/agent/cascade_expand.rs` — 子任务边默认 LeadsTo
- `src/agent/proposer.rs` + `src/web/api_runs.rs` + `skills/prompts/*` — prompt 关系图谱 + schema
- `webui` — 节点名展示(轻量)

---

## Task 1: 新增 `RelationType::LeadsTo` + `is_structural()`

**Files:** `src/graph/mod.rs`

- [ ] **Step 1: 写失败测试**

在 `src/graph/mod.rs` 的 `#[cfg(test)] mod tests` 加:

```rust
    #[test]
    fn leadsto_wire_roundtrips() {
        assert_eq!(RelationType::LeadsTo.as_wire(), "LeadsTo");
        assert!(matches!(RelationType::parse_wire("LeadsTo"), RelationType::LeadsTo));
    }

    #[test]
    fn is_structural_classifies_relations() {
        assert!(RelationType::LeadsTo.is_structural());
        assert!(RelationType::DependsOn.is_structural());
        assert!(RelationType::Contains.is_structural());
        assert!(!RelationType::RevealedBy.is_structural());
        assert!(!RelationType::InvalidatedBy.is_structural());
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cd /home/hhhh/Graph-Centric && cargo test --lib graph::mod 2>&1 | tail -8` (或 `cargo build --lib`)
Expected: 编译失败 — 无 `LeadsTo` 变体 / 无 `is_structural`。

- [ ] **Step 3: 加变体**

`RelationType` enum 在 `// Behavioral` 之前(Dependency 段后)加 `LeadsTo`:

```rust
    // Dependency
    Imports,
    Exports,
    DependsOn,
    // Flow — process / sequencing ("start leads to deliverable"). May cycle.
    LeadsTo,
    // Behavioral
    Calls,
```

`as_wire` 加 `Self::LeadsTo => "LeadsTo",`(放 DependsOn 行之后)。`parse_wire` 加 `"LeadsTo" => Self::LeadsTo,`(放 DependsOn 行之后)。

- [ ] **Step 4: 加 is_structural()**

在 `impl RelationType`(parse_wire 之后)加:

```rust
    /// Structural relations participate in graph connectivity/replay
    /// traversal (start → deliverable flow, dependencies, containment).
    /// Meta-relations (provenance) do not. Used by path_exists/replay to
    /// decide which edges are walkable.
    pub fn is_structural(&self) -> bool {
        matches!(
            self,
            Self::Contains | Self::BelongsTo | Self::Imports | Self::Exports
                | Self::DependsOn | Self::LeadsTo | Self::Calls | Self::Triggers
                | Self::Reads | Self::Writes
        )
    }
```

（Serialize 自定义在 :253 — 确认它内部调 `as_wire()`;若是,LeadsTo 自动随 as_wire 正确序列化,无需改。Step 5 构建会暴露遗漏。）

- [ ] **Step 5: 测试通过**

Run: `cd /home/hhhh/Graph-Centric && cargo test --lib graph:: 2>&1 | tail -8`
Expected: 全绿,含 2 新测试。

- [ ] **Step 6: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/graph/mod.rs
git commit -m "feat(graph): add LeadsTo relation + is_structural()"
```

---

## Task 2: 无环集合加 Contains(LeadsTo 允许环)

**Files:** `src/graph/validation.rs`

- [ ] **Step 1: 改 ACYCLIC_RELATIONS**

把 `const ACYCLIC_RELATIONS: &[RelationType] = &[RelationType::DependsOn];`(:191)改为:

```rust
const ACYCLIC_RELATIONS: &[RelationType] = &[RelationType::DependsOn, RelationType::Contains];
```

（LeadsTo 故意不列入 — 流程允许回退/循环。）

- [ ] **Step 2: 加测试**

在 validation.rs 测试模块加:

```rust
    #[test]
    fn leadsto_cycle_is_allowed() {
        let mut g = Graph::new();
        g.add_node(Node::task("x", "X"));
        g.add_node(Node::task("y", "Y"));
        g.add_edge(Edge::new("x", "y", RelationType::LeadsTo, 1.0, "")).unwrap();
        g.add_edge(Edge::new("y", "x", RelationType::LeadsTo, 1.0, "")).unwrap();
        // LeadsTo cycles must NOT be reported as inconsistencies.
        let issues = check_consistency(&g);
        assert!(!issues.iter().any(|i| matches!(i, Inconsistency::Cycle { relation, .. } if *relation == RelationType::LeadsTo)));
    }
```

（确认 `check_consistency` 是该模块的公开检查入口名 — 读 validation.rs 验证;若名字不同按实际改。Node::task/Edge::new/Inconsistency 已在现有测试用过。）

- [ ] **Step 3: 测试通过**

Run: `cd /home/hhhh/Graph-Centric && cargo test --lib validation 2>&1 | tail -8`
Expected: 全绿。

- [ ] **Step 4: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/graph/validation.rs
git commit -m "feat(graph): acyclic check covers DependsOn+Contains, LeadsTo may cycle"
```

---

## Task 3: seed 改 start/deliverable + LeadsTo 方向

**Files:** `src/agent/graph_loop.rs`

- [ ] **Step 1: 改 auto_seed_start_goal**

把 `auto_seed_start_goal`(:2350)整体改为(id→start/deliverable,边→`start--LeadsTo-->deliverable`):

```rust
    fn auto_seed_start_goal(&mut self) {
        use crate::graph::{Edge, Node, NodeId, NodeKind, RelationType};
        let mut anchor =
            Node::new("start", NodeKind::Task, "start", "Start: current state / the task to accomplish");
        anchor.immutable = true;
        let goal = Node::new("deliverable", NodeKind::Task, "deliverable", "Deliverable: the desired outcome");
        self.graph.add_node(anchor);
        self.graph.add_node(goal);
        let _ = self.graph.add_edge(Edge::new(
            NodeId::from("start"),
            NodeId::from("deliverable"),
            RelationType::LeadsTo,
            0.9,
            "start leads to deliverable",
        ));
    }
```

- [ ] **Step 2: 改 step_graph 里的 seed 裁剪逻辑**

在 step_graph 的 seed 块(:1503-1545),把节点 id 和边改为 start/deliverable + LeadsTo。具体:
- `:1503-1505` 新建 goal 节点的 `id: NodeId::from("D")` / `path: "D"` → `"deliverable"`
- `:1513` `kept[0].id = NodeId::from("A")` → `NodeId::from("start")`
- `:1520` `kept[1].id = NodeId::from("D")` → `NodeId::from("deliverable")`
- `:1529-1533` 和 `:1541-1545` 的两处建边:`Edge::new(NodeId::from("D"), NodeId::from("A"), RelationType::DependsOn, ..., "goal depends on start")` → `Edge::new(NodeId::from("start"), NodeId::from("deliverable"), RelationType::LeadsTo, ..., "start leads to deliverable")`

READ :1495-1550 先看清确切结构再改(裁剪逻辑把首尾节点设为 A/D)。首节点(原 A)是 immutable anchor=start,尾节点(原 D)=deliverable。

- [ ] **Step 3: 改 anchor 自动识别**

在 :1553 的 anchor 识别块,把匹配 `"a"`/`a-`… 改为也认 `start`:

```rust
                        let id_lower = node.id.as_str().to_lowercase();
                        let is_anchor = id_lower == "start"
                            || id_lower == "a"
                            || id_lower.contains("anchor")
                            || id_lower.starts_with("start")
                            || id_lower.starts_with("a-")
                            || id_lower.starts_with("a_")
                            || id_lower.starts_with("a.")
                            || id_lower.starts_with("anchor");
```

- [ ] **Step 4: 改 auto_seed_start_goal 后续引用**

`auto_seed_start_goal` 之后(:2377)有 `.get(&NodeId::from("D"))` 取 goal summary 的逻辑(build_forced_search_items)→ 改为 `NodeId::from("deliverable")`。grep `NodeId::from("D")` / `NodeId::from("A")` 全文件,逐个改为 deliverable/start。

Run 先定位: `grep -n 'NodeId::from("A")\|NodeId::from("D")\|"A"\|"D"' src/agent/graph_loop.rs`(注意排除测试,测试在 Task 6 一起改)。

- [ ] **Step 5: 构建**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error" | head; echo exit=${PIPESTATUS[0]}`
Expected: exit=0(测试可能因 A/D 断言失败,Task 6 修;此处只要非测试代码编译通过)。

- [ ] **Step 6: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "feat(agent): seed start--LeadsTo-->deliverable (was D--DependsOn-->A)"
```

---

## Task 4: 连通性/replay 方向翻转 + 沿结构边

**Files:** `src/agent/graph_loop.rs`

- [ ] **Step 1: path_exists 只走结构边**

把 `path_exists`(:3006)的 BFS 加结构边过滤:

```rust
    fn path_exists(&self, from: &NodeId, to: &NodeId) -> bool {
        let mut seen = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(from.clone());
        seen.insert(from.clone());
        while let Some(cur) = queue.pop_front() {
            for edge in self.graph.outgoing(&cur) {
                if !edge.relation.is_structural() { continue; }
                if &edge.target == to {
                    return true;
                }
                if seen.insert(edge.target.clone()) {
                    queue.push_back(edge.target.clone());
                }
            }
        }
        false
    }
```

- [ ] **Step 2: replay_from_anchor 方向翻转**

把 `replay_from_anchor`(:3037)的孤儿判定从"节点能到锚点"翻成"锚点能到节点":

```rust
        let mut orphaned: Vec<NodeId> = self
            .graph
            .nodes
            .values()
            .filter(|n| !n.immutable && n.id != anchor)
            .filter(|n| !self.path_exists(&anchor, &n.id))  // start 能否流到该节点
            .map(|n| n.id.clone())
            .collect();
```

（仅把 `self.path_exists(&n.id, &anchor)` 改为 `self.path_exists(&anchor, &n.id)`;注释同步更新为"从 start 流向各节点"。)

- [ ] **Step 3: anchor_goal_connected 找 deliverable**

把 `anchor_goal_connected`(:2977)里找 goal 的 `"D"` 改为 `"deliverable"`:

`:2986` `if self.graph.nodes.contains_key(&NodeId::from("D"))` → `NodeId::from("deliverable")`;`:2987` `Some(NodeId::from("D"))` → `Some(NodeId::from("deliverable"))`。`:3001` 的 `path_exists(&d,&a)||path_exists(&a,&d)` 双向保留即可(已宽容,start↔deliverable 任一方向连通都算)。

- [ ] **Step 4: 构建**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error" | head; echo exit=${PIPESTATUS[0]}`
Expected: exit=0。

- [ ] **Step 5: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "feat(agent): traversal flows from start along structural edges"
```

---

## Task 5: cascade 上游 = 入边来源 + 结构边

**Files:** `src/agent/cascade.rs`

- [ ] **Step 1: 改 dependency_predecessors_of**

把(:282):

```rust
fn dependency_predecessors_of<'a>(
    graph: &'a Graph,
    node: &NodeId,
) -> Vec<(&'a Edge, &'a Node)> {
    graph
        .edges
        .iter()
        .filter(|e| e.relation == RelationType::DependsOn && e.source == *node)
        .filter_map(|e| graph.nodes.get(&e.target).map(|n| (e, n)))
        .collect()
}
```

改为(上游 = 指向本节点的入边的 source,且认结构边):

```rust
fn dependency_predecessors_of<'a>(
    graph: &'a Graph,
    node: &NodeId,
) -> Vec<(&'a Edge, &'a Node)> {
    // Upstream = nodes whose structural edge points INTO `node` (they feed
    // it). With the start→deliverable flow, an edge source→target means
    // source flows to target, so `node`'s upstream are edges with
    // target == node. Walk LeadsTo/DependsOn/Contains (structural) edges.
    graph
        .edges
        .iter()
        .filter(|e| e.relation.is_structural() && e.target == *node)
        .filter_map(|e| graph.nodes.get(&e.source).map(|n| (e, n)))
        .collect()
}
```

- [ ] **Step 2: 构建**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error" | head; echo exit=${PIPESTATUS[0]}`
Expected: exit=0。

- [ ] **Step 3: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/cascade.rs
git commit -m "feat(agent): cascade upstream = incoming structural edges (flow direction)"
```

---

## Task 6: 修正 graph_loop 既有测试的方向/命名

**Files:** `src/agent/graph_loop.rs`(测试区)

翻转方向后,既有测试里建 A/D + DependsOn + 旧方向断言会失败。逐一校正。

- [ ] **Step 1: 跑测试看哪些挂了**

Run: `cd /home/hhhh/Graph-Centric && cargo test --lib graph_loop:: 2>&1 | grep -E "FAILED|test result" | head -30`
Expected: 列出失败测试(预期:build_chain_graph、path_exists、replay、anchor_goal、seeding 等用 A/D 或旧方向的)。

- [ ] **Step 2: 改测试 helper build_chain_graph**

graph_loop 测试里有 `build_chain_graph`(之前 Gap2/3 加的)用 `A`/`D` + DependsOn 建链、方向是 D→…→A。改为 start/deliverable + LeadsTo + 正向 `start→mid→deliverable`:READ 该 helper(grep `fn build_chain_graph`),把 anchor id `"A"`→`"start"`、goal `"D"`→`"deliverable"`、relation `DependsOn`→`LeadsTo`、边方向从 `successor→predecessor` 翻成 `predecessor→successor`(start 在前),`path_exists` 断言从 `(D,A)` 改 `(start,deliverable)`。

- [ ] **Step 3: 逐个修剩余失败断言**

对每个失败测试:A→start、D→deliverable、DependsOn→LeadsTo、方向断言翻转(`path_exists(start, x)`)。包括 `replay_from_anchor_*`、`anchor_goal_connected_*`、`auto_seed_*`、`seeding_stall_*`、`confirm_sentinel_*`(后者若断言 seed 节点 id)。逐个改、逐个跑。

- [ ] **Step 4: graph_loop 测试全绿**

Run: `cd /home/hhhh/Graph-Centric && cargo test --lib graph_loop:: 2>&1 | tail -4`
Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "test(agent): update graph_loop tests for start/deliverable + LeadsTo direction"
```

---

## Task 7: decomposer / cascade_expand 子任务边 + prompt + schema

**Files:** `src/agent/decomposer.rs`, `src/agent/cascade_expand.rs`, `src/web/api_runs.rs`, `src/agent/proposer.rs`, `skills/prompts/proposer-rules.md`

- [ ] **Step 1: decomposer 子任务边改 LeadsTo**

`decomposer.rs:313` 建子任务边的 `RelationType::DependsOn` → `RelationType::LeadsTo`。`:322` 的 `find_cycle_in_relation(RelationType::DependsOn)` → 保留对 DependsOn 查环,**另外不要**对 LeadsTo 查环(子任务现在是 LeadsTo,流程允许环,所以这行无环检测改为不阻塞 LeadsTo;若该检测是硬性拦截,改成只在子图含 DependsOn 时查)。READ :305-330 看清逻辑再改:目标是子任务默认 LeadsTo 串接、不因正常流程被无环拦死。

- [ ] **Step 2: cascade_expand 子图边改 LeadsTo**

`cascade_expand.rs:291` 的 `RelationType::DependsOn` → `RelationType::LeadsTo`。prompt 字符串(:83/:93/:356 提到 "DependsOn")改为说明可用 LeadsTo(流程)或 DependsOn(依赖),示例边用 LeadsTo。

- [ ] **Step 3: api_runs graph_schema**

`api_runs.rs:593` `required_edge_relation: Some(RelationType::DependsOn)` → `Some(RelationType::LeadsTo)`(heartbeat schema 主链要求 LeadsTo)。同文件 prompt 文案(:386/:390 "relation 固定为 DependsOn")→ 改为"主链用 LeadsTo;中间步骤按需选 LeadsTo/DependsOn/Contains"。

- [ ] **Step 4: proposer + prompt 关系图谱**

`skills/prompts/proposer-rules.md` 加一段关系图谱指引(放在 step schemas 附近):

```markdown
## 关系类型(建边时按任务判断)
- `LeadsTo`:流程/步骤流向(先做 X 再做 Y)。start→deliverable 主链必用此。可有环(流程回退/循环)。
- `DependsOn`:真正的依赖(B 必须先存在/完成,A 才能工作)。无环。
- `Contains`:层级包含(节点展开成子节点)。无环。
先判断任务类型:线性任务(如写文档)→ 纯 LeadsTo;系统构建 → 依赖用 DependsOn、流程用 LeadsTo。
```

`proposer.rs` 的 `propose_patch` 工具 schema 里 relation 的 enum 描述加上 `LeadsTo`(若 Task 之前的 schema 列了 relation 枚举)。

- [ ] **Step 5: 构建 + 全量测试**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error"; echo "exit=${PIPESTATUS[0]}"; cargo test --lib 2>&1 | tail -3`
Expected: 构建 exit=0;测试全绿(decomposer/cascade_expand 测试若断言 DependsOn 也要在此 Task 同步改)。

- [ ] **Step 6: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/decomposer.rs src/agent/cascade_expand.rs src/web/api_runs.rs src/agent/proposer.rs skills/prompts/proposer-rules.md
git commit -m "feat(agent): subtask edges default LeadsTo; prompt relation guidance; schema LeadsTo"
```

---

## Task 8: 前端节点名 + 端到端验证 + 推送

**Files:** `webui`(轻量,若有 A/D 硬编码)

- [ ] **Step 1: 查前端是否硬编码 A/D**

Run: `cd /home/hhhh/Graph-Centric && grep -rnE "'A'|'D'|\"A\"|\"D\"|anchor.*A|goal.*D" webui/src | grep -iE "node|anchor|goal|seed" | head`
Expected: 多半无硬编码(图渲染按数据 id);若有把锚点名写死 A/D 的地方,改为不假设具体 id(用 `node.immutable` 找 start)。无命中则跳过。

- [ ] **Step 2: 重建后端 + 前端**

Run:
```bash
cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error"; echo "serve exit=${PIPESTATUS[0]}"
cd /home/hhhh/Graph-Centric/webui && npm run build 2>&1 | tail -1
```
Expected: serve exit=0;前端 `✓ built`。

- [ ] **Step 3: 重启 serve**

```bash
cd /home/hhhh/Graph-Centric
pid=$(pgrep -f "target/debug/serve" | head -1); [ -n "$pid" ] && kill "$pid"; sleep 1
WEB_PORT=8090 setsid ./target/debug/serve > /tmp/graph-serve.log 2>&1 < /dev/null & disown
sleep 4; curl -s -o /dev/null -w "HTTP %{http_code}\n" http://localhost:8090/
```
Expected: HTTP 200。

- [ ] **Step 4: 端到端验证(pinchtab)**

跑一个任务(走完澄清→确认→建图),确认图里出现 `start` 和 `deliverable` 节点、边方向是 `start → … → deliverable`(箭头从 start 指出),且节点能正常生长。

- [ ] **Step 5: 推送**

```bash
cd /home/hhhh/Graph-Centric
git push origin main
```

---

## 验收(整体)
- `cargo test --lib` 全绿(LeadsTo 往返 / is_structural / LeadsTo 可环 / 各方向断言)。
- seed 产出 `start --LeadsTo--> deliverable`(start immutable anchor、source 端)。
- 连通性/replay 从 start 沿结构边流向各节点;cascade 上游=入边来源。
- 无环只卡 DependsOn/Contains;LeadsTo 允许环。
- decomposer/cascade_expand 子任务默认 LeadsTo;prompt 教模型按任务选边。
- 端到端:图正向流 `start→…→deliverable`,箭头方向正确。

## 不做(YAGNI)
- LeadsTo 环的复杂语义(循环次数上限等)——replay 的 visited 集合防死循环足够。
- 旧 checkpoint 数据兼容层——无正式持久化数据。
- 前端关系类型图例大改——节点名 + 箭头方向随数据自动正确。
