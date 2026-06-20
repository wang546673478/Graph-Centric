# 目标澄清阶段(Clarifying Phase)实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** 在 agent 建图之前插入一个「目标澄清」FSM 阶段:任务进来先和用户确认目标(模型给选项+其他或纯文本问题,Paused 等答,来回几轮),用户点「确认开始」哨兵后才进入 Seeding 建图。Heartbeat 无人值守模式跳过。

**Architecture:** 在 `GraphPhase` 加 `Clarifying` 前置阶段。非 heartbeat run 起始于 `Clarifying`,Proposer 被 prompt 约束为只发 AskUser(不建图);复用现有 `Paused → POST /answer → resume()` 管线(零新增端点)。收到退出哨兵 `__CONFIRM_START__` 时切到 `Seeding`。Heartbeat 起始仍是 `Seeding`。

**Tech Stack:** Rust(graph_loop FSM、proposer prompt)+ 既有 Vue 前端(AskUser options 已能渲染)。后端 `cargo test --lib`,前端 `npm run build`。

参考:spec `docs/superpowers/specs/2026-06-20-goal-clarification-design.md`。

读码确认的现状:
- `GraphPhase` 枚举(graph_loop.rs:327)= Seeding/Filling/Expanding/Verifying。
- 构造器 phase 初始化:`graph_phase: GraphPhase::Seeding`(:810)。
- `step_graph` 顶部有 Seeding 守卫(:1100,空图 Seeding 计数);AskUser arm 在 :1184(已 `Ok(LoopState::Paused{question,rationale})`)。
- `resume(answer)`(:973)把答案 add_user、清 pending。
- Proposer `build_system_prompt`(:256)/ `build_system_prompt_heartbeat`(:331)。
- `is_heartbeat`(config:468)。

## File Structure
- Modify: `src/agent/graph_loop.rs` — 加 `GraphPhase::Clarifying`;起始阶段按 is_heartbeat 选择;step_graph 处理 Clarifying(AskUser→Paused、哨兵→Seeding);常量 `CONFIRM_START_SENTINEL`
- Modify: `src/agent/proposer.rs` — Clarifying 阶段的 system-prompt 块(只澄清、给选项+其他、不建图)
- Modify: `webui/src/components/run/Transcript.vue` 或 `Composer.vue` — 澄清进行时显示「✅ 确认开始」按钮,点击经 /answer 发送哨兵

---

## Task 1: 加 `GraphPhase::Clarifying` 变体 + 退出哨兵常量

**Files:**
- Modify: `src/agent/graph_loop.rs:326-337`(GraphPhase 枚举)

- [ ] **Step 1: 加枚举变体**

把 `GraphPhase` 枚举(:326)改为(在 Seeding 前加 Clarifying):

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphPhase {
    /// Pre-build: confirm the goal WITH the user. The Proposer only emits
    /// AskUser (options + "other", or free text) and never builds the graph
    /// until the user sends the confirm sentinel. Skipped in heartbeat mode.
    Clarifying,
    /// First step: build only Start (anchor, immutable) + Goal (target)
    /// with a single DependsOn edge Goal→Start.
    Seeding,
    /// The model explores and fills intermediate nodes between Start and Goal.
    Filling,
    /// Cascade-expand abstract Task nodes into sub-graphs of concrete sub-nodes.
    Expanding,
    /// Model emitted ready_for_verify — verifier runs, then Task phase.
    Verifying,
}
```

- [ ] **Step 2: 加退出哨兵常量**

在 `GraphPhase` 枚举定义之后(`}` 之后)加:

```rust
/// When the user sends this exact answer during the Clarifying phase, the
/// loop treats the goal as confirmed and advances to Seeding. The frontend's
/// "✅ 确认开始" button posts this via /answer.
pub const CONFIRM_START_SENTINEL: &str = "__CONFIRM_START__";
```

- [ ] **Step 3: 构建验证(预期失败 — match 不穷尽)**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "non-exhaustive|^error" | head`
Expected: 出现 non-exhaustive match 错误(Clarifying 未处理)——确认新变体已加入,下一步补 match 分支。

