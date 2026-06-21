# 目标澄清交互重做 + 对话可读性 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** 把目标澄清改成 AskUserQuestion 式(选项卡点击=回答,删除「确认开始」按钮+哨兵,收口靠"模型开始 propose_patch");对话主流只显示人话摘要、抑制原始 JSON 流式,原始 JSON 归到已有 Debug tab;修选项卡跨任务串台。

**Architecture:** 后端 graph_loop 删 `CONFIRM_START_SENTINEL`,Clarifying 收口改为"模型发 propose_patch 即进建图";api_runs 在建图/澄清阶段抑制把模型原始 JSON 当 stream_chunk 流给前端(保留 thinking)。前端删确认按钮、修 clarifyOptions 切任务清空、对话只渲染人话摘要 + thinking,原始 JSON 进 Debug tab。

**Tech Stack:** Rust(graph_loop/api_runs)+ Vue(RunView/Composer/Transcript)。`cargo test --lib` + `npm run build`。

参考:spec `docs/superpowers/specs/2026-06-21-clarification-ux-and-readable-transcript-design.md`。

读码确认:
- `CONFIRM_START_SENTINEL`(graph_loop.rs:347)+ Clarifying 检测(:1119-1151,confirmed 走哨兵)。
- ProposePatch arm 在 :1485;Seeding 守卫 :1164。
- 原始 JSON 来自 `stream_chunk` 的 `d.content`(RunView.rs:101-106);Proposer 经 `ModelWithEvents` 流式推 StreamChunk(streaming.rs)。stream_end 锁成 assistant。
- 前端 confirmStart(RunView:174)、Composer 确认按钮(:13)、clarifyOptions(:23)、watch(activeRunId)(:134,未清 clarifyOptions)。
- Debug tab(RunView:252/261)+ DebugTimeline 已有,model_call 进 timeline。

## File Structure
- `src/agent/graph_loop.rs` — 删哨兵 + Clarifying 收口改 propose_patch 驱动 + 改 prompt + 修测试
- `src/web/api_runs.rs` — 抑制建图阶段原始 JSON 流式(保留 thinking)
- `webui/src/components/run/RunView.vue` — 删 confirmStart、切任务清 clarifyOptions、对话渲染防御
- `webui/src/components/run/Composer.vue` — 删确认按钮
- `webui/src/components/run/Transcript.vue` — 不渲染原始 JSON(防御)

---

## Task 1: 后端删哨兵 + Clarifying 收口改 propose_patch 驱动

**Files:** `src/agent/graph_loop.rs`

- [ ] **Step 1: 改 Clarifying 处理块**

把 Clarifying 块(:1119-1151)整体替换为(去掉哨兵检测;收口改为"非 ask_user 步即视为想清楚,进 Seeding";首轮注入澄清指令,提示模型给选项、问清后直接 propose_patch):

```rust
        // ── Clarifying phase: confirm the goal via ask_user, then the model
        // itself starts building. The model emits ask_user (with options) to
        // clarify; the user answers by clicking an option or typing. When the
        // model is satisfied it emits propose_patch to seed start+deliverable
        // — that step IS the "goal confirmed" signal (no button/sentinel).
        if self.graph_phase == GraphPhase::Clarifying {
            if !self.clarifying_primed {
                self.clarifying_primed = true;
                self.conversation.add_user(
                    "GOAL CLARIFICATION PHASE. Confirm the user's goal before building. \
                     Emit `ask_user`: state your current understanding of the goal, and \
                     provide 2-4 concrete `options` (the user can also type their own \
                     answer). Keep asking until the goal is clear. When you are confident \
                     about the deliverable, emit `propose_patch` to seed `start` and \
                     `deliverable` — that begins building. Do not ask for confirmation; \
                     starting to build IS the signal you're ready.",
                );
            }
        }
```

(收口逻辑不在这里做——放到 ProposePatch arm:见 Step 2。这里只负责首轮 priming。)

- [ ] **Step 2: ProposePatch arm 里:Clarifying→Seeding**

在 ProposePatch arm(:1485 `ProposerStep::ProposePatch { mut patch, rationale: _ } => {`)的最开头,加:

