# 反序列化后重建图索引 修复设计

日期:2026-06-21
状态:设计已确认(用户选方案 C),待审阅

## Context

实测报告 + dump 铁证:某 run 的 `data/runs/fb95e74a.../checkpoints/0028.json` 里 4 条主链边都在(都是 `source=start, target=下游, relation="LeadsTo", confidence=0.99`),但 ready_for_verify 兜底仍报 "start 不可达 deliverable / explore-existing / outline",图被退回 Filling,无法验证。

**根因(读码坐实,排除用户的 3 个假设)**:
- `Graph` 的邻接索引 `outgoing_idx` / `incoming_idx`(`HashMap<NodeId, Vec<usize>>`)标了 `#[serde(skip)]`(graph/mod.rs:519-522)——**反序列化时不还原,默认空 map**。
- `Graph::outgoing()`(:617)**只读** `outgoing_idx`;索引空 → 返回零条出边。
- `path_exists` BFS 经 `outgoing()` 遍历(graph_loop.rs:3057)→ 走不出任何边 → `path_exists(start, *)` 恒 false。
- `replay_from_anchor` 据此把 start 到不了的节点全判成孤儿 → ready_for_verify 兜底(graph_loop.rs:1805)退回 Filling。
- `Graph` 有 `rebuild_indices()`(:692),但 `persistence.rs` 从 checkpoint 反序列化 `Checkpoint`/`Graph` 时(:101/:114)**从不调用它**。

排除用户假设:不是陈旧快照(demo_output/graph.json 无关,验证器/replay 读的是反序列化的 Graph)、不是 immutable 不可遍历、不是 relation/去重 bug —— 是**反序列化后边索引未重建**。

## 已确认决策(方案 C)

- **A 根治**:让 `Graph` 反序列化后自动调 `rebuild_indices()`,使所有加载路径(checkpoint / persistence / branch restore / 任意 `serde_json::from_*`)都不会再得到空索引。
- **+ 回归单测**:序列化 round-trip 后 `path_exists(start, deliverable)` 仍为 true,防止回归(`outgoing` 是 cascade / replay / convergence 共同依赖的承重不变量)。

## 架构 / 实现

### `src/graph/mod.rs` — Graph 自定义反序列化后重建索引

`Graph` 当前是 `#[derive(Serialize, Deserialize)]`。改为**保留 derive 的字段反序列化,但在反序列化后重建索引**。最小侵入做法:

- 用一个"影子结构体"模式:定义一个私有 `GraphData`(与 Graph 同字段、`#[derive(Deserialize)]`,不含两个 skip 索引),为 `Graph` 手写 `impl<'de> Deserialize<'de>`:先反序列化成 `GraphData`,构造 `Graph`,再调 `self.rebuild_indices()` 填充 `outgoing_idx`/`incoming_idx`,返回。
- `rebuild_indices()`(已存在,:692)已 clear + 重建两个索引,直接复用。
- `Serialize` 保持 derive 不变(skip 索引本就不序列化,正确)。

(若手写 Deserialize 过繁,等价备选:保留 derive,但在所有加载出口统一过一层 `g.rebuild_indices()`——但用户选 A 根治,优先自定义 Deserialize,确保任何反序列化路径都安全。)

### 复用
- `rebuild_indices()` 已实现且正确,不改其逻辑,只确保反序列化路径调它。
- `parent`(也是 `#[serde(skip)]`)按现状(注释说由 Contains 边重walk 重建)不在本次范围。

## 测试

- **单测(回归)**:构造 `start --LeadsTo--> deliverable`(+ 一两个中间节点串成链)的 Graph → `serde_json::to_string` → `from_str` → 断言反序列化后的图 `path_exists(start, deliverable)` 为 true、`outgoing(start)` 非空。证明 round-trip 后索引重建。
- **单测(直接)**:`rebuild_indices` 后 `outgoing`/`incoming` 计数正确(若已有类似测试则跳过)。
- 全量 `cargo test --lib` 全绿。
- **端到端**:重启 serve,对一个已有 checkpoint 的 run 或新跑任务走到 ready_for_verify,确认不再误报 "start 不可达";图能正常进验证。

## 错误处理 / 边界
- 自定义 Deserialize 必须覆盖 `l1`/`version`/`status` 等所有字段(影子结构体同字段),避免漏字段。
- 空图反序列化:rebuild_indices 对空 edges 安全(clear 后无插入)。
- 向后兼容:旧 checkpoint JSON 不含索引字段,本就如此;修复只影响"加载后是否重建",不改 JSON 格式。

## 不做(YAGNI)
- 不改 JSON 序列化格式(索引仍不落盘,加载时重建)。
- 不重建 `parent`(超范围,现有机制不变)。
- 不动 Verifier(根因不在它)。
