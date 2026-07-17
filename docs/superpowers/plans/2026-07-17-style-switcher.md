# Style Switcher Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a runtime-toggleable visual style dimension to the Graph-Centric webui. Users pick one of four styles (Minimal / Glassmorphism / Notion / Bento) from a Settings page thumbnail picker; the existing light/dark `[data-theme]` continues to work orthogonally, yielding 8 visual states.

**Architecture:** Extend the existing `useTheme.ts` composable to manage a second axis (`style`), copy 4 mockup JPEGs into `webui/public/themes/` as preview thumbnails, write a new `themes.css` with 8 `[data-theme][data-style]` selector blocks that hold all CSS variable values, and remove the existing `:root, [data-theme="light"]` / `[data-theme="dark"]` token blocks from `main.css`. The HTML `<html>` element ends up carrying `data-theme="light"` AND `data-style="minimal"` etc., and CSS resolves the tokens via the matched selector.

**Tech Stack:** Vue 3 (composition API), TypeScript, Vite (dev server), plain CSS variables — no new dependencies.

## Global Constraints

- **Branch:** commit directly on `main` (per project memory; no branches).
- **Push:** every commit is followed by `git push origin main`.
- **Existing `useTheme.ts`:** must be **extended**, not replaced. Existing `theme` / `toggleTheme` / `applyTheme` API surface stays intact.
- **Import order in `main.ts`:** `themes.css` MUST be imported BEFORE `main.css` so its variable overrides win the cascade.
- **`main.ts` preload pattern:** `applyTheme()` is already called pre-mount to avoid first-paint flash. `applyStyle()` follows the same pattern, also called pre-mount, after the existing `applyTheme()`.
- **Selector syntax:** all 8 selector blocks are `[data-theme="..."][data-style="..."]` rooted at `html`. Scoped blocks like `:root, [data-theme="light"]` from the old `main.css` are removed (their tokens live in `themes.css`).
- **Storage keys:** light/dark stays `gc-theme`. New style key is `gc-style`. Default `gc-style` = `"minimal"`.
- **Preview thumbnails:** copy the 4 light-mode mockups from `mockups/`. Do NOT generate fresh images — reuse what's already on disk.
- **i18n:** all user-facing strings go through `t(...)` — both `en.ts` and `zh-CN.ts` get the new keys.
- **i18n key shape:** `settings.appearance.{title,hint,minimal,glassmorphism,notion,bento}`.
- **Hard reload survival:** the chosen style survives Cmd+Shift+R and tab-close.
- **Scope guard:** no backend changes; no changes to TopBar; no changes to other Settings sections.
- **Verification:** before each commit, the dev server must compile without errors (`npm run dev` or `npm run build`); end-to-end manual test passes after Task 8.

## File Structure

```
webui/
├── public/
│   └── themes/                                # NEW
│       ├── minimal.jpg                        # copy of mockups/01_minimal-light.jpg
│       ├── glassmorphism.jpg                  # copy of mockups/03_glassmorphism-light.jpg
│       ├── notion.jpg                         # copy of mockups/05_notion-light.jpg
│       └── bento.jpg                          # copy of mockups/07_bento-light.jpg
└── src/
    ├── components/
    │   └── config/
    │       └── SettingsView.vue               # MODIFY (+ Appearance section)
    ├── composables/
    │   └── useTheme.ts                        # MODIFY (extend existing)
    ├── locales/
    │   ├── en.ts                              # MODIFY (+ 6 keys)
    │   └── zh-CN.ts                           # MODIFY (+ 6 keys)
    ├── styles/
    │   ├── main.css                           # MODIFY (remove token blocks)
    │   └── themes.css                         # NEW (8 selector blocks)
    └── main.ts                                # MODIFY (import order + applyStyle preload)
```