```rust
                // If we're still Clarifying and the model starts building,
                // that's the "goal confirmed" signal — advance to Seeding so
                // the seed/guard logic treats this as the first build patch.
                if self.graph_phase == GraphPhase::Clarifying {
                    info!("clarifying: model started building — advancing to Seeding");
                    self.graph_phase = GraphPhase::Seeding;
                }
```

- [ ] **Step 3: 删哨兵常量**

删除 `pub const CONFIRM_START_SENTINEL: &str = "__CONFIRM_START__";`(:347)。

- [ ] **Step 4: 构建(暴露引用哨兵的测试)**

Run: `cd /home/hhhh/Graph-Centric && cargo build --lib 2>&1 | grep -E "CONFIRM_START_SENTINEL|^error" | head`
Expected: 测试里引用 `CONFIRM_START_SENTINEL` 的地方报错(Step 5 修)。非测试代码应无引用。

- [ ] **Step 5: 修/删哨兵相关测试**

`confirm_sentinel_advances_clarifying_to_seeding` 测试(用 `CONFIRM_START_SENTINEL` + resume)改为验证新机制:Clarifying 阶段模型发 propose_patch → 进 Seeding。把该测试改为:

```rust
    #[tokio::test]
    async fn propose_patch_advances_clarifying_to_seeding() {
        // In Clarifying, when the model emits a propose_patch (starts building),
        // the phase advances to Seeding — no confirm button/sentinel.
        let patch = r#"{"step":"propose_patch","patch":{"add_nodes":[{"id":"start","kind":"Task","summary":"s","immutable":true},{"id":"deliverable","kind":"Task","summary":"d"}],"add_edges":[{"source":"start","target":"deliverable","relation":"LeadsTo","confidence":0.9}],"reason":"seed"},"rationale":"r"}"#;
        let mut gl = build_loop_with(vec![patch]);
        assert_eq!(gl.graph_phase, GraphPhase::Clarifying);
        let _ = gl.step_graph().await.unwrap();
        assert_eq!(gl.graph_phase, GraphPhase::Filling, "propose_patch from Clarifying builds + advances");
    }
```
(注:seed patch 应用后,现有 Seeding→Filling 转换会把 phase 推到 Filling,所以断言 Filling。)若还有其它引用 `CONFIRM_START_SENTINEL` 的测试,一并改/删。

- [ ] **Step 6: 测试 + 构建**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error" | head; echo "exit=${PIPESTATUS[0]}"; cargo test --lib graph_loop:: 2>&1 | tail -4`
Expected: 构建 exit=0;graph_loop 测试全绿。

- [ ] **Step 7: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "feat(agent): clarifying ends when model starts building (drop confirm sentinel)"
```

---

## Task 2: 后端抑制建图阶段原始 JSON 流式

**Files:** `src/web/api_runs.rs`

模型每步的原始输出(含 propose_patch JSON)经 `ModelWithEvents` → StreamChunk → 前端 `stream:<comp>` 显示。要让对话只剩人话摘要,需让 Proposer 那一步的 model 不把原始内容当可见流推给对话。但 thinking(reasoning_content)要保留。

最小侵入做法:不动流式机制本身,而在**前端**把 `stream:proposer`/`stream:model` 这类**内容流**不渲染进主对话(只渲染 thinking + 后端人话摘要 Transcript)。因此 Task 2 改为**前端职责**(见 Task 4),后端无需改流式。

→ **本 Task 仅确认:后端已有每步人话摘要 Transcript(📝/🔍/✅)。** 读 api_runs 确认 `transcript_lines_for_step`/render 逻辑产出人话摘要事件;若 propose_patch 的摘要不含 reason,则补上。

- [ ] **Step 1: 确认/增强人话摘要**

Run: `grep -nE "📝|🔍|✅|propose_patch.*reason|fn .*transcript.*step|action.into" src/web/api_runs.rs | head`
读出每步摘要生成处。确认 propose_patch → `📝 {reason}` 已存在(之前见过)。若 reason 为空时摘要太干,补默认文案"📝 更新关系图(+Nn/+Ne)"。不改则跳过。

- [ ] **Step 2: 构建**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error"; echo exit=${PIPESTATUS[0]}`
Expected: exit=0。

