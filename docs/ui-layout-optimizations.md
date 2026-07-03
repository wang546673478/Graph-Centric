# WebUI Layout Optimizations

> **Pinchtab-driven** layout fixes for the Graph-Centric Agent frontend
> (`webui/`). All changes are pure Vue/CSS, no backend modifications.
>
> Companion doc: `docs/v2-README.md`(v2 spec), `docs/v2-agent-harness-complete-spec.zh-CN.md`

---

## 0. Method

Used [pinchtab](https://pinchtab.com) browser automation to take
screenshots of every page in every state (empty / loaded / active run
/ error), then enumerated the visual issues and fixed them in
small focused commits. 5 commits shipped across one session:

```
f2c0050 fix(history): single-line task text + tooltip + clearer close button
44970c5 fix(sidebar): status-tinted run items with pagination
1d83a84 fix(history): search + status filter + pagination + colored status pills
b5a0865 fix(ui): divider between 2D/3D + Graph/Debug tabs + i18n emptyGraph CTA
f3a0d02 fix(ui): remove duplicate New Chat button + default-collapse empty right rail
```

## 1. Issues Found (before)

| # | Page | Symptom |
|---|---|---|
| 1 | Top header | `New Chat` button in the top-right duplicates the sidebar's `+ 新建` button — two visible entry points for the same action |
| 2 | Home (right column) | 340px right column is just a "P3 在此填充" placeholder — wasted screen estate on every fresh visit |
| 3 | RunView (center) | The 2D/3D toggle and Graph/Debug tabs are visually crammed together with no separator — hard to tell which is a group |
| 4 | RunView (empty state) | When no run is active, the GraphPanel renders an empty cytoscape canvas with no instructions — user has to guess what to do |
| 5 | History | 24+ runs shown as one unbounded table with no search, no status filter, no pagination — long vertical scroll-fest |
| 6 | History (status) | Status text color only — Done (green) and Paused (amber) are nearly indistinguishable at a glance |
| 7 | Sidebar (run list) | 24 runs all shown with no pagination — sidebar becomes a vertical scroll wall |
| 8 | Sidebar (status) | Same as #6: inline status text color only, no border or dot |
| 9 | History (task cell) | Long Chinese task descriptions wrap to 2 lines, creating inconsistent row heights |
| 10 | History (trash icon) | 🗑 emoji at 0.85rem / 50% opacity — hard to spot in a 24-row table |

## 2. Fixes Applied

### 2.1 Remove duplicate `New Chat` button (`f3a0d02`)

`webui/src/components/shared/TopBar.vue`: removed the `new-chat-btn`
`<button>` and the dead `newChat()` helper + unused `useRouter` /
`activeRunId` imports. The sidebar's `+ 新建` is now the single
entry point for "start a fresh run".

### 2.2 Default-collapse empty right rail (`f3a0d02`)

`webui/src/App.vue`: the right rail was a 340px placeholder for
unbuilt Phase-3 features. Default it to collapsed for fresh users
(`localStorage.getItem(RIGHT_KEY) === null → rightCollapsed = true`).
The existing rail-toggle (›/‹) lets users re-open it when needed.
Also polished the placeholder text — title / note / hint — so
"Phase 3 实现" isn't just a dev-marked TODO.

**The composer stays accessible**: the Composer lives in
`col-center` (via `RunView`), not in `col-right`. Folding the right
rail doesn't hide the new-task input.

### 2.3 Divider between 2D/3D + Graph/Debug tabs (`b5a0865`)

`webui/src/components/run/RunView.vue`: added a 1px × 18px vertical
divider (60% opacity border) between the view-dimension toggle
(2D / 3D) and the graph-mode tabs (Graph / Debug). Visual
grouping without consuming extra space.

### 2.4 Empty-state CTA in RunView (`b5a0865`)

`webui/src/components/run/RunView.vue`: when `!activeRunId`, render
a centered CTA with a hexagon icon (matches the brand mark in
`Sidebar.vue`), an "No task yet" title, and a hint pointing to
the composer + sidebar. The GraphPanel only renders when a run
is active.

```vue
<div v-if="!activeRunId" class="empty-graph-cta">
  <div class="cta-icon">⬡</div>
  <h3>{{ t('run.emptyGraph.title') || '等待任务' }}</h3>
  <p>{{ t('run.emptyGraph.hint') || '在右侧 composer 中输入任务,或从左侧选一个已有的 run。' }}</p>
</div>
<template v-if="tab === 'graph' && activeRunId">
  <GraphPanel ... />
  <GraphPanel3D v-else ... />
</template>
```

i18n keys added to both `locales/en.ts` and `locales/zh-CN.ts`:

```ts
run: {
  ...
  emptyGraph: {
    title: 'No task yet',
    hint: 'Enter a task in the composer on the right, or pick an existing run from the sidebar.'
  }
}
```

### 2.5 History view: search + status filter + pagination (`1d83a84`)

`webui/src/components/history/HistoryView.vue`:

- Search box (`搜索 task 或 run id…`) — matches by task text
  OR run id, case-insensitive
- Status dropdown — `全部 (24)` / `已完成` / `出错` / `已暂停` /
  `运行中`, each option labelled with the current count
- `match-count` badge (`24 / 24`) on the right of the row
- Empty-match state: `没有匹配的 run` + `清除过滤` button
  (mirrors the sidebar's empty-filter UX)
- 30/page pagination with `还有 N 个 ▼` expand link
- `statusKey()` helper normalizes the backend's various status
  string forms (`'Done'` / `'Error'` / `'{Done: null}'` etc.) to
  four canonical classes (`done` / `error` / `paused` / `running`)
- Colored status pills: `var(--success-soft)` for done,
  `var(--danger-soft)` for error, `var(--warning-soft)` for
  paused, `var(--accent-soft)` for running. Previously the
  pill was text-color-only — Done and Paused were almost
  indistinguishable at 0.7rem.

### 2.6 History view: single-line + tooltip + clear delete (`f2c0050`)

`webui/src/components/history/HistoryView.vue`:

- `table-layout: fixed` on the table + explicit column widths
  (STATUS = 110px, DURATION = 80px, delete = 40px; the TASK
  column gets the rest)
- `.task-cell`: `white-space: nowrap; overflow: hidden;
  text-overflow: ellipsis; max-width: 0` (the `max-width: 0` trick
  forces the cell to shrink-to-fit its column, enabling
  ellipsis on a `table-layout: fixed` cell)
- `:title="r.task"` on the task cell — full text is reachable as
  a browser tooltip on hover
- `.duration-cell` gets `font-family: var(--font-mono)` so
  durations line up vertically across rows
- Trash button: replaced the small `🗑` emoji with a plain `×`
  character at 1.2rem in a 28×28 button cell. Opacity
  0.4 → 1 + danger-soft background on hover. Easier to spot +
  cleaner in a tabular context.

### 2.7 Sidebar: status-tinted run items + pagination (`44970c5`)

`webui/src/components/layout/Sidebar.vue`:

- 3px left border on each `.run-item`, colored by status
  (success / danger / warning / accent). Active run uses the
  accent border (overrides the status border) for clear
  "this is selected" feedback
- Small 6×6 colored dot before the status text in the meta line.
  Two cues (border + dot) so even when the text is truncated the
  status is unmistakable
- 15/page pagination (same UX as `HistoryView`) with
  `还有 N 个 ▼` expand link
- `:title="r.task"` on the task line — full text on hover
  (browser tooltip)

## 3. Result

| Page | Before | After |
|---|---|---|
| Home (empty) | Empty cytoscape canvas | Centered CTA: hex icon + "No task yet" + hint |
| Home (right column) | 340px "P3 placeholder" wasting space | Default-collapsed (toggle to open) |
| TopBar | `New Chat` button duplicated sidebar's `+ 新建` | Single entry point in sidebar |
| Sidebar (run list) | 24 runs all shown, text-only status | 15/page + status dot + colored left border |
| History | One unbounded table, 24 rows, no search | Search + status filter + 30/page + colored status pills |
| History (rows) | Task text wrapped to 2 lines | Single line + ellipsis + tooltip on hover |
| History (delete) | 🗑 emoji at 0.5 opacity | × at 1.2rem in 28×28 button cell |
| History (DURATION) | sans-serif | monospace (numbers align) |
| RunView (tabs) | 2D/3D ↔ Graph/Debug visually merged | 1px divider between groups |

## 4. Verification

Used pinchtab to take a screenshot of each page + state, then
verified the fix in-place. Examples saved to `/tmp/ui-*.png` on
the host that ran the verification.

| State | Result |
|---|---|
| Home empty | CTA "No task yet" + hex icon visible in center |
| Sidebar (24 runs) | First 15 visible, "还有 9 个 ▼" button, each row has colored dot + left border |
| History (24 runs) | Search + filter row at top, all 24 visible (24 < 30 limit), "24 / 24" match count |
| History (long task) | Task text single-line with `…` ellipsis, full text on hover tooltip |
| Right rail collapsed | No 340px placeholder wasting space — only the thin toggle button on the right edge |

## 5. Files Changed

```
webui/src/App.vue                          (+19 -7)   collapse right rail
webui/src/components/shared/TopBar.vue      (-9)       remove duplicate New Chat
webui/src/components/run/RunView.vue         (+19 -1)   divider + empty CTA
webui/src/components/layout/Sidebar.vue     (+79 -8)   status dots + border + pagination
webui/src/components/history/HistoryView.vue (+107 -10)  search + filter + pagination + single-line
webui/src/locales/en.ts                     (+1 -1)    emptyGraph i18n key
webui/src/locales/zh-CN.ts                  (+1 -1)    emptyGraph i18n key
```

5 commits, 7 files, ~155 line changes (all in `webui/`, no
backend changes).

## 6. Not Yet Addressed (low priority)

| # | Item | Why deferred |
|---|---|---|
| 1 | Settings 折叠区视觉提示 (Advisor / 高级设置) | "(click to expand, 17 fields)" text is already explicit — adding a chevron icon is cosmetic |
| 2 | Run list 24/2 sidebar 状态色条 (sidebar) | Status dot + border already in #7 — sidebar can use the same treatment in a future pass |
| 3 | Empty graph 画布"按 / 聚焦 composer"快捷键提示 | ShortcutsHelp modal (⌨ ? button in TopBar) already covers this — adding inline text would be redundant |
| 4 | Bottom composer 与图区视觉连接 | Composer is fixed at the bottom of col-center; a visual gradient / shadow could highlight the connection, but the existing layout is already coherent |

These are cosmetic. The current state is functional and
maintainable.
