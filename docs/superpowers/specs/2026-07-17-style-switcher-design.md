# Style Switcher — Design

**Date:** 2026-07-17
**Status:** Approved (pending user spec review)
**Owner:** user + assistant

## Goal

Surface all four visual styles from the mockup gallery (Minimal, Glassmorphism, Notion,
Bento) as **runtime switchable themes** in the actual Graph-Centric webui. The existing
light/dark `[data-theme]` toggle keeps working unchanged; the new dimension is the
**style** axis. The user picks a style from a Settings-page thumbnail picker; the choice
persists in localStorage; CSS variables cascade the look across every component
without per-component code changes.

## Scope

In scope:

- New CSS variable layer (`themes.css`) with 4 styles × 2 modes = 8 selector blocks.
- New `useTheme.ts` composable managing `data-style` on `<html>` + persistence.
- One new section ("外观 / Appearance") in `SettingsView.vue` with 4 thumbnail cards.
- Reuse the 4 light-mode mockup images as preview thumbnails (copied into `webui/public/themes/`).
- Default style on first load: `minimal` (closest to current look, lowest churn for users
  who never touch the picker).

Out of scope:

- Per-route overrides (every page picks up the same style).
- Custom style editor or user-defined tokens.
- Per-component theme tweaks beyond what `themes.css` already controls.
- Dark-mode picker redesign (current toggle stays).
- Backend changes (this is purely a frontend / CSS refactor).

## Architecture

```
                ┌───────────────────────────────────────────┐
                │  localStorage  "gc-style" = "minimal"     │   (persisted)
                └─────────────┬─────────────────────────────┘
                              │
                       init / setStyle
                              │
                ┌─────────────▼─────────────────────────────┐
                │  useTheme (composable, reactive ref)      │
                │   currentStyle: Ref<'minimal'|'glass'...> │
                │   setStyle(s): persist + emit             │
                └─────────────┬─────────────────────────────┘
                              │ bind to <html data-style>
                              │
                ┌─────────────▼─────────────────────────────┐
                │  document.documentElement                 │
                │  data-style="minimal"  data-theme="light" │
                └─────────────┬─────────────────────────────┘
                              │ CSS selector hits
                              │
                ┌─────────────▼─────────────────────────────┐
                │  themes.css                               │
                │  [data-theme="light"][data-style="..."] { │
                │    --accent: ...;                         │
                │    ...                                    │
                │  }                                        │
                └───────────────────────────────────────────┘
```

Two orthogonal axes:

- `data-theme` (existing): `light` | `dark`
- `data-style` (new):      `minimal` | `glassmorphism` | `notion` | `bento`

The product: `8` visual states.

## Files

### Created

| Path | Purpose |
|---|---|
| `webui/src/styles/themes.css` | 4 styles × 2 modes of CSS variable overrides. |
| `webui/src/composables/useTheme.ts` | Reactive `currentStyle`, `setStyle`, normalization, fallback, persistence. |
| `webui/public/themes/minimal.jpg` | Preview thumbnail — copy of `mockups/01_minimal-light.jpg`. |
| `webui/public/themes/glassmorphism.jpg` | Preview thumbnail — copy of `mockups/03_glassmorphism-light.jpg`. |
| `webui/public/themes/notion.jpg` | Preview thumbnail — copy of `mockups/05_notion-light.jpg`. |
| `webui/public/themes/bento.jpg` | Preview thumbnail — copy of `mockups/07_bento-light.jpg`. |

### Modified

| Path | Change |
|---|---|
| `webui/src/styles/main.css` | Remove the `:root, [data-theme="light"]` and `[data-theme="dark"]` blocks; their tokens move to `themes.css` keyed on `[data-theme][data-style]`. Keep global reset + base typography. |
| `webui/src/App.vue` | Import `useTheme`, call `init()` on mount, add a watcher on `currentStyle` that updates `document.documentElement.dataset.style`. |
| `webui/src/components/config/SettingsView.vue` | Add a new `<section>` "外观 / Appearance" with 4 thumbnail cards. Click → `setStyle(...)`. |

### Not touched

- `webui/src/components/shared/TopBar.vue` — picker lives in Settings, not top bar.
- `webui/src/components/config/SettingsView.vue`'s other sections.
- Anything in `src/`.

## Token Migration (main.css → themes.css)

`main.css` currently defines these variables under `:root, [data-theme="light"]` and
`[data-theme="dark"]`:

```css
--bg, --bg-panel, --bg-hover, --border,
--text, --text-muted, --accent, --accent-hover, --accent-soft,
--danger, --danger-soft, --success, --success-soft,
--warning, --warning-soft,
--shadow, --shadow-md,
--radius,
--font, --font-mono
```

`themes.css` redefines ALL of these under 8 selector blocks:

```
[data-theme="light"][data-style="minimal"]       { ... }
[data-theme="dark"][data-style="minimal"]        { ... }
[data-theme="light"][data-style="glassmorphism"] { ... }
[data-theme="dark"][data-style="glassmorphism"]  { ... }
[data-theme="light"][data-style="notion"]        { ... }
[data-theme="dark"][data-style="notion"]         { ... }
[data-theme="light"][data-style="bento"]         { ... }
[data-theme="dark"][data-style="bento"]          { ... }
```