- [ ] **Step 3: 提交(若有改动)**

```bash
cd /home/hhhh/Graph-Centric
git add src/web/api_runs.rs
git commit -m "feat(web): ensure human-readable per-step summary for propose_patch" || echo "no change"
```

---

## Task 3: 前端删确认按钮

**Files:** `webui/src/components/run/Composer.vue`, `webui/src/components/run/RunView.vue`

- [ ] **Step 1: Composer 删确认按钮**

把 `Composer.vue` 改回无确认按钮版(删 `paused` prop、`confirmStart` emit、按钮、`.confirm-btn` 样式):

```vue
<script setup lang="ts">
import { ref } from 'vue'
import { useI18n } from '../../composables/useI18n'
const { t } = useI18n()
const props = defineProps<{ disabled: boolean }>()
const emit = defineEmits<{ send: [task: string] }>()
const msg = ref('')
function send() { const v = msg.value.trim(); if (!v || props.disabled) return; msg.value = ''; emit('send', v) }
</script>

<template>
  <div class="composer">
    <input v-model="msg" :disabled="disabled" :placeholder="t('composer.placeholder')" @keydown.enter="send" />
    <button class="primary" :disabled="disabled" @click="send">{{ disabled ? '…' : t('composer.send') }}</button>
  </div>
</template>

<style scoped>
.composer { display: flex; gap: 8px; padding: 12px; border-top: 1px solid var(--border); }
.composer input { flex: 1; }
.composer button { padding: 8px 20px; white-space: nowrap; }
</style>
```

- [ ] **Step 2: RunView 删 confirmStart + Composer 改用法**

在 RunView.vue:删除 `confirmStart` 函数(:174 附近整段)。把模板里的 `<Composer ... :paused=... @confirmStart=... />` 改回:
```html
      <Composer :disabled="sending" @send="submitTask" />
```
(grep 确认 Composer 用法那行的确切属性后改。)

- [ ] **Step 3: 构建**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功(若 confirmStart 还被引用会报错,清掉)。

- [ ] **Step 4: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/components/run/Composer.vue webui/src/components/run/RunView.vue
git commit -m "feat(webui): remove confirm-start button (option click = answer)"
```

---

## Task 4: 前端修串台 + 对话只渲染人话(抑制原始 JSON)

**Files:** `webui/src/components/run/RunView.vue`, `webui/src/components/run/Transcript.vue`

- [ ] **Step 1: 切任务清空 clarifyOptions**

RunView.vue 的 `watch(activeRunId, (id) => {...})`(:134)改为切任务时清空选项:
```typescript
watch(activeRunId, (id) => {
  clarifyOptions.value = []
  if (id && getRunStore(id)) {
    connectToRun(id)
  }
})
```

- [ ] **Step 2: 抑制内容流进主对话**

RunView.vue 的 `case 'stream_chunk':`(:86)块:保留 thinking(reasoning_content)缓冲,但**不再**把 `d.content`(模型原始输出,含 JSON)push 进主 transcript。把 `// Content: per-component buffer.` 那段(:100-107,push `stream:<comp>`)删除或改为不进 transcript。原始内容仍可经 model_call 事件进 Debug。改为:
```typescript
      case 'stream_chunk': {
        const comp = d.component || 'model'
        const thinkRole = 'thinking:' + comp
        // Keep thinking (reasoning) in the transcript (collapsible). Do NOT
        // stream raw model content (it includes the step JSON) into the main
        // conversation — the human-readable per-step summary (📝/🔍/✅) comes
        // from the Transcript events instead. Raw output stays in Debug tab.
        if (d.reasoning_content) {
          const thinkLast = s.transcript[s.transcript.length - 1]
          if (thinkLast && thinkLast.role === thinkRole) {
            thinkLast.content += d.reasoning_content
          } else {
            s.transcript.push({ role: thinkRole, content: d.reasoning_content })
          }
        }
        break
      }
```
对应的 `case 'stream_end':` 里锁 `stream:<comp>`→assistant 的逻辑可保留(无 stream 内容时无副作用),但 thinking 锁定保留。

