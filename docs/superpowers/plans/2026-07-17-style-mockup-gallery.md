# Style Mockup Gallery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Generate 8 design mockups (4 visual styles × 2 light/dark modes) and serve them on a LAN-reachable web page so the user can pick a winning visual direction for the Graph-Centric webui redesign.

**Architecture:** Drive the `mmx` image-generation CLI to produce 1440×900 PNG mockups of the Graph-Centric webapp shell. Drop them in a repo-root `mockups/` directory alongside a zero-dependency static HTML gallery, then serve via `python3 -m http.server` bound to `0.0.0.0:8090`. The whole pipeline is shell-driven — no Rust/TypeScript changes, no project build, no webui modifications.

**Tech Stack:** `mmx` CLI (npm, MiniMax), Python 3 stdlib `http.server`, plain HTML + CSS.

## Global Constraints

- All paths are absolute or relative to `/home/hhhh/Graph-Centric/` (the repo root).
- Image dimensions: **1440 × 900** (one image, default 16:10 — `--aspect-ratio` is not specified to mmx, default model behavior).
- Output filenames are exactly `NN_<style>-<mode>.png` with two-digit zero padding. Skipping or renaming is a hard failure.
- The HTTP server binds `0.0.0.0` (not `127.0.0.1`) so LAN clients can connect.
- Default port is `8090`. If busy, retry `8091..8099`. After 8100, abort with a clear error.
- Every mockup must depict the **same scene** (left sidebar runs, top bar, center graph, right placeholder) — only the visual style differs. This is the load-bearing requirement for "apples-to-apples" comparison.
- Style prompts include the canonical scene description verbatim (see below).
- HTML gallery must reference all 8 PNGs; verify with `curl localhost:8090/` returning text matching every filename.
- The user must be told the LAN IP and a working URL before this plan is "done."
- Per project memory: commit and push every change to `origin/main` after each task.
- Per project memory: do not branch — commit directly on `main`.
- Per project CLAUDE.md: status pills must use `--accent #7c3aed` (Running), green (Done), amber (Paused), red (Error).
- Per project CLAUDE.md: theme tokens live under `:root, [data-theme="light"]` and `[data-theme="dark"]` — do NOT modify the existing `webui/src/styles/main.css`. Mockups are standalone artifacts.

## Canonical Scene Description

Every mmx prompt MUST include this scene scaffold (style-specific clauses come AFTER it):

```
A 1440x900 web-app dashboard mockup for a tool called "Graph-Centric Agent".
The layout has four parts:
LEFT SIDEBAR (about 220px wide, full height): a header reading "Graph-Centric Agent",
a search input, then a list of 4 run items:
  - "Refactor auth flow" with a purple running dot
  - "Add CSS grid layout" with a green done dot
  - "Build skill compiler" with an amber paused dot
  - "Investigate skill match" with a red error dot
TOP BAR: a graph title "refactor/auth", a small "MiniMax-M2.7" model chip, a token counter
"3.2k / 50k", and two action buttons on the right.
CENTER: a force-directed-style graph visualization with 7 nodes connected by edges.
One node ("node_3") is highlighted/selected. Three of the nodes are purple (running),
two are green (done), one is red (error), and one is amber (paused).
RIGHT PANEL (about 340px wide): a placeholder card that reads
"对话 / 顾问面板 · Phase 3" with a hint below saying to collapse for more graph space.
```

This text is appended to each style prompt so all 8 mockups render the same content with only the visual treatment varying.

---

## File Structure

Files created (all under `/home/hhhh/Graph-Centric/mockups/`):

| Path | Purpose |
|---|---|
| `mockups/01_minimal-light.png` | Style 1, light mode |
| `mockups/02_minimal-dark.png` | Style 1, dark mode |
| `mockups/03_glassmorphism-light.png` | Style 2, light mode |
| `mockups/04_glassmorphism-dark.png` | Style 2, dark mode |
| `mockups/05_notion-light.png` | Style 3, light mode |
| `mockups/06_notion-dark.png` | Style 3, dark mode |
| `mockups/07_bento-light.png` | Style 4, light mode |
| `mockups/08_bento-dark.png` | Style 4, dark mode |
| `mockups/index.html` | Static gallery page (zero-dep HTML + inline CSS) |
| `mockups/serve-mockups.sh` | Bash script: starts `python3 -m http.server` with port fallback |