`main.css` keeps ONLY:

- `* { box-sizing; margin: 0; padding: 0; }`
- `body { font-family: var(--font); ... }`
- `::-webkit-scrollbar` rules
- Status pill defaults
- Button / input / link base styles
- `.md-body` markdown styling

Even those rules reference `var(--font)` etc., which resolve from `themes.css` once it's
loaded. Order matters: `themes.css` must be imported BEFORE `main.css` so its tokens win
on the cascade.

## Style Token Palettes (initial values)

These are starting points; final values are tuned during implementation.

### Minimal

```
light: --bg #f5f5f0; --bg-panel #ffffff; --bg-hover #f0ede8; --border #e0ddd6;
       --text #1a1a2e; --text-muted #787878; --accent #7c3aed; --radius 8px;
       --font 'Inter', system-ui, sans-serif;
dark:  --bg #0f1117; --bg-panel #151823; --bg-hover #1c2030; --border #232838;
       --text #e5e7eb; --text-muted #8b8fa3; --accent #a78bfa; --radius 8px;
```

### Glassmorphism

```
light: --bg #e8e4f8; --bg-panel rgba(255,255,255,0.55); --bg-hover rgba(255,255,255,0.7);
       --border rgba(124,58,237,0.18); --text #1a1a2e; --accent #7c3aed;
       --radius 16px; --shadow-md 0 8px 32px rgba(124,58,237,0.18);
dark:  --bg #0a0c14; --bg-panel rgba(21,24,35,0.55); --bg-hover rgba(167,139,250,0.12);
       --border rgba(167,139,250,0.25); --text #e5e7eb; --accent #a78bfa;
       --shadow 0 8px 32px rgba(167,139,250,0.3);
```

### Notion

```
light: --bg #f7f3ec; --bg-panel #ffffff; --bg-hover #efe9df; --border #e6dfd3;
       --text #2c2620; --text-muted #8b7e6e; --accent #c4694a; --radius 12px;
       --font 'Inter', system-ui, sans-serif; --shadow 0 2px 8px rgba(60,40,20,0.08);
dark:  --bg #2a2520; --bg-panel #3a322a; --bg-hover #443c33; --border #4d443a;
       --text #ece2d3; --text-muted #a89882; --accent #d99770; --radius 12px;
```

### Bento

```
light: --bg #f5f7fa; --bg-panel #ffffff; --bg-hover #ebeef3; --border #d8dee9;
       --text #1a1a2e; --text-muted #6a7585; --accent #2563eb; --radius 6px;
       --font-mono 'JetBrains Mono', 'Fira Code', monospace;
dark:  --bg #0d1117; --bg-panel #161b22; --bg-hover #21262d; --border #30363d;
       --text #c9d1d9; --text-muted #8b949e; --accent #58a6ff; --radius 6px;
```

The four palettes differ most in: `--bg`, `--accent`, `--radius`, `--font`/`--font-mono`,
and shadow intensity. Components consume the same variables, so the visual flip is
automatic.

## Composables API (`useTheme.ts`)

```ts
// webui/src/composables/useTheme.ts
import { ref, watch, onMounted } from 'vue'

export type StyleId = 'minimal' | 'glassmorphism' | 'notion' | 'bento'
const VALID: StyleId[] = ['minimal', 'glassmorphism', 'notion', 'bento']
const STORAGE_KEY = 'gc-style'
const DEFAULT: StyleId = 'minimal'

const currentStyle = ref<StyleId>(DEFAULT)

function normalize(v: string | null): StyleId {
  return VALID.includes(v as StyleId) ? (v as StyleId) : DEFAULT
}

function readStorage(): StyleId | null {
  try {
    return normalize(localStorage.getItem(STORAGE_KEY))
  } catch {
    return null
  }
}

function applyToHtml(s: StyleId) {
  if (typeof document !== 'undefined') {
    document.documentElement.dataset.style = s
  }
}

function setStyle(s: StyleId) {
  if (!VALID.includes(s)) return
  currentStyle.value = s
  try { localStorage.setItem(STORAGE_KEY, s) } catch { /* */ }
}

function init() {
  const stored = readStorage()
  // readStorage() always returns a valid value (normalizes).
  currentStyle.value = stored ?? DEFAULT
  applyToHtml(currentStyle.value)
}

export function useTheme() {
  return { currentStyle, setStyle, init, STYLES: VALID }
}
```

Notes:
- `init()` is safe to call multiple times.
- `setStyle` never throws — localStorage failure is swallowed with a comment.
- The `<html>` attribute is the single source of truth; `currentStyle` is the
  reactive mirror for Vue components.

## App.vue wiring

```ts
import { useTheme } from './composables/useTheme'
const { currentStyle, init } = useTheme()
onMounted(() => init())
watch(currentStyle, (v) => {
  document.documentElement.dataset.style = v
})
```

`App.vue` already imports `provide` — but the style does not need to be `provide`d,
because every component that needs the current style can read `document.documentElement.dataset.style`
directly, or — for components with style-aware logic — call `useTheme()` to get the ref.