Files NOT touched: `App.vue` (the existing composable's `watch` handles the DOM update), anything under `src/agent`, anything in `src/tools`, anything in Rust.

---

### Task 1: Copy preview thumbnails into `webui/public/themes/`

**Files:**
- Create: `webui/public/themes/minimal.jpg` (copy of `mockups/01_minimal-light.jpg`)
- Create: `webui/public/themes/glassmorphism.jpg` (copy of `mockups/03_glassmorphism-light.jpg`)
- Create: `webui/public/themes/notion.jpg` (copy of `mockups/05_notion-light.jpg`)
- Create: `webui/public/themes/bento.jpg` (copy of `mockups/07_bento-light.jpg`)

**Prerequisites:** none (Phase 1 produced the mockups).

- [ ] **Step 1: Verify source files exist**

```bash
ls -la /home/hhhh/Graph-Centric/mockups/01_minimal-light.jpg \
       /home/hhhh/Graph-Centric/mockups/03_glassmorphism-light.jpg \
       /home/hhhh/Graph-Centric/mockups/05_notion-light.jpg \
       /home/hhhh/Graph-Centric/mockups/07_bento-light.jpg
```

Expected: 4 lines, each with size > 100KB.

- [ ] **Step 2: Create the destination directory and copy**

```bash
mkdir -p /home/hhhh/Graph-Centric/webui/public/themes
cp /home/hhhh/Graph-Centric/mockups/01_minimal-light.jpg       /home/hhhh/Graph-Centric/webui/public/themes/minimal.jpg
cp /home/hhhh/Graph-Centric/mockups/03_glassmorphism-light.jpg /home/hhhh/Graph-Centric/webui/public/themes/glassmorphism.jpg
cp /home/hhhh/Graph-Centric/mockups/05_notion-light.jpg        /home/hhhh/Graph-Centric/webui/public/themes/notion.jpg
cp /home/hhhh/Graph-Centric/mockups/07_bento-light.jpg         /home/hhhh/Graph-Centric/webui/public/themes/bento.jpg
ls -la /home/hhhh/Graph-Centric/webui/public/themes
```

Expected: 4 files, sizes matching the source.

- [ ] **Step 3: Confirm files are identical (no corruption on copy)**

```bash
md5sum /home/hhhh/Graph-Centric/mockups/01_minimal-light.jpg /home/hhhh/Graph-Centric/webui/public/themes/minimal.jpg
md5sum /home/hhhh/Graph-Centric/mockups/03_glassmorphism-light.jpg /home/hhhh/Graph-Centric/webui/public/themes/glassmorphism.jpg
md5sum /home/hhhh/Graph-Centric/mockups/05_notion-light.jpg /home/hhhh/Graph-Centric/webui/public/themes/notion.jpg
md5sum /home/hhhh/Graph-Centric/mockups/07_bento-light.jpg /home/hhhh/Graph-Centric/webui/public/themes/bento.jpg
```

Expected: each pair has matching hashes.

- [ ] **Step 4: Commit**

```bash
cd /home/hhhh/Graph-Centric
git add webui/public/themes
git commit -m "feat(webui): copy 4 style preview thumbnails into public/themes

Reuses the light-mode mockup images from Phase 1 (no new generation);
each is the source of truth for the corresponding style's picker card.

Co-Authored-By: Claude <noreply@anthropic.com>"
git push origin main 2>&1 | tail -3
```

---

### Task 2: Extend `useTheme.ts` to manage `data-style`

**Files:**
- Modify: `webui/src/composables/useTheme.ts` (replace contents with extended version)

**Prerequisites:** Task 1 complete.

The composable becomes responsible for BOTH the existing light/dark `theme` axis and the new `style` axis. Existing exports (`theme`, `applyTheme`, `useTheme → {theme, toggleTheme}`) stay intact.

- [ ] **Step 1: Replace `webui/src/composables/useTheme.ts` with this exact content**

```ts
import { ref, watch } from 'vue'

/* ------------------------------ data-theme axis ----------------------------- */

export type Theme = 'light' | 'dark'

const THEME_STORAGE_KEY = 'gc-theme'
const DEFAULT_THEME: Theme = 'dark'

function initialTheme(): Theme {
  const saved = localStorage.getItem(THEME_STORAGE_KEY)
  if (saved === 'light' || saved === 'dark') return saved
  return DEFAULT_THEME
}

export const theme = ref<Theme>(initialTheme())

/** 把当前主题写到 <html data-theme>。在 mount 前调用一次,避免首帧闪白。 */
export function applyTheme(t: Theme = theme.value) {
  document.documentElement.setAttribute('data-theme', t)
}

watch(theme, (t) => {
  applyTheme(t)
  try { localStorage.setItem(THEME_STORAGE_KEY, t) } catch { /* */ }
})

/* ------------------------------ data-style axis ----------------------------- */

export type StyleId = 'minimal' | 'glassmorphism' | 'notion' | 'bento'
export const STYLES: StyleId[] = ['minimal', 'glassmorphism', 'notion', 'bento']

const STYLE_STORAGE_KEY = 'gc-style'
const DEFAULT_STYLE: StyleId = 'minimal'

function normalizeStyle(v: string | null): StyleId {
  return (STYLES as string[]).includes(v ?? '') ? (v as StyleId) : DEFAULT_STYLE
}

function initialStyle(): StyleId {
  try {
    return normalizeStyle(localStorage.getItem(STYLE_STORAGE_KEY))
  } catch {
    return DEFAULT_STYLE
  }
}

export const style = ref<StyleId>(initialStyle())

/** 把当前风格写到 <html data-style>。在 mount 前调用一次,避免首帧默认态闪现。 */
export function applyStyle(s: StyleId = style.value) {
  document.documentElement.setAttribute('data-style', s)
}

watch(style, (s) => {
  applyStyle(s)
  try { localStorage.setItem(STYLE_STORAGE_KEY, s) } catch { /* */ }
})

/* ---------------------------------- useTheme -------------------------------- */

export function useTheme() {
  function toggleTheme() {
    theme.value = theme.value === 'dark' ? 'light' : 'dark'
  }
  function setStyle(s: StyleId) {
    if (!(STYLES as string[]).includes(s)) return
    style.value = s
  }
  return { theme, style, toggleTheme, setStyle, STYLES }
}
```

- [ ] **Step 2: Verify the file compiles (lightweight check)**

```bash
cd /home/hhhh/Graph-Centric/webui
test -f src/composables/useTheme.ts && wc -l src/composables/useTheme.ts
grep -E "^export (type|const|function)" src/composables/useTheme.ts
```

Expected: file size > 50 lines; expected exports present:
```
export type Theme
export const theme
export function applyTheme
export type StyleId
export const STYLES
export const style
export function applyStyle
export function useTheme
```

- [ ] **Step 3: Commit**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/composables/useTheme.ts
git commit -m "feat(webui): extend useTheme with style axis (data-style)

Adds StyleId enum, module-scope style ref, applyStyle() preload helper,
and setStyle() on useTheme(). Existing theme/toggleTheme/applyTheme API
unchanged so existing callers (TopBar, main.ts) keep working.

Co-Authored-By: Claude <noreply@anthropic.com>"
git push origin main 2>&1 | tail -3
```

---

### Task 3: Write `webui/src/styles/themes.css`

**Files:**
- Create: `webui/src/styles/themes.css`

**Prerequisites:** Task 2 complete (so `applyStyle` will resolve to the same string values that this file's selector targets).

- [ ] **Step 1: Create `webui/src/styles/themes.css` with this exact content**

```css
/* 8 selector blocks (4 styles × 2 modes).
   Each redefines ALL custom-property tokens the app uses, so the rest of the
   codebase keeps reading `var(--accent)` etc. and gets the right values for
   the currently active (theme, style) combination. */

[data-theme="light"][data-style="minimal"] {
  --bg: #f5f5f0;
  --bg-panel: #ffffff;
  --bg-hover: #f0ede8;
  --border: #e0ddd6;
  --text: #1a1a2e;
  --text-muted: #787878;
  --accent: #7c3aed;
  --accent-hover: #6d28d9;
  --accent-soft: #f5f3ff;
  --danger: #dc2626;
  --danger-soft: #fef2f2;
  --success: #059669;
  --success-soft: #ecfdf5;
  --warning: #d97706;
  --warning-soft: #fffbeb;
  --shadow: 0 1px 3px rgba(0, 0, 0, 0.06), 0 1px 2px rgba(0, 0, 0, 0.04);
  --shadow-md: 0 4px 6px rgba(0, 0, 0, 0.05), 0 2px 4px rgba(0, 0, 0, 0.04);
  --radius: 8px;
  --font: 'Inter', -apple-system, 'Segoe UI', system-ui, sans-serif;
  --font-mono: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
}

[data-theme="dark"][data-style="minimal"] {
  --bg: #0f1117;
  --bg-panel: #151823;
  --bg-hover: #1c2030;
  --border: #232838;
  --text: #e5e7eb;
  --text-muted: #8b8fa3;
  --accent: #a78bfa;
  --accent-hover: #b9a3fc;
  --accent-soft: #1e1b3a;
  --danger: #f87171;
  --danger-soft: #2a1517;
  --success: #34d399;
  --success-soft: #0f2922;
  --warning: #fbbf24;
  --warning-soft: #2a2310;
  --shadow: 0 1px 3px rgba(0, 0, 0, 0.4), 0 1px 2px rgba(0, 0, 0, 0.3);
  --shadow-md: 0 4px 12px rgba(0, 0, 0, 0.5), 0 2px 4px rgba(0, 0, 0, 0.3);
  --radius: 8px;
  --font: 'Inter', -apple-system, 'Segoe UI', system-ui, sans-serif;
  --font-mono: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
}

[data-theme="light"][data-style="glassmorphism"] {
  --bg: #e8e4f8;
  --bg-panel: rgba(255, 255, 255, 0.55);
  --bg-hover: rgba(255, 255, 255, 0.75);
  --border: rgba(124, 58, 237, 0.20);
  --text: #1a1a2e;
  --text-muted: #6b6580;
  --accent: #7c3aed;
  --accent-hover: #6d28d9;
  --accent-soft: rgba(124, 58, 237, 0.12);
  --danger: #dc2626;
  --danger-soft: rgba(220, 38, 38, 0.12);
  --success: #059669;
  --success-soft: rgba(5, 150, 105, 0.14);
  --warning: #d97706;
  --warning-soft: rgba(217, 119, 6, 0.14);
  --shadow: 0 4px 16px rgba(124, 58, 237, 0.12), 0 1px 3px rgba(0, 0, 0, 0.04);
  --shadow-md: 0 12px 32px rgba(124, 58, 237, 0.20), 0 4px 12px rgba(124, 58, 237, 0.10);
  --radius: 16px;
  --font: 'Inter', -apple-system, 'Segoe UI', system-ui, sans-serif;
  --font-mono: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
}

[data-theme="dark"][data-style="glassmorphism"] {
  --bg: #0a0c14;
  --bg-panel: rgba(21, 24, 35, 0.55);
  --bg-hover: rgba(167, 139, 250, 0.12);
  --border: rgba(167, 139, 250, 0.25);
  --text: #e5e7eb;
  --text-muted: #8b8fa3;
  --accent: #a78bfa;
  --accent-hover: #b9a3fc;
  --accent-soft: rgba(167, 139, 250, 0.15);
  --danger: #f87171;
  --danger-soft: rgba(248, 113, 113, 0.15);
  --success: #34d399;
  --success-soft: rgba(52, 211, 153, 0.15);
  --warning: #fbbf24;
  --warning-soft: rgba(251, 191, 36, 0.15);
  --shadow: 0 4px 16px rgba(167, 139, 250, 0.20), 0 1px 3px rgba(0, 0, 0, 0.30);
  --shadow-md: 0 12px 32px rgba(167, 139, 250, 0.30), 0 4px 12px rgba(0, 0, 0, 0.40);
  --radius: 16px;
  --font: 'Inter', -apple-system, 'Segoe UI', system-ui, sans-serif;
  --font-mono: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
}

[data-theme="light"][data-style="notion"] {
  --bg: #f7f3ec;
  --bg-panel: #ffffff;
  --bg-hover: #efe9df;
  --border: #e6dfd3;
  --text: #2c2620;
  --text-muted: #8b7e6e;
  --accent: #c4694a;
  --accent-hover: #a85738;
  --accent-soft: #f9ede6;
  --danger: #c43c3c;
  --danger-soft: #fbeaea;
  --success: #4a8a5c;
  --success-soft: #ebf5ee;
  --warning: #b87a2b;
  --warning-soft: #fbf2e3;
  --shadow: 0 2px 8px rgba(60, 40, 20, 0.08);
  --shadow-md: 0 6px 16px rgba(60, 40, 20, 0.12);
  --radius: 12px;
  --font: 'Inter', -apple-system, 'Segoe UI', system-ui, sans-serif;
  --font-mono: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
}

[data-theme="dark"][data-style="notion"] {
  --bg: #2a2520;
  --bg-panel: #3a322a;
  --bg-hover: #443c33;
  --border: #4d443a;
  --text: #ece2d3;
  --text-muted: #a89882;
  --accent: #d99770;
  --accent-hover: #e7a784;
  --accent-soft: #3d2e22;
  --danger: #e88a8a;
  --danger-soft: #4a2626;
  --success: #8cc099;
  --success-soft: #1f3a28;
  --warning: #e0b06a;
  --warning-soft: #3d2f18;
  --shadow: 0 2px 8px rgba(0, 0, 0, 0.35);
  --shadow-md: 0 6px 16px rgba(0, 0, 0, 0.45);
  --radius: 12px;
  --font: 'Inter', -apple-system, 'Segoe UI', system-ui, sans-serif;
  --font-mono: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
}

[data-theme="light"][data-style="bento"] {
  --bg: #f5f7fa;
  --bg-panel: #ffffff;
  --bg-hover: #ebeef3;
  --border: #d8dee9;
  --text: #1a1a2e;
  --text-muted: #6a7585;
  --accent: #2563eb;
  --accent-hover: #1d4ed8;
  --accent-soft: #e0eaff;
  --danger: #d93636;
  --danger-soft: #fde8e8;
  --success: #1f8a4f;
  --success-soft: #e3f5ea;
  --warning: #b26407;
  --warning-soft: #faecc8;
  --shadow: 0 1px 2px rgba(20, 30, 50, 0.06);
  --shadow-md: 0 2px 4px rgba(20, 30, 50, 0.08), 0 1px 2px rgba(20, 30, 50, 0.04);
  --radius: 6px;
  --font: 'Inter', -apple-system, 'Segoe UI', system-ui, sans-serif;
  --font-mono: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
}

[data-theme="dark"][data-style="bento"] {
  --bg: #0d1117;
  --bg-panel: #161b22;
  --bg-hover: #21262d;
  --border: #30363d;
  --text: #c9d1d9;
  --text-muted: #8b949e;
  --accent: #58a6ff;
  --accent-hover: #79b8ff;
  --accent-soft: #1c2330;
  --danger: #ff7b72;
  --danger-soft: #3a1c19;
  --success: #56d364;
  --success-soft: #14301c;
  --warning: #e3b341;
  --warning-soft: #3a2c12;
  --shadow: 0 1px 2px rgba(0, 0, 0, 0.45);
  --shadow-md: 0 2px 4px rgba(0, 0, 0, 0.55), 0 1px 2px rgba(0, 0, 0, 0.40);
  --radius: 6px;
  --font: 'Inter', -apple-system, 'Segoe UI', system-ui, sans-serif;
  --font-mono: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
}
```

- [ ] **Step 2: Verify file exists and has 8 selector blocks**

```bash
wc -l /home/hhhh/Graph-Centric/webui/src/styles/themes.css
grep -c '^\[data-theme=".*"\]\[data-style=".*"\] {$' /home/hhhh/Graph-Centric/webui/src/styles/themes.css
```

Expected: line count > 100; selector count = `8`.

- [ ] **Step 3: Commit**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/styles/themes.css
git commit -m "feat(webui): add themes.css with 8 (theme x style) selector blocks

Each block redefines all design tokens so the rest of the codebase
keeps reading var(--accent) etc. Light/dark pairs are preserved within
each style, so 4 styles x 2 modes = 8 visual states are addressable.

Co-Authored-By: Claude <noreply@anthropic.com>"
git push origin main 2>&1 | tail -3
```

---

### Task 4: Strip token blocks from `main.css`

**Files:**
- Modify: `webui/src/styles/main.css` (remove lines 1-45 of the original `:root, [data-theme="light"]` / `[data-theme="dark"]` blocks; keep everything from `html { color-scheme: light dark; }` onward)

**Prerequisites:** Task 3 complete (the token values now live in `themes.css`).

Critical: this step **must happen in the same window** as Task 5 (the `main.ts` import-order change). Do Task 5 immediately after this one and check both with a build / dev server before committing Task 4 or 5 alone — if `themes.css` doesn't load, removing `main.css`'s tokens leaves the app with no `--accent` / `--bg` defined and the page renders broken until the next reload. The dev server cannot hot-reload partial CSS reliably, so a full reload is needed between Task 4 and Task 5.

- [ ] **Step 1: Open `main.css` and find the boundary**

The current file starts with these two blocks (lines 1-45):

```css
:root, [data-theme="light"] {
  ... --font, --font-mono ...
}

[data-theme="dark"] {
  ...
}
```

Then `html { color-scheme: light dark; }` and onward.

- [ ] **Step 2: Replace lines 1-45 with this exact replacement**

Delete the entire `:root, [data-theme="light"] { ... }` block (the first 22 lines plus the closing `}`) and the entire `[data-theme="dark"] { ... }` block (the next 22 lines). Keep everything else as-is. The file should now START at:

```css
html { color-scheme: light dark; }
```

Resulting file structure after this change:

```css
/* Token values live in themes.css (8 selector blocks for 4 styles × 2 modes)
   and are imported BEFORE this file in main.ts so they cascade in here. */

/* keep this line */
html { color-scheme: light dark; }

/* keep this line */
* { box-sizing: border-box; margin: 0; padding: 0; }

/* keep this and all following rules unchanged */
body { ... }
::-webkit-scrollbar { ... }
.status-pill { ... }
button { ... }
... etc.
```

Use Edit tool. The exact replacement: take the full file content (122 lines), strip the two token blocks, and write back. The simplest way is to read the file, remove lines 1-45, prepend a 3-line comment explaining the move, and write back.

- [ ] **Step 3: Verify the result**

```bash
cd /home/hhhh/Graph-Centric/webui
grep -c "^:root\|^:root,\|^[data-theme=" src/styles/main.css
head -10 src/styles/main.css
```

Expected: count is `0`; the file's first 10 lines should start with a comment about tokens moving to themes.css and then `html { color-scheme: light dark; }`.

- [ ] **Step 4: Commit — but DO NOT push yet**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/styles/main.css
git commit -m "refactor(webui): strip token blocks from main.css

Tokens have moved to themes.css (8 selector blocks for 4 styles x 2
modes). main.css now starts past the token definitions and references
them via var(--*).

Co-Authored-By: Claude <noreply@anthropic.com>"
```

(Hold the push until Task 5 is also done — pushing partial CSS state on a public
branch means anyone who pulls gets a half-broken page until Task 5 lands.)

---

### Task 5: Update `main.ts` to import `themes.css` first and apply style pre-mount

**Files:**
- Modify: `webui/src/main.ts` (add `themes.css` import above `main.css`, add `applyStyle()` call)

**Prerequisites:** Task 4 complete (the partial CSS state is local; the dev server can recover).

- [ ] **Step 1: Edit `webui/src/main.ts` — change the import + preload call**

Old:

```ts
import { applyTheme } from './composables/useTheme'
import './styles/main.css'
```

New:

```ts
import { applyTheme, applyStyle } from './composables/useTheme'
import './styles/themes.css'
import './styles/main.css'
```

And in the body of the file (currently line 23), add `applyStyle()` next to the existing `applyTheme()` call so the order matches: `applyTheme()` first (light/dark), then `applyStyle()`. The existing code is:

```ts
applyTheme() // 在挂载前应用主题,避免首帧闪白
createApp(App).use(router).mount('#app')
```

Update to:

```ts
applyTheme()  // 在挂载前应用主题,避免首帧闪白
applyStyle()  // 同理:首帧前同步 data-style,避免默认态闪现
createApp(App).use(router).mount('#app')
```

- [ ] **Step 2: Build / dev sanity check**

```bash
cd /home/hhhh/Graph-Centric/webui
npm run build 2>&1 | tail -20
```

Expected: build succeeds (errors would show TS or import errors). If the build warns about anything in `themes.css` (it shouldn't — it's plain CSS), fix and rebuild.

- [ ] **Step 3: Verify the dev server starts and reports no errors**

```bash
cd /home/hhhh/Graph-Centric/webui
timeout 8 npm run dev 2>&1 | head -20 || true
```

Expected: `Vite ... ready in ...ms` line is printed, no compilation errors. (The `timeout 8` lets the command exit on its own after 8 seconds so we don't hang the bash session.)

- [ ] **Step 4: Push Tasks 4 + 5 together**

```bash
cd /home/hhhh/Graph-Centric
git push origin main 2>&1 | tail -3
```

(Both the `main.css` and `main.ts` changes go out in one push. Their order is
already correct in the repo because Task 4 was committed locally first; the push
ships both diffs atomically.)

---

### Task 6: Add Appearance section to `SettingsView.vue`

**Files:**
- Modify: `webui/src/components/config/SettingsView.vue`
  - Add `useTheme` import + `STYLES`, `currentStyle`, `setStyle` references in `<script setup>`
  - Add `<section class="appearance-section">` block BEFORE the existing `<section class="heartbeat-section">` (or AFTER, near other UI controls — your call based on visual flow)
  - Add scoped CSS for `.appearance-section`, `.theme-grid`, `.theme-card`, `.theme-card.active`

**Prerequisites:** Tasks 2 + 5 complete (`useTheme` returns `style`, `setStyle`, `STYLES`; main.ts preload works).

- [ ] **Step 1: Modify the `<script setup>` block**

At the top of the existing `<script setup lang="ts">`, add (right after the existing `useI18n` import):

```ts
import { useTheme, type StyleId } from '../../composables/useTheme'
const { style: currentStyle, setStyle, STYLES } = useTheme()
```

Note the renaming: `useTheme()` returns `style`, but inside this component we alias it to `currentStyle` to avoid shadowing the property name in the template.

- [ ] **Step 2: Insert the new section in the template**

Insert this new `<section>` right after `<h2>{{ t('settings.title') }}</h2>` (the section ordering: title → Appearance → Heartbeat → Profiles → Model → ... etc.) OR right before the Heartbeat section. Either way, it's clearly the FIRST section.

```vue
<section class="appearance-section">
  <h3>{{ t('settings.appearance.title') }}</h3>
  <p class="hint">{{ t('settings.appearance.hint') }}</p>
  <div class="theme-grid">
    <button
      v-for="s in STYLES"
      :key="s"
      type="button"
      :class="['theme-card', { active: currentStyle === s }]"
      :aria-pressed="currentStyle === s"
      @click="setStyle(s)"
    >
      <img :src="`/themes/${s}.jpg`" :alt="s" loading="lazy" />
      <span class="theme-name">{{ t(`settings.appearance.${s}`) }}</span>
    </button>
  </div>
</section>
```

- [ ] **Step 3: Add scoped CSS to `<style scoped>` (at the bottom of the file)**

Append these rules (don't modify the existing styles):

```css
.appearance-section { /* same look as other sections; tokens handle it */ }
.theme-grid {
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: 12px;
  margin-top: 12px;
}
@media (max-width: 900px) {
  .theme-grid { grid-template-columns: repeat(2, 1fr); }
}
.theme-card {
  display: flex;
  flex-direction: column;
  align-items: stretch;
  padding: 8px;
  border: 2px solid var(--border);
  border-radius: var(--radius);
  background: var(--bg-panel);
  cursor: pointer;
  transition: transform 0.15s ease, border-color 0.15s ease;
  font: inherit;
  color: inherit;
}
.theme-card:hover { transform: translateY(-2px); border-color: var(--accent); }
.theme-card.active {
  border-color: var(--accent);
  box-shadow: 0 0 0 3px var(--accent-soft);
}
.theme-card img {
  width: 100%;
  aspect-ratio: 1440 / 896;
  object-fit: cover;
  object-position: top center;
  border-radius: calc(var(--radius) - 2px);
  display: block;
}
.theme-name {
  margin-top: 6px;
  font-size: 0.8rem;
  font-weight: 500;
  text-align: center;
  color: var(--text);
}
```

- [ ] **Step 4: Verify the Vue template still compiles**

```bash
cd /home/hhhh/Graph-Centric/webui
npm run build 2>&1 | tail -10
```

Expected: build passes; no template errors about `currentStyle` / `setStyle` / `STYLES` undefined.

- [ ] **Step 5: Commit**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/components/config/SettingsView.vue
git commit -m "feat(webui): add Appearance section with 4-style thumbnail picker

4 cards in a CSS grid (responsive -> 2-col on narrow), each renders
the preview thumbnail reused from webui/public/themes/. Click sets
data-style; useTheme's existing watch persists to localStorage and
syncs the DOM.

Co-Authored-By: Claude <noreply@anthropic.com>"
git push origin main 2>&1 | tail -3
```

---

### Task 7: Add `settings.appearance.*` keys to both locales

**Files:**
- Modify: `webui/src/locales/en.ts` (add 6 keys under `settings`)
- Modify: `webui/src/locales/zh-CN.ts` (add 6 keys under `settings`)

**Prerequisites:** Task 6 references the keys; both locales must provide them or the page falls back to English literals.

The exact same 6 keys go into both files:

```ts
appearance: {
  title: 'Appearance',           // zh-CN: '外观'
  hint: 'Pick a visual style. Light / dark is controlled elsewhere.',  // zh-CN: '选择视觉风格。明暗模式在其他位置单独控制。'
  minimal: 'Minimal',           // zh-CN: '极简'
  glassmorphism: 'Glassmorphism', // zh-CN: '玻璃拟态'
  notion: 'Notion',              // zh-CN: 'Notion'
  bento: 'Bento',                // zh-CN: 'Bento'
},
```

- [ ] **Step 1: Open `en.ts`**

Locate the `settings: { ... }` block (probably near the end of the file). Add the `appearance` sub-object inside it, alongside `title`, `model`, `policy`, `loopTuning`, etc.

- [ ] **Step 2: Open `zh-CN.ts`**

Same — locate `settings: { ... }` and add the same `appearance` sub-object with translated values.

- [ ] **Step 3: Verify the locale TS modules still compile**

```bash
cd /home/hhhh/Graph-Centric/webui
npm run build 2>&1 | tail -10
```

Expected: build passes; no TypeScript errors from missing `appearance` keys.

- [ ] **Step 4: Commit**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/locales/en.ts webui/src/locales/zh-CN.ts
git commit -m "i18n(webui): add settings.appearance.* keys (en + zh-CN)

Coverage: title, hint, plus 4 style name labels (minimal / glassmorphism
/ notion / bento). Both locales updated; English text used as fallback
if a key is missing.

Co-Authored-By: Claude <noreply@anthropic.com>"
git push origin main 2>&1 | tail -3
```

---

### Task 8: End-to-end manual verification

**Files:** none — this is validation only.

**Prerequisites:** Tasks 1-7 all pushed to `main`.

Manual checklist. Run the dev server first:

```bash
cd /home/hhhh/Graph-Centric/webui
npm run dev &
DEV_PID=$!
sleep 4
echo "dev server pid: $DEV_PID"
```

(The server stays in background; kill it after this task with `kill $DEV_PID`.)

- [ ] **Step 1: Default load renders correctly**

Browse to `http://localhost:5173/settings` (Vite default port). Verify:
- Header reads "Appearance" with the hint text.
- 4 thumbnail cards visible (minimal, glassmorphism, notion, bento).
- The thumbnail labeled "Minimal" has the active border (since default = minimal).
- Other 3 thumbnails show their respective mockup variants.
- No console errors (F12 → Console).

- [ ] **Step 2: Style switch is instant**

Click each thumbnail in sequence (glassmorphism, notion, bento, minimal back):
- The whole UI's color, radius, shadow, font-mono accents visibly flip.
- The active-card border moves to the clicked card.
- Switch latency is < 100ms (no flash, no white frame).

- [ ] **Step 3: Light/dark × style grid — toggle mode then switch style**

Click the existing theme toggle (or open settings, switch data-theme via DevTools console: `document.documentElement.dataset.theme = 'light'`). Verify both modes for each style:
- For each style, in both light and dark mode, every page's contrast is acceptable (text on bg ≥ 4.5:1 in body text).
- Specifically check graph view (cytoscape) — does the rendered graph pick up the new colors or look stuck on old ones?

- [ ] **Step 4: Persistence**

With the chosen style = bento (or any non-default), F5 reload the page:
- `<html>` still has `data-style="bento"` (verify via DevTools: `document.documentElement.dataset.style`).
- The Settings page card "Bento" is highlighted.

- [ ] **Step 5: Hard-reload + tab-close survival**

Same as Step 4 but with Cmd+Shift+R and after closing/reopening the tab.

- [ ] **Step 6: localStorage disabled (private browsing) emulation**

In DevTools → Application → Storage → "Clear site data", OR set browser to block localStorage. Reload:
- Page still renders — picked up the default `minimal`.
- Choosing another style applies but doesn't persist (expected).
- No console errors about localStorage.

- [ ] **Step 7: i18n roundtrip**

In the existing nav, switch language (if a switcher exists; otherwise edit the locale code path). Verify the picker shows translated labels (外观 / 极简 / 玻璃拟态 / Notion / Bento).

- [ ] **Step 8: Stop dev server**

```bash
DEV_PID=$(lsof -ti:5173 || true)
[ -n "$DEV_PID" ] && kill $DEV_PID
echo "dev server stopped"
```

- [ ] **Step 9: Final report**

Tell the user (in chat) what passed and any rough edges observed. If everything is green, propose stopping the mockup server from Phase 1:

```bash
MOCK_PID=$(lsof -ti:8090 || true)
[ -n "$MOCK_PID" ] && kill $MOCK_PID && echo "mockup server stopped" || echo "mockup server already down"
```

## Self-Review Checklist (run before reporting done)

- [ ] `webui/public/themes/` has 4 JPEGs (~105-175 KB each) with matching md5sums to source `mockups/0X_<style>-light.jpg`.
- [ ] `useTheme.ts` exports `theme`, `style`, `STYLES`, `applyTheme`, `applyStyle`, `useTheme → {theme, style, toggleTheme, setStyle, STYLES}`. `setStyle` rejects unknown values.
- [ ] `themes.css` has exactly 8 selector blocks. Each block redefines all 19 custom-property tokens (--bg, --bg-panel, --bg-hover, --border, --text, --text-muted, --accent, --accent-hover, --accent-soft, --danger, --danger-soft, --success, --success-soft, --warning, --warning-soft, --shadow, --shadow-md, --radius, --font, --font-mono).
- [ ] `main.css` no longer defines tokens (no `:root` or `[data-theme]` rules).
- [ ] `main.ts` imports `./styles/themes.css` BEFORE `./styles/main.css`, and calls `applyTheme()` + `applyStyle()` before mounting.
- [ ] `SettingsView.vue` renders the new section in dev server with no console errors.
- [ ] Both `en.ts` and `zh-CN.ts` have the `settings.appearance.*` keys.
- [ ] All commits pushed to `origin/main`.
- [ ] Choosing each of 4 styles × 2 modes gives 8 visually distinct, acceptable-contrast renderings.
- [ ] Persistence roundtrip works (style survives reload).

## Out-of-Scope Reminder

Implementing this plan does NOT include:

- Per-page style overrides.
- Custom style editor.
- Removing the dark-mode toggle (it stays).
- Generating dark-mode preview thumbnails.
- A 5th style (e.g. Cyberpunk).
