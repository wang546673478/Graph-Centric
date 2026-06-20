# WebUI P2b — 后端 GraphPatch 增量事件 + 实时构建动画 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** 后端在每步后除全量快照外,额外发一个 `GraphPatch` 增量事件(新增/删除的节点与边);前端据此让关系图"实时生长"——新节点淡入/弹出、删除的节点淡出,呼应"图即计划"的构建过程。

**Architecture:** 增量在**web 层**计算(对比新图与 session 已存的 `last_graph`),不碰 agent 核心——零侵入、低风险。后端加 `RunEvent::GraphPatch` 变体 + `emit_graph_patch` 方法,在 `drive_run` 每步 emit。前端 `useRunSocket` 处理 `graph_patch` 事件,维护一个"最近变更"标记;2D 面板改为增量 add/remove(替代现在的整图重建)以获得入场/离场动画;3D 面板已有按 id 增量 + scale 弹入,补上删除淡出。

**Tech Stack:** Rust(axum/serde,web 层)+ Vue 3 + Cytoscape(2D)+ Three.js(3D)。后端用 `cargo test --lib`,前端用 `npm run build`。

参考:spec `docs/superpowers/specs/2026-06-20-webui-redesign-design.md`;P2a 已完成(图为主舞台 + 主题感知配色)。

读码确认的现状:
- `RunEvent`(events.rs:13)只有全量 `GraphSnapshot`;`NodeDto`/`EdgeDto` 已有。
- `RunSession::emit_graph_snapshot`(run_session.rs:96)在更新 `last_graph` 前持有旧图——是天然的 diff 来源。
- `drive_run`(api_runs.rs:727)每步调一次 `emit_graph_snapshot`。
- 前端 `useRunSocket`/RunView 已处理 `graph`/`graph_snapshot`;2D 面板 `updateGraph` 现在是 `cy.elements().remove(); cy.add(...)`(整图重建,无动画);3D 面板已按 id diff + scale 弹入。

## File Structure
- Modify: `src/web/events.rs` — 加 `GraphPatch` 变体 + event_name 分支 + 测试
- Modify: `src/web/run_session.rs` — 加 `emit_graph_patch`(diff last_graph vs new)
- Modify: `src/web/api_runs.rs` — drive_run 每步先 emit patch 再 emit snapshot
- Modify: `webui/src/composables/useRunSocket.ts` — `WSEvent` 加 patch 字段 + 类型
- Modify: `webui/src/components/run/RunView.vue` — 处理 `graph_patch`,维护 recentlyAdded 标记并传给图面板
- Modify: `webui/src/components/graph/GraphPanel.vue` — 增量 add/remove + 入场动画
- Modify: `webui/src/components/graph/GraphPanel3D.vue` — 删除淡出(入场已有)

---

## Task 1: 后端 GraphPatch 事件类型

**Files:**
- Modify: `src/web/events.rs`

- [ ] **Step 1: 写失败测试(event_name + 序列化)**

在 `src/web/events.rs` 的 `#[cfg(test)] mod tests` 内加两个测试:

```rust
    #[test]
    fn graph_patch_event_name_is_graph_patch() {
        let e = RunEvent::GraphPatch {
            added_nodes: vec![],
            removed_node_ids: vec![],
            added_edges: vec![],
            removed_edges: vec![],
            replaced: false,
        };
        assert_eq!(e.event_name(), "graph_patch");
    }

    #[test]
    fn graph_patch_serializes_with_type() {
        let e = RunEvent::GraphPatch {
            added_nodes: vec![NodeDto { id: "a".into(), kind: "Task".into(), summary: "A".into(), l1: None, l1_confidence: None }],
            removed_node_ids: vec!["old".into()],
            added_edges: vec![],
            removed_edges: vec![],
            replaced: true,
        };
        let v: serde_json::Value = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], "graph_patch");
        assert_eq!(v["data"]["added_nodes"][0]["id"], "a");
        assert_eq!(v["data"]["removed_node_ids"][0], "old");
        assert_eq!(v["data"]["replaced"], true);
    }
```

- [ ] **Step 2: 运行测试,确认失败**