- [ ] **Step 4: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "feat(agent): add GraphPhase::Clarifying variant + confirm sentinel"
```

---

## Task 2: 起始阶段按 is_heartbeat 选择

**Files:**
- Modify: `src/agent/graph_loop.rs:810`(构造器 graph_phase 初始化)

- [ ] **Step 1: 起始阶段条件化**

把 `:810` 的 `graph_phase: GraphPhase::Seeding,` 改为:

```rust
            graph_phase: if config.is_heartbeat { GraphPhase::Seeding } else { GraphPhase::Clarifying },
```

（注意:此行在构造 `Self { ... }` 内,`config` 在此作用域可用——确认上下文里 `config` 字段在该结构体字面量构造之前已绑定;若 `config` 已被 move 进结构体字段,改为在构造前先计算 `let initial_phase = if config.is_heartbeat { GraphPhase::Seeding } else { GraphPhase::Clarifying };` 再用 `graph_phase: initial_phase,`。实现时按实际借用情况二选一。）

- [ ] **Step 2: 构建验证**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error" | head; echo exit=${PIPESTATUS[0]}`
Expected: 仍有 non-exhaustive match 错误(Task 3 处理),但**不应**有 `config` 借用/move 错误。若有 move 错误,改用上面括号里的 `let initial_phase` 方案。

- [ ] **Step 3: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "feat(agent): start in Clarifying phase (Seeding for heartbeat)"
```

---

## Task 3: step_graph 处理 Clarifying 阶段

**Files:**
- Modify: `src/agent/graph_loop.rs`(step_graph 顶部,Seeding 守卫 :1100 之前)

Clarifying 阶段逻辑:在 step_graph 调用 proposer 之前,如果处于 Clarifying,检查最近一次用户答案是否为哨兵 → 是则切 Seeding 继续正常流程;否则正常让 proposer 出 AskUser(它会被 prompt 约束成澄清问题),走现有 AskUser→Paused 路径。

实现要点:哨兵检测放在 step_graph 入口。用户答案通过 `resume()` 进了 `conversation`(add_user)。我们检查 conversation 最后一条 user 消息是否等于哨兵。

- [ ] **Step 1: 在 step_graph 入口加 Clarifying 处理**

在 `step_graph` 方法体最开头(`SEEDING_STALL_LIMIT` 那段 `:1099` 之前)插入:

```rust
        // ── Clarifying phase: confirm the goal with the user before building ──
        // The user's "✅ 确认开始" button posts CONFIRM_START_SENTINEL via
        // /answer, which resume() appended as the last user message. Seeing it
        // means the goal is confirmed → advance to Seeding. Otherwise stay in
        // Clarifying; the Proposer (prompt-constrained) will emit an AskUser to
        // refine the goal, which routes through the normal AskUser→Paused path.
        if self.graph_phase == GraphPhase::Clarifying {
            let confirmed = self
                .conversation
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m.role, crate::model::Role::User))
                .map(|m| m.content.trim() == CONFIRM_START_SENTINEL)
                .unwrap_or(false);
            if confirmed {
                info!("clarifying: user confirmed goal — advancing to Seeding");
                self.graph_phase = GraphPhase::Seeding;
                self.conversation.add_user(
                    "✅ Goal confirmed. Now build the graph: emit a propose_patch \
                     creating exactly two nodes — Start (A) and Goal (D) — joined by \
                     one DependsOn edge.",
                );
            }
        }