- [ ] **Step 3: Transcript 防御:不渲染裸 JSON**

Transcript.vue:对 role 为 assistant 的消息,若 content trim 后以 `{` 开头且含 `"step"`(裸 step JSON),不在主对话渲染(防御,防遗漏的流)。在 useMd / 渲染判断里加:
```typescript
function looksLikeStepJson(c: string): boolean {
  const t = c.trim()
  return t.startsWith('{') && t.includes('"step"')
}
```
渲染模板里 `v-if` 跳过 `looksLikeStepJson(m.content)` 的 assistant 消息(或显示成"📝 (见 Debug)")。最小:跳过不显示。

- [ ] **Step 4: 构建**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功。

- [ ] **Step 5: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/components/run/RunView.vue webui/src/components/run/Transcript.vue
git commit -m "feat(webui): per-run options + suppress raw JSON in main chat (keep thinking)"
```

---

## Task 5: Debug tab 确认原始 JSON 可查

**Files:** `webui/src/components/run/DebugTimeline.vue`(确认,可能无需改)

- [ ] **Step 1: 确认 model_call 进 Debug**

读 DebugTimeline.vue + RunView 的 `case 'model_call':`(:80):model_call 含 `response_content`(模型完整输出)。确认它进 transcript(role='model')且 DebugTimeline 渲染 model 项。`detailMode` 开启时后端才发 model_call(确认 detail_mode 逻辑)。
- 若 Debug tab 已能显示 model_call 的完整 response_content → 无需改,本 Task 仅验证。
- 若 detailMode 默认关导致 Debug 看不到 → 在 Debug tab 视图里提示"开启 detail mode 查看模型原始输出"。

- [ ] **Step 2: 构建(若改动)**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功。

- [ ] **Step 3: 提交(若改动)**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/components/run/DebugTimeline.vue
git commit -m "feat(webui): debug tab shows raw model output for inspection" || echo "no change"
```

---

## Task 6: 重建 + 重启 + 端到端验证 + 推送

- [ ] **Step 1: 重建**

Run: `cd /home/hhhh/Graph-Centric && cargo build --bin serve 2>&1 | grep -E "^error"; echo "exit=${PIPESTATUS[0]}"; cargo test --lib 2>&1 | tail -3; cd webui && npm run build 2>&1 | tail -1`
Expected: 构建 exit=0;测试全绿;前端 ✓ built。

- [ ] **Step 2: 重启 serve**

```bash
cd /home/hhhh/Graph-Centric
pid=$(pgrep -f "target/debug/serve" | head -1); [ -n "$pid" ] && kill "$pid"; sleep 1
WEB_PORT=8090 setsid ./target/debug/serve > /tmp/graph-serve.log 2>&1 < /dev/null & disown
sleep 4; curl -s -o /dev/null -w "HTTP %{http_code}\n" http://localhost:8090/
```
Expected: HTTP 200。

- [ ] **Step 3: 端到端(pinchtab)**

跑一个任务,确认:① 澄清阶段出现问题 + 选项卡,**无「确认开始」按钮**;② 点选项卡 = 回答(或输入框打字);③ 模型问清后自己 propose_patch 开始建图(无需按钮);④ 对话主流是人话摘要,**无原始 `{...}` JSON**;⑤ 切到别的任务再切回,选项卡不串台;⑥ Debug tab 能看到原始 JSON/model_call。

- [ ] **Step 4: 推送**

```bash
cd /home/hhhh/Graph-Centric
git push origin main
```

---

## 验收(整体)
- 澄清:选项卡点击=回答,无确认按钮/哨兵;模型 propose_patch 即进建图。
- 对话主流只显示人话摘要 + thinking,无原始 JSON;Debug tab 可查原文。
- 选项卡不跨任务串台。
- `cargo test --lib` 全绿(含 propose_patch→Seeding 新测试)。
- 端到端 6 项全过。

## 不做(YAGNI)
- 不重构流式机制(前端不渲染内容流即可,后端 StreamChunk 保留供 Debug/未来)。
- 不为选项卡做多选/富交互。
- 不改 DebugTimeline 结构(仅确认/最小提示)。
