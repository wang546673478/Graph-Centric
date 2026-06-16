# Graph-Centric 分形架构实施计划

**目标**：SubAgent 上下文从文件级（10000 行）降到代码块级（50-100 行）

---

## 核心改动

```
现在：                        目标：

auth.rs (File)                auth.rs (File)
  L1: "处理认证"                 Contains
  L2: 10000 行完整源码           ├── login() (Function, 45-120)
                               │     L1: "验证凭证"
                               │     L2: auth.rs[45-120] = 75 行
                               ├── logout() (Function, 122-155)
                               │     L1: "清除会话"
                               │     L2: auth.rs[122-155] = 33 行
                               └── rate_limit() (Function, 160-220)
                                     L1: "限制频率"
                                     L2: auth.rs[160-220] = 60 行
```

---

## 数据模型改动

### 1. Graph + `parent` 引用

`src/graph/mod.rs`：

```rust
pub struct Graph {
    pub nodes: HashMap<NodeId, Node>,
    pub edges: Vec<Edge>,
    pub l1: L1Store,
    // ...existing fields...
    
    /// 递归子图：如果这个图是某复杂节点的内部展开，
    /// 记录父节点 ID 和父图引用。
    #[serde(skip)]
    pub parent: Option<(NodeId, Box<Graph>)>,
}
```

### 2. Node + `expanded` + 行范围 metadata

`src/graph/mod.rs`：

```rust
pub struct Node {
    // ...existing fields...
    
    /// 复杂节点是否已展开子图（true = 下面有子图）
    #[serde(default)]
    pub expanded: bool,
}

// metadata 约定（不是新字段，是 metadata HashMap 的 key）：
//   "line_start": 45    ← AST scanner 填入
//   "line_end": 120     ← AST scanner 填入
```

### 3. NodeKind 不变

`File`、`Function`、`Class`、`Module`、`Task` 等保持原样。AST scanner 产出 `NodeKind::Function` 和 `NodeKind::Class` 节点，已有枚举覆盖。

### 4. RelationType 不变

用已有的 `Contains` 表达文件→函数的包含关系。边是 `File -[Contains]-> Function`。

---

## 三步实施

### Step 1: AST Scanner 重写

`src/domain/code/ast_scanner.rs`

```
当前：stub，产出一个低置信度 File 节点
目标：tree-sitter 扫描，产出 Function/Class 子节点 + Contains 边

输入：文件路径 + 源码
输出：
  Vec<(Node, Vec<Edge>)>
    Node(Function, id="login", metadata={line_start:45, line_end:120})
    Edge(source="auth.rs", target="login", relation=Contains, confidence=0.9)
```

如果暂不引入 tree-sitter，先用**正则**兜底——匹配 `fn`、`def`、`func`、`class` 等关键字，提取名字和起止行。后续切换 tree-sitter 只需替换 scanner 实现，调用方不变。

### Step 2: SourceLoader + `load_range`

`src/context/mod.rs`

```rust
pub trait SourceLoader: Send + Sync {
    fn load(&self, node_id: &NodeId) -> Result<String>;      // 已有
    fn load_range(&self, node_id: &NodeId, start: usize, end: usize) -> Result<String>;  // 新增，默认实现调 load()
}
```

ContextBuilder 加载 L2 时：
- 检查节点 metadata 是否有 `line_start`/`line_end`
- 有 → 调 `load_range(node_id, start, end)`，只读行范围
- 没有 → 调 `load(node_id)`，读整文件（向后兼容）

### Step 3: Decomposer 展开

`src/agent/decomposer.rs`

当前 `decompose()` 产出文件级 Task 节点。改造后：
- 遍历 Task 的 `involved_nodes`
- 如果节点是复杂节点（`expanded = true` 或有 `Contains` 子节点）→ 把子节点加入 Task DAG
- `SubTask.involved_nodes` 从 `[auth.rs]` 变为 `[login(), rate_limit()]`
- ScopeGuard 自动从节点 metadata 读取 `line_start`/`line_end`，生成路径范围

---

## 不改动的部分（引擎不变）

| 组件 | 角色 | 原因 |
|------|------|------|
| `GraphLoop` | 状态机 | 每层同一套 Graph→Task→Review |
| `Proposer` | 规划 | L0 快照渲染支持嵌套（format 时收起了图） |
| `Verifier` | 验证 | 三层验证（结构/模型/L1 采样）对每层都一样 |
| `Repairer` | 修复 | 局部修复逻辑不区分层级 |
| `CascadeBacktracker` | 回溯 | 沿入边回溯，在当前图内做，不进父图 |
| `Dispatcher` | 调度 | 已有 `involved_nodes` + `restrict_reads` |
| `ScopeGuard` | 范围守护 | 已有 `restrict_reads`，自动读取节点 metadata 行号 |
| `SubAgent` | 执行 | 上下文由 ContextBuilder 构建，自动缩到子图范围 |
| 前端组件 | 展示 | 图 JSON 序列化自动支持嵌套 |

---

## 改动文件一览

| 文件 | 改什么 | 权重 |
|------|--------|:--:|
| `src/graph/mod.rs` | `Graph.parent`、`Node.expanded` | 小 |
| `src/domain/code/ast_scanner.rs` | 重写：正则/tree-sitter 扫描 | **大** |
| `src/context/mod.rs` | `load_range()` 方法 | 小 |
| `src/agent/decomposer.rs` | `decompose()` 展开复杂节点 | 中 |
| `Cargo.toml` | （可选）tree-sitter 依赖 | 小 |
| `src/tools/scope_guard.rs` | 无改动，已有 `restrict_reads` | 0 |
| 其余全部文件 | 无改动 | 0 |

---

## 效果

```
改前：
  SubAgent "修 login()"
  → context = 整个 auth.rs (10000 行) + db.rs + config.rs + ...
  → LLM 上下文 ~30000 tokens

改后：
  SubAgent "修 login()"
  → L0 图 = login() 子图 3 节点 + 2 边
  → L1 语义 = login() + rate_limit() + validate_email()
  → L2 源码 = auth.rs[45-220] ≈ 175 行（login + 邻接函数）
  → LLM 上下文 ~5000 tokens
  
  ScopeGuard: bash 只能读 auth.rs:45-220
  上下文精确度提升 ~6 倍
```