```

（注:`crate::model::Role` 的引用路径——确认 conversation.messages 元素的 role 字段类型。读 conversation.rs 已知 `Message { role: Role, content: String }`,`Role::User` 是枚举值。若 `Role` 已在本文件 import,可直接写 `Role::User`。)

- [ ] **Step 2: 在所有 match graph_phase 处补 Clarifying 分支**

`cargo build` 报的 non-exhaustive 位置(Task 1 Step 3 看到的)逐一补 `GraphPhase::Clarifying => {}` 或合并到已有分支。已知的 match 点:`:2878` 的 convergence 检测(`if self.graph_phase == GraphPhase::Seeding || ...`)——Clarifying 阶段图为空,convergence 不应触发,该处用 `==` 比较不是 match,无需改;真正的 `match self.graph_phase {}` 在 phase-transition 块(:1581 附近 `GraphPhase::Seeding => {...} GraphPhase::Filling => {...} _ => {}`)已有 `_ => ` 兜底,Clarifying 落入兜底即可。运行构建确认还有没有遗漏的穷尽 match。

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error|non-exhaustive" | head`
Expected: 逐步消除所有 non-exhaustive 错误,最终无 error。

- [ ] **Step 3: 构建通过**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error" | head; echo exit=${PIPESTATUS[0]}`
Expected: exit=0。

- [ ] **Step 4: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "feat(agent): handle Clarifying phase — confirm sentinel advances to Seeding"
```

---

## Task 4: Proposer 在 Clarifying 阶段的 prompt 约束

**Files:**
- Modify: `src/agent/proposer.rs`

Proposer 需要知道当前在 Clarifying 阶段,从而 system prompt 指示"只澄清目标、给选项+其他或纯文本问题、绝不 propose_patch"。Proposer 当前不持有 graph_phase——最小改动:在 `next_step`/`next_step_with_retry` 已接收 `graph`,但 phase 不在其中。改为在 graph_loop 调用 proposer 前,Clarifying 阶段时往 conversation 注入一条 system/user 引导,而不改 proposer 签名(YAGNI,避免大改接口)。

- [ ] **Step 1: graph_loop 在 Clarifying 阶段注入澄清指令**

在 Task 3 Step 1 的 Clarifying 块里,`if confirmed { ... }` 的 `else` 分支加一次性引导(只在该阶段首轮注入,用一个 bool 标记避免重复)。先在结构体加字段:在 `convergence_hint_sent: bool,` 附近(结构体字段区)加:

```rust
    /// Whether the one-time Clarifying-phase instruction was injected.
    clarifying_primed: bool,
```

构造器初始化(`convergence_hint_sent: false,` 附近)加:

```rust
            clarifying_primed: false,
```

把 Task 3 的 Clarifying 块改为(在非 confirmed 时首轮注入指令):

```rust
        if self.graph_phase == GraphPhase::Clarifying {
            let confirmed = self
                .conversation
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m.role, crate::model::Role::User))
                .map(|m| m.content.trim() == CONFIRM_START_SENTINEL)
                .unwrap_or(false);
            if confirmed {
                info!("clarifying: user confirmed goal — advancing to Seeding");
                self.graph_phase = GraphPhase::Seeding;
                self.conversation.add_user(
                    "✅ Goal confirmed. Now build the graph: emit a propose_patch \
                     creating exactly two nodes — Start (A) and Goal (D) — joined by \
                     one DependsOn edge.",
                );
            } else if !self.clarifying_primed {
                self.clarifying_primed = true;
                self.conversation.add_user(
                    "GOAL CLARIFICATION PHASE. Before building anything, confirm the \
                     user's goal. Emit an `ask_user` step: state your current \
                     understanding of the goal, then either offer a few concrete \
                     options (the user can also reply with their own answer), or ask \
                     a focused question. Do NOT propose_patch or build the graph yet. \
                     The user will keep answering until they confirm; when they're \
                     satisfied they click a confirm button. Keep clarifying until then.",
                );
            }
        }
```

- [ ] **Step 2: 构建验证**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error" | head; echo exit=${PIPESTATUS[0]}`
Expected: exit=0(新字段 + 注入逻辑编译通过)。