## Settings Picker — "Appearance" section

Inserted into `SettingsView.vue` between the **Heartbeat** section and the **Profiles**
section (i.e. near the top, since it's user-facing rather than dev-facing).

```vue
<section class="appearance-section">
  <h3>{{ t('settings.appearance.title') }}</h3>
  <p class="hint">{{ t('settings.appearance.hint') }}</p>
  <div class="theme-grid">
    <button
      v-for="style in STYLES"
      :key="style"
      :class="['theme-card', { active: currentStyle === style }]"
      @click="setStyle(style)"
    >
      <img :src="`/themes/${style}.jpg`" :alt="style" loading="lazy" />
      <span class="theme-name">{{ t(`settings.appearance.${style}`) }}</span>
    </button>
  </div>
</section>
```

CSS for `.theme-grid`: 4 columns on desktop, 2 columns on <900 px. Active card gets
the `--accent` border + a small checkmark. Hover lifts the card 2 px.

## i18n Keys

Add to both `webui/src/locales/en.ts` and `webui/src/locales/zh-CN.ts`:

```ts
settings.appearance.title   = "Appearance"
settings.appearance.hint    = "Pick a visual style. Light / dark mode is controlled separately above."  // zh-CN: 选择视觉风格。明暗模式在另一处单独控制。
settings.appearance.minimal       = "Minimal"
settings.appearance.glassmorphism = "Glassmorphism"
settings.appearance.notion        = "Notion"
settings.appearance.bento         = "Bento"
```

## Error Handling

| Failure | Behavior |
|---|---|
| `localStorage.getItem` throws (private browsing) | `try/catch` returns `null`; falls through to `DEFAULT`. |
| `localStorage.setItem` throws (quota) | Silently swallowed; in-memory still works for the current session. |
| Stored value is not a known style | `normalize()` returns `DEFAULT`; value overwritten on next `setStyle`. |
| Preview image 404 (`/themes/<style>.jpg` missing) | `<img>` shows broken icon; card layout still works because button is the click target. Add a `<noscript>`-free fallback `alt` so user can still pick. |
| `useTheme` called outside Vue setup | `ref` can be created at module scope; no Vue context needed. |
| `document` undefined (SSR) | `applyToHtml` skips via `typeof document !== 'undefined'` early-return. |

This codebase has no SSR today, but the guard is cheap and forward-compatible.

## Testing

Manually verified during development:

1. **Default load**: clear localStorage, hard-reload — page renders with `minimal` tokens.
2. **Mode × style grid**: in Settings, pick `glassmorphism`, then toggle `data-theme` between `light` and `dark` — both glass versions render correctly.
3. **Persistence**: change to `bento`, reload — still on `bento`.
4. **Persistence across browsers/devices**: not required, but verify the chosen style survives Cmd+Shift+R (hard reload) and tab close / re-open.
5. **localStorage disabled**: devtools → Application → clear storage / block storage — app still picks a default and renders.
6. **Switch latency**: pick `notion` from picker — verify the visual flip is sub-100ms with no unstyled flash.
7. **Component coverage**: every existing component, including graph view (cytoscape), checkpoint viewer, history list, run detail — verify they all pick up the new tokens (none hardcode hex).
8. **i18n**: switch language en ↔ zh-CN — verify the picker labels translate.

Note: this codebase has no automated test setup for `webui/` today, so all checks are
manual visual checks. (If unit tests for the composable become useful later, a vitest
suite for `useTheme` is straightforward — out of scope for this phase.)

## Open Questions (none blocking)

- *Should the picker live in TopBar too?* — Decided: no, TopBar stays minimal. Settings is the canonical "look & feel" page.
- *Should we ship all 4 dark-mode preview thumbnails?* — Decided: no, just 4 light thumbnails. The dark-mode preview would need 4 more images and the Settings page itself already has a dark/light toggle elsewhere, so the picker is light-only by design.
- *One additional style (Cyberpunk)* — not added in this phase; the architecture supports adding a 5th style by adding one row to `themes.css` and one image to `webui/public/themes/`.

## Exit Criteria

- All 8 visual states (4 styles × 2 modes) render the existing webui as styled.
- Settings page shows 4 working thumbnail cards.
- Choice persists across reload.
- Default (no localStorage) loads on `minimal`.
- No new console errors.
- i18n keys present in both locales.
- Plan executed task-by-task with commits and pushes per project memory rules.

## File Layout (final)

```
webui/
├── public/
│   └── themes/
│       ├── minimal.jpg         ← copied from mockups/01_minimal-light.jpg
│       ├── glassmorphism.jpg   ← copied from mockups/03_glassmorphism-light.jpg
│       ├── notion.jpg          ← copied from mockups/05_notion-light.jpg
│       └── bento.jpg           ← copied from mockups/07_bento-light.jpg
└── src/
    ├── components/
    │   └── config/
    │       └── SettingsView.vue (modified: +1 section)
    ├── composables/
    │   └── useTheme.ts         (new)
    └── styles/
        ├── main.css            (modified: token definitions removed, base styles kept)
        └── themes.css          (new: 8 selector blocks)
```