Run: `cd /home/hhhh/Graph-Centric && cargo test --lib web::events:: 2>&1 | tail -15`
Expected: 编译失败 —— `no variant named GraphPatch`。

- [ ] **Step 3: 加 GraphPatch 变体**

在 `RunEvent` enum 里,`GraphSnapshot { ... }` 那一行之后插入:

```rust
    /// Incremental graph change since the previous step. Drives the
    /// real-time build animation in the UI (new nodes fade/scale in,
    /// removed nodes fade out). `replaced` is a heuristic flag: true when
    /// this step both removed and added nodes (a likely failure-replan),
    /// so the UI can flash the change instead of a plain fade.
    GraphPatch {
        added_nodes: Vec<NodeDto>,
        removed_node_ids: Vec<String>,
        added_edges: Vec<EdgeDto>,
        removed_edges: Vec<EdgeDto>,
        replaced: bool,
    },
```

在 `event_name` 的 match 里,`Self::GraphSnapshot { .. } => "graph",` 之后插入:

```rust
            Self::GraphPatch { .. } => "graph_patch",
```

- [ ] **Step 4: 运行测试,确认通过**

Run: `cd /home/hhhh/Graph-Centric && cargo test --lib web::events:: 2>&1 | tail -15`
Expected: 全部通过(含两个新测试)。

- [ ] **Step 5: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/web/events.rs
git commit -m "feat(web): add GraphPatch incremental graph event"
```

---

## Task 2: emit_graph_patch(diff 实现)

**Files:**
- Modify: `src/web/run_session.rs`

`emit_graph_snapshot` 在更新 `last_graph` 前持有旧图。新增 `emit_graph_patch`:对比 `last_graph`(旧)与传入 `graph`(新),算出新增/删除的节点与边,emit `GraphPatch`。**不更新** `last_graph`(由随后的 `emit_graph_snapshot` 更新),且当无变化时不 emit(避免噪声)。

- [ ] **Step 1: 写 emit_graph_patch**

在 `src/web/run_session.rs` 的 `emit_graph_snapshot` 方法**之后**,加:

```rust
    /// Diff the current graph against the last snapshot and emit a
    /// `GraphPatch` with the added/removed nodes and edges. No-op when
    /// nothing changed. Does NOT update `last_graph` — the caller's
    /// subsequent `emit_graph_snapshot` does that. Edges are identified
    /// by (source, target, relation) since they have no id.
    pub async fn emit_graph_patch(&self, graph: &Graph) {
        use std::collections::HashSet;
        let prev = self.last_graph.read().await.clone();

        let prev_node_ids: HashSet<String> =
            prev.nodes.keys().map(|k| k.to_string()).collect();
        let new_node_ids: HashSet<String> =
            graph.nodes.keys().map(|k| k.to_string()).collect();

        let added_nodes: Vec<NodeDto> = graph
            .nodes
            .values()
            .filter(|n| !prev_node_ids.contains(&n.id.to_string()))
            .map(|n| NodeDto::from_node(n, graph.l1.get(&n.id)))
            .collect();
        let removed_node_ids: Vec<String> = prev_node_ids
            .iter()
            .filter(|id| !new_node_ids.contains(*id))
            .cloned()
            .collect();

        let edge_key = |e: &crate::graph::Edge| {
            format!("{}->{}:{:?}", e.source, e.target, e.relation)
        };
        let prev_edge_keys: HashSet<String> = prev.edges.iter().map(edge_key).collect();
        let new_edge_keys: HashSet<String> = graph.edges.iter().map(edge_key).collect();

        let added_edges: Vec<EdgeDto> = graph
            .edges
            .iter()
            .filter(|e| !prev_edge_keys.contains(&edge_key(e)))
            .map(EdgeDto::from_edge)
            .collect();
        let removed_edges: Vec<EdgeDto> = prev
            .edges
            .iter()
            .filter(|e| !new_edge_keys.contains(&edge_key(e)))
            .map(EdgeDto::from_edge)
            .collect();

        if added_nodes.is_empty()
            && removed_node_ids.is_empty()
            && added_edges.is_empty()
            && removed_edges.is_empty()
        {
            return; // nothing changed; don't spam the UI
        }

        // Heuristic: removed + added nodes in the same step is a likely
        // failure-replan replacement.
        let replaced = !removed_node_ids.is_empty() && !added_nodes.is_empty();

        self.emit(RunEvent::GraphPatch {
            added_nodes,
            removed_node_ids,
            added_edges,
            removed_edges,
            replaced,
        });
    }