- [ ] **Step 3: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "feat(agent): prime Proposer to clarify goal during Clarifying phase"
```

---

## Task 5: 单元测试 — Clarifying 状态流转

**Files:**
- Modify: `src/agent/graph_loop.rs`(#[cfg(test)] 区)

- [ ] **Step 1: 写测试**

在测试模块加(复用现有 `build_loop_with` helper;模型脚本让其发 ask_user,再模拟用户确认):

```rust
    #[tokio::test]
    async fn non_heartbeat_run_starts_in_clarifying() {
        let gl = build_loop_with(vec!["{}"]);
        assert_eq!(gl.graph_phase, GraphPhase::Clarifying);
    }

    #[tokio::test]
    async fn confirm_sentinel_advances_clarifying_to_seeding() {
        let ask = r#"{"step":"ask_user","question":"目标是什么?","rationale":"r"}"#;
        let mut gl = build_loop_with(vec![ask, ask]);
        assert_eq!(gl.graph_phase, GraphPhase::Clarifying);
        // Round 1: model asks → Paused.
        let s1 = gl.step_graph().await.unwrap();
        assert!(matches!(s1, LoopState::Paused { .. }));
        assert_eq!(gl.graph_phase, GraphPhase::Clarifying, "still clarifying after a question");
        // User confirms via sentinel.
        gl.resume(CONFIRM_START_SENTINEL);
        // Next step sees the sentinel → advances to Seeding.
        let _ = gl.step_graph().await.unwrap();
        assert_eq!(gl.graph_phase, GraphPhase::Seeding, "confirm advances to Seeding");
    }
```

注:`build_loop_with` 默认 `is_heartbeat=false`,所以起始为 Clarifying。若该 helper 的 config 未设 is_heartbeat，确认其默认值为 false(GraphLoopConfig::defaults_at 设的 false)。

- [ ] **Step 2: 跑测试**

Run: `cd /home/hhhh/Graph-Centric && cargo test --lib graph_loop:: 2>&1 | tail -6`
Expected: 全绿,含两个新测试。

- [ ] **Step 3: heartbeat 起始测试**

加测试验证 heartbeat 起始为 Seeding。在测试模块加:

```rust
    #[test]
    fn heartbeat_run_starts_in_seeding() {
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec!["{}"]));
        let tools = Arc::new(ToolRegistry::new());
        let proposer = GraphProposer::new(model.clone(), tools.clone(), None);
        let verifier = Verifier::structural_only();
        let mut cfg = GraphLoopConfig::defaults_at(std::env::current_dir().unwrap());
        cfg.is_heartbeat = true;
        let gl = GraphLoop::new("hb task", proposer, verifier, None, tools, cfg);
        assert_eq!(gl.graph_phase, GraphPhase::Seeding);
    }
```

Run: `cd /home/hhhh/Graph-Centric && cargo test --lib graph_loop:: 2>&1 | tail -6`
Expected: 全绿。

- [ ] **Step 4: 全量测试**

Run: `cd /home/hhhh/Graph-Centric && cargo test --lib 2>&1 | tail -3`
Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "test(agent): Clarifying phase start + confirm-sentinel transition"
```

---

## Task 6: 前端「✅ 确认开始」按钮

**Files:**
- Modify: `webui/src/components/run/Composer.vue`

澄清进行时(run 状态为 paused),在 Composer 加一个「✅ 确认开始」按钮,点击经 `/answer` 发送哨兵 `__CONFIRM_START__`。RunView 的 `submitTask` 已能在 paused 时把输入当 answer 发(读 RunView 已知:paused 时 POST `/api/runs/:id/answer`)。按钮复用这条路径,发送固定哨兵字符串。

- [ ] **Step 1: Composer 加确认按钮**

把 `webui/src/components/run/Composer.vue` 改为(加一个 emit 和按钮):