Files modified: none. Files deleted: none.

---

### Task 1: Pre-flight (auth + dir + port)

**Files:**
- Create: `/home/hhhh/Graph-Centric/mockups/` (empty directory)
- Verify-only: existing `mmx` install, existing Python 3, network binding sanity

**Prerequisites:** none (this is task 1).

- [ ] **Step 1: Verify `mmx` is authenticated**

```bash
cd /home/hhhh/Graph-Centric
mmx quota show --output json --quiet --non-interactive
```

Expected: JSON object printed, ending with `"base_resp": {"status_code": 0, ...}`. If you see an auth error or empty output, abort with message:
> "`mmx` is not authenticated. Run `mmx auth login --api-key sk-xxxx` and retry."

- [ ] **Step 2: Verify Python 3**

```bash
python3 --version
```

Expected: `Python 3.x.y` where x ≥ 8 (any modern 3.8+ works for `http.server`).

- [ ] **Step 3: Create the `mockups/` directory**

```bash
mkdir -p /home/hhhh/Graph-Centric/mockups
ls -la /home/hhhh/Graph-Centric/mockups
```

Expected: directory exists and is empty.

- [ ] **Step 4: Confirm port 8090 is free**

```bash
ss -tln 2>/dev/null | grep -E ':809[0-9] ' || echo "ports 8090-8099 all free"
```

Expected: either "ports 8090-8099 all free" or an empty line for the grep (no listeners).

- [ ] **Step 5: Commit**

```bash
cd /home/hhhh/Graph-Centric
git add mockups/.gitkeep
touch mockups/.gitkeep
git add mockups/.gitkeep
git commit -m "chore: scaffold mockups/ directory for style gallery"
git push origin main
```

(Use `.gitkeep` so the empty directory is tracked before any files exist.)

---

### Task 2: Generate Minimal-style mockups (light + dark)

**Files:**
- Create: `mockups/01_minimal-light.png`
- Create: `mockups/02_minimal-dark.png`

**Prerequisites:** Task 1 complete.

The COMMON_SCENE block (copy verbatim):

```
A 1440x900 web-app dashboard mockup for a tool called "Graph-Centric Agent".
The layout has four parts:
LEFT SIDEBAR (about 220px wide, full height): a header reading "Graph-Centric Agent",
a search input, then a list of 4 run items:
  - "Refactor auth flow" with a purple running dot
  - "Add CSS grid layout" with a green done dot
  - "Build skill compiler" with an amber paused dot
  - "Investigate skill match" with a red error dot
TOP BAR: a graph title "refactor/auth", a small "MiniMax-M2.7" model chip, a token counter
"3.2k / 50k", and two action buttons on the right.
CENTER: a force-directed-style graph visualization with 7 nodes connected by edges.
One node ("node_3") is highlighted/selected. Three of the nodes are purple (running),
two are green (done), one is red (error), and one is amber (paused).
RIGHT PANEL (about 340px wide): a placeholder card that reads
"对话 / 顾问面板 · Phase 3" with a hint below saying to collapse for more graph space.
```

- [ ] **Step 1: Generate `01_minimal-light.png`**

```bash
cd /home/hhhh/Graph-Centric
mmx image generate \
  --prompt "Linear/Vercel-style minimal web app mockup. Generous whitespace, hairline 1px borders, restrained neutral palette with a single purple accent (#7c3aed). Subtle drop shadows. Light background (#fafafa). Crisp Inter typography. The exact scene below.

$COMMON_SCENE" \
  --n 1 \
  --out /home/hhhh/Graph-Centric/mockups/01_minimal-light.png \
  --quiet \
  --non-interactive
```

(Substitute the literal COMMON_SCENE block where `$COMMON_SCENE` appears above; do not include the `$COMMON_SCENE` placeholder token in the actual command.)

- [ ] **Step 2: Verify `01_minimal-light.png` exists and is a real PNG**

```bash
ls -la /home/hhhh/Graph-Centric/mockups/01_minimal-light.png
file /home/hhhh/Graph-Centric/mockups/01_minimal-light.png
```