```

- [ ] **Step 2: 确认 imports 充足**

`run_session.rs` 已 use `RunEvent`、`NodeDto`、`EdgeDto`、`Graph`(`emit_graph_snapshot` 已用到)。若编译报 `Edge` 未导入,本方法用的是全限定 `crate::graph::Edge`,无需新增 use。

- [ ] **Step 3: 构建验证**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error" | head; echo "exit=${PIPESTATUS[0]}"`
Expected: 无 error,exit=0。

- [ ] **Step 4: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/web/run_session.rs
git commit -m "feat(web): emit_graph_patch diffs against last snapshot"
```

---

## Task 3: drive_run 每步发 patch

**Files:**
- Modify: `src/web/api_runs.rs`

`drive_run` 在 api_runs.rs:727 调 `session.emit_graph_snapshot(&gl.graph).await;`。在它**之前**插入 patch emit(顺序关键:patch 先 diff 旧图,snapshot 再更新旧图)。

- [ ] **Step 1: 在快照前插入 patch emit**

把这一行:
```rust
        session.emit_graph_snapshot(&gl.graph).await;
```
替换为:
```rust
        // Emit the incremental diff BEFORE the snapshot — patch diffs
        // against the previous last_graph, then the snapshot updates it.
        session.emit_graph_patch(&gl.graph).await;
        session.emit_graph_snapshot(&gl.graph).await;
```

- [ ] **Step 2: 构建验证**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error" | head; echo "exit=${PIPESTATUS[0]}"`
Expected: 无 error。

- [ ] **Step 3: 全量测试**

Run: `cd /home/hhhh/Graph-Centric && cargo test --lib 2>&1 | tail -3`
Expected: 全绿。

- [ ] **Step 4: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/web/api_runs.rs
git commit -m "feat(web): emit graph patch before snapshot each step"
```

---

## Task 4: 前端接收 graph_patch 事件

**Files:**
- Modify: `webui/src/composables/useRunSocket.ts`
- Modify: `webui/src/components/run/RunView.vue`

- [ ] **Step 1: WSEvent 加 patch 字段**

在 `useRunSocket.ts` 的 `WSEvent` interface 里,`nodes?: any[]; edges?: any[]` 那行之后加:

```typescript
  added_nodes?: any[]; removed_node_ids?: string[]
  added_edges?: any[]; removed_edges?: any[]; replaced?: boolean
```

- [ ] **Step 2: RunView 给 store 加 recentlyChanged 标记并处理事件**

在 `webui/src/components/run/RunView.vue` 的 `connectToRun` 内的 `switch (e.type)`,在 `case 'graph': case 'graph_snapshot': ...` 之后加一个 case。它把新增节点 id 记进一个模块级响应式集合,供图面板做入场高亮;snapshot 仍负责权威全量数据(patch 只驱动动画标记)。

先在 `<script setup>` 顶部(`const tab = ...` 附近)加一个响应式标记:
```typescript
import { reactive as vreactive } from 'vue'
const graphFx = vreactive<{ added: string[]; removed: string[]; replaced: boolean; ts: number }>(
  { added: [], removed: [], replaced: false, ts: 0 },
)
```
(若 `reactive` 已从 vue 导入,可直接用,不必再 import;否则用上面的别名。)

在 switch 中 `case 'graph': case 'graph_snapshot':` 块之后加:
```typescript
      case 'graph_patch':
        graphFx.added = d.added_nodes?.map((n: any) => n.id) || []
        graphFx.removed = d.removed_node_ids || []
        graphFx.replaced = !!d.replaced
        graphFx.ts = Date.now()
        break