```vue
<script setup lang="ts">
import { ref } from 'vue'
import { useI18n } from '../../composables/useI18n'
const { t } = useI18n()
const props = defineProps<{ disabled: boolean; paused?: boolean }>()
const emit = defineEmits<{ send: [task: string]; confirmStart: [] }>()
const msg = ref('')
function send() { const v = msg.value.trim(); if (!v || props.disabled) return; msg.value = ''; emit('send', v) }
</script>

<template>
  <div class="composer">
    <button v-if="paused" class="primary confirm-btn" @click="emit('confirmStart')">✅ 确认开始</button>
    <input v-model="msg" :disabled="disabled" :placeholder="t('composer.placeholder')" @keydown.enter="send" />
    <button class="primary" :disabled="disabled" @click="send">{{ disabled ? '…' : t('composer.send') }}</button>
  </div>
</template>

<style scoped>
.composer { display: flex; gap: 8px; padding: 12px; border-top: 1px solid var(--border); }
.composer input { flex: 1; }
.composer button { padding: 8px 20px; white-space: nowrap; }
.confirm-btn { background: var(--success); }
</style>
```

- [ ] **Step 2: RunView 处理 confirmStart**

在 `webui/src/components/run/RunView.vue`,给 Composer 传 `:paused` 并处理 `@confirmStart`。找到模板里的 `<Composer :disabled="sending" @send="submitTask" />`,改为:

```html
      <Composer :disabled="sending" :paused="status === 'paused' || status === 'Paused'" @send="submitTask" @confirmStart="confirmStart" />
```

在 `<script setup>` 的 `submitTask` 附近加函数(复用 paused-answer 路径,发送哨兵):

```typescript
async function confirmStart() {
  const id = activeRunId.value
  if (!id) return
  try {
    await fetch(`/api/runs/${id}/answer`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ answer: '__CONFIRM_START__' }),
    })
    const s = getRunStore(id)
    if (s) s.status = 'Running'
  } catch (e: any) {
    const s = getRunStore(id); if (s) s.error = String(e)
  }
}
```

- [ ] **Step 3: 构建验证**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功,无 TS 错误。

- [ ] **Step 4: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/components/run/Composer.vue webui/src/components/run/RunView.vue
git commit -m "feat(webui): add confirm-start button for goal clarification"
```

---

## Task 7: 重建 + 重启 + 端到端验证 + 推送

- [ ] **Step 1: 重建后端 + 前端**

Run:
```bash
cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error"; echo "serve exit=${PIPESTATUS[0]}"
cd /home/hhhh/Graph-Centric/webui && npm run build 2>&1 | tail -1
```
Expected: serve exit=0;前端 `✓ built`。

- [ ] **Step 2: 重启 serve**

```bash
cd /home/hhhh/Graph-Centric
pid=$(pgrep -f "target/debug/serve" | head -1); [ -n "$pid" ] && kill "$pid"; sleep 1
WEB_PORT=8090 setsid ./target/debug/serve > /tmp/graph-serve.log 2>&1 < /dev/null & disown
sleep 4; curl -s -o /dev/null -w "HTTP %{http_code}\n" http://localhost:8090/
```
Expected: HTTP 200。

- [ ] **Step 3: 端到端验证(pinchtab)**

新建一个任务,确认:进入运行页后 **图不立即构建**,对话栏出现模型的目标澄清问题(+选项),Composer 出现「✅ 确认开始」按钮;答一两轮后点「确认开始」→ 图开始构建(A/D 出现)。

- [ ] **Step 4: 推送**

```bash
cd /home/hhhh/Graph-Centric
git push origin main
```

---

## 验收(整体)
- `cargo test --lib` 全绿(含 Clarifying 起始 / 哨兵转 Seeding / heartbeat 起始 Seeding 三测)。
- 非 heartbeat run 起始于 Clarifying:不建图,先出 ask_user 澄清目标。
- 用户点「✅ 确认开始」(发哨兵)→ 转 Seeding 建 A→D。
- Heartbeat run 起始 Seeding,跳过澄清(自动化不被阻塞)。
- 澄清问答全走现有 Paused/answer 管线,无新增后端端点。
- 现有功能无回归。

## 不做(YAGNI / 留后续)
- 图方向翻转为 LeadsTo:高风险核心改动,第二期单独 spec。
- 执行阶段(Seeding 之后)的 consult_advisor/自决约束:本期靠现有机制 + prompt,不新增升级逻辑(spec 已说明)。
- 澄清独立面板:复用对话栏。