Expected: file exists, size > 10KB, `file` says `PNG image data` (target dimensions ~1440×900; image model may produce slightly different exact pixel size and that's acceptable — anything ≥1280×720 is fine).

- [ ] **Step 3: Generate `02_minimal-dark.png`**

```bash
cd /home/hhhh/Graph-Centric
mmx image generate \
  --prompt "Linear/Vercel-style minimal web app mockup, dark mode. Near-black background (#0f1117), restrained neutral text, single violet accent (#a78bfa). Subtle shadows on cards. Hairline 1px borders. Inter typography. The exact scene below.

$COMMON_SCENE" \
  --n 1 \
  --out /home/hhhh/Graph-Centric/mockups/02_minimal-dark.png \
  --quiet \
  --non-interactive
```

- [ ] **Step 4: Verify `02_minimal-dark.png`**

```bash
ls -la /home/hhhh/Graph-Centric/mockups/02_minimal-dark.png
file /home/hhhh/Graph-Centric/mockups/02_minimal-dark.png
```

Expected: file exists, size > 10KB, `file` says `PNG image data`. If `mmx` exited non-zero (e.g. quota exceeded, content filter), the file will not exist; investigate the response, retry, or skip and document the skip in the user-facing report — do not silently swallow.

Apply the same single-image-failure policy to all subsequent `mmx image generate` calls: a non-zero exit or missing file is a STOP on that single image only; continue with the rest. The Plan Self-Review checklist gates on "all 8 PNGs ≥10KB" — if any one failed, the gate fails.

- [ ] **Step 5: Commit**

```bash
cd /home/hhhh/Graph-Centric
git add mockups/01_minimal-light.png mockups/02_minimal-dark.png
git commit -m "feat(mockups): add Minimal style (light + dark) mockups"
git push origin main
```

---

### Task 3: Generate Glassmorphism-style mockups (light + dark)

**Files:**
- Create: `mockups/03_glassmorphism-light.png`
- Create: `mockups/04_glassmorphism-dark.png`

**Prerequisites:** Task 2 complete.

- [ ] **Step 1: Generate `03_glassmorphism-light.png`**

```bash
cd /home/hhhh/Graph-Centric
mmx image generate \
  --prompt "Frosted-glass web app mockup. Blurred translucent cards floating over a soft pastel gradient (light pinks/lavenders/pale cyan). Subtle neon purple edge glow on the active node. Modern UI. Light mode. The exact scene below.

$COMMON_SCENE" \
  --n 1 \
  --out /home/hhhh/Graph-Centric/mockups/03_glassmorphism-light.png \
  --quiet \
  --non-interactive
```

- [ ] **Step 2: Verify `03_glassmorphism-light.png`**

```bash
ls -la /home/hhhh/Graph-Centric/mockups/03_glassmorphism-light.png
file /home/hhhh/Graph-Centric/mockups/03_glassmorphism-light.png
```

Expected: file exists, size > 10KB, PNG.

- [ ] **Step 3: Generate `04_glassmorphism-dark.png`**

```bash
cd /home/hhhh/Graph-Centric
mmx image generate \
  --prompt "Frosted-glass web app mockup, dark mode. Dark backdrop with neon purple and cyan accents glowing on the active graph node and selected runs. Blurred translucent cards. Replit / Cursor adjacent. The exact scene below.

$COMMON_SCENE" \
  --n 1 \
  --out /home/hhhh/Graph-Centric/mockups/04_glassmorphism-dark.png \
  --quiet \
  --non-interactive
```

- [ ] **Step 4: Verify `04_glassmorphism-dark.png`**

```bash
ls -la /home/hhhh/Graph-Centric/mockups/04_glassmorphism-dark.png
file /home/hhhh/Graph-Centric/mockups/04_glassmorphism-dark.png
```

Expected: file exists, size > 10KB, PNG.

- [ ] **Step 5: Commit**

```bash
cd /home/hhhh/Graph-Centric
git add mockups/03_glassmorphism-light.png mockups/04_glassmorphism-dark.png
git commit -m "feat(mockups): add Glassmorphism style (light + dark) mockups"
git push origin main
```

---

### Task 4: Generate Notion-style mockups (light + dark)

**Files:**
- Create: `mockups/05_notion-light.png`
- Create: `mockups/06_notion-dark.png`

**Prerequisites:** Task 3 complete.

- [ ] **Step 1: Generate `05_notion-light.png`**

```bash
cd /home/hhhh/Graph-Centric
mmx image generate \
  --prompt "Warm, friendly web app mockup. Cream / paper-toned background (#f7f3ec), soft drop shadows, rounded corners (12px), muted accent palette. Notion / Arc / Loom aesthetic. Light mode. The exact scene below.

$COMMON_SCENE" \
  --n 1 \
  --out /home/hhhh/Graph-Centric/mockups/05_notion-light.png \
  --quiet \
  --non-interactive
```

- [ ] **Step 2: Verify `05_notion-light.png`**

```bash
ls -la /home/hhhh/Graph-Centric/mockups/05_notion-light.png
file /home/hhhh/Graph-Centric/mockups/05_notion-light.png
```

Expected: file exists, size > 10KB, PNG.

- [ ] **Step 3: Generate `06_notion-dark.png`**

```bash
cd /home/hhhh/Graph-Centric
mmx image generate \
  --prompt "Warm dark web app mockup. Dark brown background (#2a2520), soft shadows, rounded corners, paper-like muted card surfaces, low-contrast muted accent colors. Notion at night. The exact scene below.

$COMMON_SCENE" \
  --n 1 \
  --out /home/hhhh/Graph-Centric/mockups/06_notion-dark.png \
  --quiet \
  --non-interactive
```

- [ ] **Step 4: Verify `06_notion-dark.png`**

```bash
ls -la /home/hhhh/Graph-Centric/mockups/06_notion-dark.png
file /home/hhhh/Graph-Centric/mockups/06_notion-dark.png
```

Expected: file exists, size > 10KB, PNG.

- [ ] **Step 5: Commit**

```bash
cd /home/hhhh/Graph-Centric
git add mockups/05_notion-light.png mockups/06_notion-dark.png
git commit -m "feat(mockups): add Notion style (light + dark) mockups"
git push origin main
```

---

### Task 5: Generate Bento-style mockups (light + dark)

**Files:**
- Create: `mockups/07_bento-light.png`
- Create: `mockups/08_bento-dark.png`

**Prerequisites:** Task 4 complete.

- [ ] **Step 1: Generate `07_bento-light.png`**

```bash
cd /home/hhhh/Graph-Centric
mmx image generate \
  --prompt "Info-dense bento-grid web app mockup. Mixed card sizes in a grid, JetBrains Mono accents on status labels, code-adjacent IDE aesthetic. Light background. Raycast / GitHub Primer / VS Code adjacent. The exact scene below.

$COMMON_SCENE" \
  --n 1 \
  --out /home/hhhh/Graph-Centric/mockups/07_bento-light.png \
  --quiet \
  --non-interactive
```

- [ ] **Step 2: Verify `07_bento-light.png`**

```bash
ls -la /home/hhhh/Graph-Centric/mockups/07_bento-light.png
file /home/hhhh/Graph-Centric/mockups/07_bento-light.png
```

Expected: file exists, size > 10KB, PNG.

- [ ] **Step 3: Generate `08_bento-dark.png`**

```bash
cd /home/hhhh/Graph-Centric
mmx image generate \
  --prompt "Info-dense bento-grid web app mockup, dark mode. Mixed card sizes, JetBrains Mono accents on status labels, code-adjacent IDE aesthetic. Near-black background. Same design language as the light variant, inverted. The exact scene below.

$COMMON_SCENE" \
  --n 1 \
  --out /home/hhhh/Graph-Centric/mockups/08_bento-dark.png \
  --quiet \
  --non-interactive
```

- [ ] **Step 4: Verify `08_bento-dark.png`**

```bash
ls -la /home/hhhh/Graph-Centric/mockups/08_bento-dark.png
file /home/hhhh/Graph-Centric/mockups/08_bento-dark.png
```

Expected: file exists, size > 10KB, PNG.

- [ ] **Step 5: Commit**

```bash
cd /home/hhhh/Graph-Centric
git add mockups/07_bento-light.png mockups/08_bento-dark.png
git commit -m "feat(mockups): add Bento style (light + dark) mockups"
git push origin main
```

---

### Task 6: Write gallery `index.html`

**Files:**
- Create: `mockups/index.html`

**Prerequisites:** Tasks 2-5 complete (all 8 PNGs exist).

- [ ] **Step 1: Write `mockups/index.html`**

Create the file with this exact content (zero dependencies; pure HTML + inline `<style>`):

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <title>Graph-Centric — Style Mockup Gallery</title>
  <style>
    * { box-sizing: border-box; margin: 0; padding: 0; }
    body {
      font-family: -apple-system, "Segoe UI", system-ui, sans-serif;
      background: #0f1117;
      color: #e5e7eb;
      padding: 32px 24px 64px;
      line-height: 1.5;
    }
    header {
      max-width: 1200px;
      margin: 0 auto 32px;
      padding-bottom: 16px;
      border-bottom: 1px solid #232838;
    }
    header h1 { font-size: 1.6rem; margin-bottom: 6px; }
    header p { color: #8b8fa3; font-size: 0.9rem; }
    .grid {
      max-width: 1200px;
      margin: 0 auto;
      display: grid;
      grid-template-columns: repeat(2, 1fr);
      gap: 24px;
    }
    @media (max-width: 900px) {
      .grid { grid-template-columns: 1fr; }
    }
    .card {
      background: #151823;
      border: 1px solid #232838;
      border-radius: 12px;
      overflow: hidden;
      transition: transform 0.2s, border-color 0.2s;
    }
    .card:hover { transform: translateY(-2px); border-color: #a78bfa; }
    .card a.image-link {
      display: block;
      aspect-ratio: 1440 / 900;
      overflow: hidden;
      background: #0a0c14;
    }
    .card img {
      display: block;
      width: 100%;
      height: 100%;
      object-fit: cover;
      object-position: top center;
    }
    .card-meta {
      padding: 14px 16px;
      display: flex;
      justify-content: space-between;
      align-items: center;
      gap: 12px;
    }
    .card-title { font-size: 1rem; font-weight: 600; }
    .chip {
      font-size: 0.7rem;
      padding: 3px 10px;
      border-radius: 12px;
      font-weight: 500;
      letter-spacing: 0.02em;
    }
    .chip.light { background: #f5f3ff; color: #6d28d9; }
    .chip.dark  { background: #1e1b3a; color: #a78bfa; }
    .card-caption {
      padding: 0 16px 16px;
      color: #8b8fa3;
      font-size: 0.85rem;
    }
    footer {
      max-width: 1200px;
      margin: 48px auto 0;
      padding-top: 16px;
      border-top: 1px solid #232838;
      color: #8b8fa3;
      font-size: 0.8rem;
    }
  </style>
</head>
<body>
  <header>
    <h1>Graph-Centric — Style Mockup Gallery</h1>
    <p>Four visual directions × light / dark. Click any mockup to view full size. Tell me which one (or which few) to keep.</p>
  </header>

  <section class="grid">
    <article class="card" id="m01">
      <a class="image-link" href="01_minimal-light.png" target="_blank">
        <img src="01_minimal-light.png" alt="Minimal style, light mode" loading="lazy" />
      </a>
      <div class="card-meta">
        <span class="card-title">01 · Minimal · light</span>
        <span class="chip light">light</span>
      </div>
      <p class="card-caption">Pure whitespace, single violet accent, hairline borders — Linear / Vercel inspired.</p>
    </article>

    <article class="card" id="m02">
      <a class="image-link" href="02_minimal-dark.png" target="_blank">
        <img src="02_minimal-dark.png" alt="Minimal style, dark mode" loading="lazy" />
      </a>
      <div class="card-meta">
        <span class="card-title">02 · Minimal · dark</span>
        <span class="chip dark">dark</span>
      </div>
      <p class="card-caption">Same as 01 inverted to deep neutral with violet glow — for night-shift use.</p>
    </article>

    <article class="card" id="m03">
      <a class="image-link" href="03_glassmorphism-light.png" target="_blank">
        <img src="03_glassmorphism-light.png" alt="Glassmorphism style, light mode" loading="lazy" />
      </a>
      <div class="card-meta">
        <span class="card-title">03 · Glassmorphism · light</span>
        <span class="chip light">light</span>
      </div>
      <p class="card-caption">Frosted glass over pastel gradient — soft, modern, low contrast.</p>
    </article>

    <article class="card" id="m04">
      <a class="image-link" href="04_glassmorphism-dark.png" target="_blank">
        <img src="04_glassmorphism-dark.png" alt="Glassmorphism style, dark mode" loading="lazy" />
      </a>
      <div class="card-meta">
        <span class="card-title">04 · Glassmorphism · dark</span>
        <span class="chip dark">dark</span>
      </div>
      <p class="card-caption">Dark frosted glass with neon purple/cyan glow — Replit / Cursor adjacent.</p>
    </article>

    <article class="card" id="m05">
      <a class="image-link" href="05_notion-light.png" target="_blank">
        <img src="05_notion-light.png" alt="Notion style, light mode" loading="lazy" />
      </a>
      <div class="card-meta">
        <span class="card-title">05 · Notion · light</span>
        <span class="chip light">light</span>
      </div>
      <p class="card-caption">Warm cream paper-toned surfaces, soft shadows — Notion / Arc inspired.</p>
    </article>

    <article class="card" id="m06">
      <a class="image-link" href="06_notion-dark.png" target="_blank">
        <img src="06_notion-dark.png" alt="Notion style, dark mode" loading="lazy" />
      </a>
      <div class="card-meta">
        <span class="card-title">06 · Notion · dark</span>
        <span class="chip dark">dark</span>
      </div>
      <p class="card-caption">Warm dark brown surfaces, muted accents — Notion at night.</p>
    </article>

    <article class="card" id="m07">
      <a class="image-link" href="07_bento-light.png" target="_blank">
        <img src="07_bento-light.png" alt="Bento style, light mode" loading="lazy" />
      </a>
      <div class="card-meta">
        <span class="card-title">07 · Bento · light</span>
        <span class="chip light">light</span>
      </div>
      <p class="card-caption">Bento-grid card mosaic, mono accents — info-dense IDE-flavored.</p>
    </article>

    <article class="card" id="m08">
      <a class="image-link" href="08_bento-dark.png" target="_blank">
        <img src="08_bento-dark.png" alt="Bento style, dark mode" loading="lazy" />
      </a>
      <div class="card-meta">
        <span class="card-title">08 · Bento · dark</span>
        <span class="chip dark">dark</span>
      </div>
      <p class="card-caption">Same as 07 inverted for late-night dev work.</p>
    </article>
  </section>

  <footer>
    8 mockups × 1440×900 PNG · generated via <code>mmx image generate</code> ·
    served by Python <code>http.server</code> bound to <code>0.0.0.0</code>.
  </footer>
</body>
</html>
```

- [ ] **Step 2: Verify file exists and contains all 8 image refs**

```bash
grep -c '<img src="' /home/hhhh/Graph-Centric/mockups/index.html
ls -la /home/hhhh/Graph-Centric/mockups/index.html
```

Expected: grep returns `8` (one match per `<img src="...">` line); file size > 5KB.

- [ ] **Step 3: Commit**

```bash
cd /home/hhhh/Graph-Centric
git add mockups/index.html
git commit -m "feat(mockups): add static gallery HTML with 8 image cards"
git push origin main
```

---

### Task 7: Write `serve-mockups.sh` + start the server

**Files:**
- Create: `mockups/serve-mockups.sh` (executable)

**Prerequisites:** Task 6 complete.

- [ ] **Step 1: Write `serve-mockups.sh`**

```bash
cat > /home/hhhh/Graph-Centric/mockups/serve-mockups.sh <<'EOF'
#!/usr/bin/env bash
# Serves the ./mockups folder on a port (default 8090), bound to 0.0.0.0
# so any LAN device can reach it. Falls back through 8090..8099.
# Prints the chosen LAN URL.

set -euo pipefail

cd "$(dirname "$0")"

PORT=8090
while [ $PORT -lt 8100 ]; do
  if ! ss -tln 2>/dev/null | grep -q ":$PORT "; then
    break
  fi
  PORT=$((PORT + 1))
done

if [ $PORT -eq 8100 ]; then
  echo "ERROR: no free port in 8090..8099" >&2
  exit 1
fi

# Best-effort LAN IP discovery.
LAN_IP=$(hostname -I 2>/dev/null | awk '{print $1}')
if [ -z "$LAN_IP" ]; then
  LAN_IP="127.0.0.1"
fi

echo "mockups dir : $(pwd)"
echo "LAN URL     : http://${LAN_IP}:${PORT}/"
echo "Local URL   : http://localhost:${PORT}/"
echo
echo "Serving... press Ctrl-C to stop."

exec python3 -m http.server "$PORT" --bind 0.0.0.0
EOF
chmod +x /home/hhhh/Graph-Centric/mockups/serve-mockups.sh
```

- [ ] **Step 2: Verify the script exists and is executable**

```bash
ls -la /home/hhhh/Graph-Centric/mockups/serve-mockups.sh
test -x /home/hhhh/Graph-Centric/mockups/serve-mockups.sh && echo "executable OK"
```

Expected: file present, mode `-rwxr-xr-x`, "executable OK" printed.

- [ ] **Step 3: Commit the script**

```bash
cd /home/hhhh/Graph-Centric
git add mockups/serve-mockups.sh
git commit -m "feat(mockups): add serve-mockups.sh with port-fallback LAN binder"
git push origin main
```

- [ ] **Step 4: Start the server in the background**

```bash
cd /home/hhhh/Graph-Centric/mockups
nohup ./serve-mockups.sh > /tmp/mockups-server.log 2>&1 &
SERVER_PID=$!
echo "server pid: $SERVER_PID"
sleep 1
echo "--- log so far ---"
cat /tmp/mockups-server.log
```

Expected: log shows `LAN URL : http://<ip>:<port>/` and `Serving... press Ctrl-C to stop.` (PID is non-zero).

- [ ] **Step 5: Verify the server reachable via curl (local check)**

```bash
curl -fsS -o /tmp/index-snippet.html http://localhost:8090/
echo "--- 8 image refs found ---"
grep -oE '[0-9]{2}_[a-z]+-(light|dark)\.png' /tmp/index-snippet.html | sort -u | wc -l
```

Expected: number is `8`. If you got fewer, see "Troubleshooting" below.

- [ ] **Step 6: Verify a single image is reachable**

```bash
curl -fsSI http://localhost:8090/01_minimal-light.png
```

Expected: `HTTP/1.0 200 OK` and `Content-Type: image/png`.

- [ ] **Step 7: Report URL to the user**

Compose a chat message that includes:

- The LAN URL (from `/tmp/mockups-server.log`).
- The PID, in case the user wants to stop it: `kill <pid>` (from step 4).
- A reminder to give back feedback like *"use Minimal and Glassmorphism, ditch Notion and Bento"* when they're done.

Troubleshooting:

- If step 5 returns fewer than 8 image refs, the server is serving from a wrong directory. Stop it (`kill <pid>`), `cd` into `mockups/`, and rerun.
- If `ss` is not present, replace with `netstat -tln 2>/dev/null | grep -q ":$PORT "`.
- The server keeps running until killed. To stop it later: `lsof -ti:8090 | xargs -r kill`.

---

## Self-Review Checklist (run before reporting done)

- [ ] All 8 PNGs exist, each ≥10KB, each `file` reports `PNG image data, 1440 x 900`.
- [ ] `mockups/index.html` references all 8 PNGs (`grep -c '<img src="' == 8`).
- [ ] `mockups/serve-mockups.sh` is executable and starts the server on the first free port in 8090..8099.
- [ ] HTTP server is running in the background, bound to `0.0.0.0`, with a known PID.
- [ ] `curl http://localhost:<port>/` returns the HTML with all 8 image refs.
- [ ] `curl -I http://localhost:<port>/<image>.png` returns 200 + `image/png`.
- [ ] Every commit has been pushed to `origin/main`.
- [ ] The user has been told the LAN URL and reminded how to give feedback.

## Out-of-Scope Reminder

This plan covers only Phase 1 (mockup gallery). Phase 2 — implementing the chosen styles as a runtime theme switcher in the actual `webui/` — gets a separate spec once the user has chosen.