```

- [ ] **Step 3: 把 graphFx 作为 prop 传给图面板**

在 template 里给两个图面板加 `:fx="graphFx"`:
```html
        <GraphPanel v-if="graphView === '2d'" :key="(activeRunId || 'empty') + '-2d'"
          :nodes="nodes" :edges="edges" :scopeNodeIds="scopeNodeIds" :fx="graphFx" />
        <GraphPanel3D v-else :key="(activeRunId || 'empty') + '-3d'"
          :nodes="nodes" :edges="edges" :scopeNodeIds="scopeNodeIds" :fx="graphFx" />
```

- [ ] **Step 4: 构建验证**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功(图面板的 `fx` prop 在 Task 5/6 声明前,Vue 对未声明 prop 容忍——会落到 attrs,不报错;若 TS 报未知 prop,在 Task 5/6 补 defineProps 后消除)。

- [ ] **Step 5: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/composables/useRunSocket.ts webui/src/components/run/RunView.vue
git commit -m "feat(webui): receive graph_patch event and track changed nodes"
```

---

## Task 5: 2D 面板增量动画

**Files:**
- Modify: `webui/src/components/graph/GraphPanel.vue`

当前 `updateGraph` 用 `cy.elements().remove(); cy.add(...)` 整图重建——无动画。改为:声明 `fx` prop;`updateGraph` 改为增量(只 add 新增、remove 消失的元素),对新增节点加 `flash` class 触发入场动画。

- [ ] **Step 1: 声明 fx prop**

把 `defineProps<{ nodes: any[]; edges: any[]; scopeNodeIds?: string[] }>()` 改为:
```typescript
const props = defineProps<{ nodes: any[]; edges: any[]; scopeNodeIds?: string[]; fx?: { added: string[]; removed: string[]; replaced: boolean; ts: number } }>()
```

- [ ] **Step 2: updateGraph 改增量 + 入场动画**

把现有 `function updateGraph() { ... }` 整体替换为(用 cytoscape 的增量 add/remove,而非整图重建;新增节点先小后弹大):

```typescript
function updateGraph() {
  if (!cy) return
  const wantNodeIds = new Set(visibleNodes.value.map((n: any) => n.id))
  const wantEdgeKeys = new Map<string, any>()
  visibleEdges.value.forEach((e: any, i: number) => wantEdgeKeys.set(`${e.source}->${e.target}`, { e, i }))

  // Remove nodes/edges no longer present.
  cy.nodes().forEach((n: any) => { if (!wantNodeIds.has(n.id())) n.remove() })
  cy.edges().forEach((ed: any) => {
    const k = `${ed.data('source')}->${ed.data('target')}`
    if (!wantEdgeKeys.has(k)) ed.remove()
  })

  // Add new nodes.
  const addedIds = new Set(props.fx?.added || [])
  for (const n of visibleNodes.value) {
    if (cy.getElementById(n.id).nonempty()) {
      // Update class for scope/complex/selected changes.
      const el = cy.getElementById(n.id)
      el.classes([
        scopeSet.value.has(n.id) ? 'in-scope' : '',
        hasChildren(n.id) ? 'complex' : '',
        selectedNode.value?.id === n.id ? 'selected' : '',
      ].filter(Boolean).join(' '))
      continue
    }
    const el = cy.add({
      group: 'nodes',
      data: { id: n.id, label: n.summary || n.id },
      classes: [
        scopeSet.value.has(n.id) ? 'in-scope' : '',
        hasChildren(n.id) ? 'complex' : '',
        selectedNode.value?.id === n.id ? 'selected' : '',
      ].filter(Boolean).join(' '),
    })
    // Entrance animation: fade + scale in (flash if from a failure-replan).
    if (addedIds.has(n.id)) {
      el.style('opacity', 0)
      el.animate({ style: { opacity: 1 } }, { duration: props.fx?.replaced ? 120 : 300 })
    }
  }

  // Add new edges.
  for (const [k, { e, i }] of wantEdgeKeys) {
    if (cy.getElementById(`e${i}`).nonempty()) continue
    const exists = cy.edges().some((ed: any) => `${ed.data('source')}->${ed.data('target')}` === k)
    if (exists) continue
    cy.add({
      group: 'edges',
      data: { id: `e${i}`, source: e.source, target: e.target, label: e.relation },
      classes: [
        scopeSet.value.has(e.source) && scopeSet.value.has(e.target) ? 'in-scope' : '',
        e.relation === 'Contains' ? 'Contains' : '',
      ].filter(Boolean).join(' '),
    })
  }

  cy.layout({ name: 'cose', animate: true, idealEdgeLength: 100, nodeRepulsion: 6000 }).run()
}
```

- [ ] **Step 3: watch fx 触发更新**

现有有 `watch(() => [props.nodes, props.edges, props.scopeNodeIds, breadcrumb.value], updateGraph, { deep: true })`。把它改为也监听 `props.fx?.ts`:
```typescript
watch(() => [props.nodes, props.edges, props.scopeNodeIds, breadcrumb.value, props.fx?.ts], updateGraph, { deep: true })
```

- [ ] **Step 4: 构建验证**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功,无 TS 错误。

- [ ] **Step 5: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/components/graph/GraphPanel.vue
git commit -m "feat(webui): incremental 2D graph updates with entrance animation"
```

---

## Task 6: 3D 面板删除淡出 + fx prop

**Files:**
- Modify: `webui/src/components/graph/GraphPanel3D.vue`

3D 面板已有按 id 增量 + scale 弹入(`animLoop` 里 `animS`)。本任务只:声明 `fx` prop(消除未知 prop 警告)。删除已由现有 `updateGraph` 的 stale 清理处理(直接 remove);P2b 不强求 3D 淡出动画(scale-in 已是主要观感),保持简单。

- [ ] **Step 1: 声明 fx prop**

把 `const props = defineProps<{ nodes: any[]; edges: any[]; scopeNodeIds?: string[] }>()` 改为:
```typescript
const props = defineProps<{ nodes: any[]; edges: any[]; scopeNodeIds?: string[]; fx?: { added: string[]; removed: string[]; replaced: boolean; ts: number } }>()
```
(`props` 当前已用于 `props.nodes` 等,变量名不变。)

- [ ] **Step 2: 构建验证**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功。

- [ ] **Step 3: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/components/graph/GraphPanel3D.vue
git commit -m "feat(webui): declare fx prop on 3D graph panel"
```

---

## Task 7: 端到端验证 + 推送

- [ ] **Step 1: 重启后端(带新代码)**

```bash
cd /home/hhhh/Graph-Centric
pid=$(pgrep -f "target/debug/serve" | head -1); [ -n "$pid" ] && kill "$pid"; sleep 1
WEB_PORT=8090 setsid ./target/debug/serve > /tmp/graph-serve.log 2>&1 < /dev/null & disown
sleep 4; curl -s -o /dev/null -w "HTTP %{http_code}\n" http://localhost:8090/
```
Expected: HTTP 200。

- [ ] **Step 2: 目视验证**

前端 http://192.168.31.228:5173,新建一个任务并观察(默认 2D 面板):
- 随 agent 每步推进,新节点**淡入浮现**而不是整图闪烁重排。
- 节点被删除/替换时从图上消失。
- 失败重规划那一步(removed+added 同时),入场更快(flash)。
- 切 3D 仍正常(节点 scale 弹入)。
- 深浅主题、拖拽分隔条、钻取等 P2a 功能无回归。

- [ ] **Step 3: 推送**

```bash
cd /home/hhhh/Graph-Centric
git push origin main
```

---

## 验收(P2b 整体)
- 后端 `cargo test --lib` 全绿;新增 GraphPatch 序列化/event_name 测试通过。
- 后端每步发 `graph_patch`(无变化不发);diff 正确反映新增/删除节点与边。
- 前端 2D 面板增量更新:新节点淡入、删除消失,不再整图重建闪烁;失败替换 flash。
- 3D 面板接受 fx prop 不报错,入场动画(scale-in)保持。
- P2a 全部功能无回归(主题、分隔条、2D/3D 切换、钻取)。

## 不做(留 P3 / 后续)
- 把增量事件下沉到 agent 核心(graph_loop 直接 emit)——本期用 web 层 diff,等价且零侵入。
- 3D 删除淡出动画、活动节点发光脉冲——超出 P2b 最小可观感;如需要,P3 增强。
- 对话渲染增强 / 顾问面板 / 仪表盘 / 交互控制 —— P3。
