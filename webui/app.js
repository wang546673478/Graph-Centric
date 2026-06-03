// Graph-Centric Web UI — vanilla JS SPA.
// Routes: / (run), /runs, /skills, /files, /settings.

const $ = (sel, root = document) => root.querySelector(sel);
const root = $('#root');

let activeRun = {
  runId: null,
  es: null,
  graph: null,
  transcript: [],
  status: 'idle',
  nodes: [],
  edges: [],
  errorMsg: null,
  durationTimer: null,
  durationSec: 0,
  activeTab: 'graph',
  changedFiles: [],
  selectedFile: null,
  fileDiffText: '',
};

// ---------- API ----------

const api = {
  health: () => fetch('/api/health').then(r => r.json()),
  listRuns: () => fetch('/api/runs').then(r => r.json()),
  createRun: (task) => fetch('/api/runs', {
    method: 'POST', headers: {'content-type': 'application/json'},
    body: JSON.stringify({ task }),
  }).then(r => r.json()),
  getRun: (id) => fetch(`/api/runs/${id}`).then(r => r.json()),
  cancelRun: (id) => fetch(`/api/runs/${id}`, { method: 'DELETE' }).then(r => r.json()),
  listSkills: () => fetch('/api/skills').then(r => r.json()),
  getSkill: (slug) => fetch(`/api/skills/${slug}`).then(r => r.json()),
  deleteSkill: (slug) => fetch(`/api/skills/${slug}`, { method: 'DELETE' }).then(r => r.json()),
  promoteSkill: (slug) => fetch(`/api/skills/${slug}/promote`, { method: 'POST' }).then(r => r.json()),
  filesChanged: (since) => {
    const q = since ? `?since=${encodeURIComponent(since)}` : '';
    return fetch(`/api/files/changed${q}`).then(r => r.json());
  },
  fileDiff: (path) => fetch(`/api/files/diff?path=${encodeURIComponent(path)}`).then(r => r.json()),
};

// ---------- Router ----------

const routes = {
  '/': renderRun,
  '/runs': renderRuns,
  '/skills': renderSkills,
  '/files': renderFiles,
  '/settings': renderSettings,
};

function mount() {
  window.addEventListener('hashchange', dispatch);
  dispatch();
  highlightNav();
}

function dispatch() {
  const path = (location.hash.replace(/^#/, '') || '/');
  const handler = routes[path] || renderNotFound;
  closeEventSource();
  handler();
  highlightNav();
}

function highlightNav() {
  const path = (location.hash.replace(/^#/, '') || '/');
  document.querySelectorAll('.topnav a').forEach(a => {
    a.classList.toggle('active', a.dataset.route === path);
  });
}

function closeEventSource() {
  if (activeRun.es) { activeRun.es.close(); activeRun.es = null; }
  if (activeRun.durationTimer) {
    clearInterval(activeRun.durationTimer);
    activeRun.durationTimer = null;
  }
}

// ---------- Run view ----------

function renderRun() {
  root.innerHTML = `
    <div class="layout-run">
      <section class="panel">
        <h2>Chat</h2>
        <div id="transcript" class="transcript"></div>
        <div class="composer">
          <input id="task-input" placeholder="Type a task…" />
          <button id="run-btn">Run</button>
        </div>
        <div id="run-meta" class="run-meta"></div>
      </section>
      <section class="panel">
        <div class="tabs">
          <button data-tab="graph" class="active">Graph</button>
          <button data-tab="files">Files</button>
          <button data-tab="diff">Diff</button>
        </div>
        <div id="tab-content" style="flex:1; display:flex; flex-direction:column; min-height:0;"></div>
      </section>
    </div>
  `;

  // Tab switching
  $('.tabs').addEventListener('click', e => {
    const tab = e.target.dataset.tab;
    if (!tab) return;
    activeRun.activeTab = tab;
    $('.tabs button').forEach(b => b.classList.toggle('active', b.dataset.tab === tab));
    renderTabContent();
  });

  // Composer
  $('#task-input').addEventListener('keydown', e => {
    if (e.key === 'Enter') submitTask();
  });
  $('#run-btn').addEventListener('click', () => {
    if (activeRun.status === 'Running' || activeRun.status === 'Paused') {
      stopRun();
    } else {
      submitTask();
    }
  });

  // Initial state
  activeRun.transcript = [];
  activeRun.nodes = [];
  activeRun.edges = [];
  activeRun.status = 'idle';
  activeRun.errorMsg = null;
  activeRun.durationSec = 0;
  renderRunMeta();
  renderTabContent();
  // restore any prior run from a previous view? v1: no.
}

async function submitTask() {
  const input = $('#task-input');
  const task = input.value.trim();
  if (!task) return;
  input.value = '';
  activeRun.transcript = [];
  activeRun.nodes = [];
  activeRun.edges = [];
  activeRun.status = 'Running';
  activeRun.errorMsg = null;
  activeRun.durationSec = 0;
  activeRun.durationTimer = setInterval(() => {
    activeRun.durationSec++;
    renderRunMeta();
  }, 1000);
  renderRunMeta();
  renderTranscript();

  try {
    const { id } = await api.createRun(task);
    activeRun.runId = id;
    attachSse(id);
  } catch (e) {
    activeRun.errorMsg = String(e);
    activeRun.status = 'Error';
    renderRunMeta();
    clearInterval(activeRun.durationTimer);
  }
}

async function stopRun() {
  if (!activeRun.runId) return;
  try { await api.cancelRun(activeRun.runId); } catch (e) { /* ignore */ }
  activeRun.status = 'Cancelled';
  if (activeRun.es) { activeRun.es.close(); activeRun.es = null; }
  if (activeRun.durationTimer) {
    clearInterval(activeRun.durationTimer);
    activeRun.durationTimer = null;
  }
  renderRunMeta();
}

function attachSse(runId) {
  const es = new EventSource(`/api/runs/${runId}/events`);
  activeRun.es = es;
  const handlers = {
    transcript: data => {
      activeRun.transcript.push(data);
      renderTranscript();
    },
    graph: data => {
      activeRun.nodes = data.nodes || [];
      activeRun.edges = data.edges || [];
      renderGraph();
    },
    loop_state: data => {
      activeRun.status = data.kind;
      renderRunMeta();
    },
    done: data => {
      activeRun.status = 'Done';
      clearInterval(activeRun.durationTimer);
      renderRunMeta();
    },
    error: data => {
      activeRun.status = 'Error';
      activeRun.errorMsg = data.message;
      clearInterval(activeRun.durationTimer);
      renderRunMeta();
    },
  };
  ['transcript', 'graph', 'loop_state', 'review', 'skill_captured', 'done', 'error']
    .forEach(type => {
      es.addEventListener(type, e => {
        try { handlers[type]?.(JSON.parse(e.data)); } catch (err) { /* ignore */ }
      });
    });
  es.onerror = () => {
    es.close();
    if (activeRun.es === es) activeRun.es = null;
  };
}

function renderTranscript() {
  const el = $('#transcript');
  if (!el) return;
  el.innerHTML = activeRun.transcript.map(msg => `
    <div class="msg ${escapeAttr(msg.role)}">
      <div class="role">${escapeHtml(msg.role)}</div>
      <div>${escapeHtml(msg.content)}</div>
    </div>
  `).join('') + (activeRun.errorMsg ? `<div class="msg error"><div class="role">error</div>${escapeHtml(activeRun.errorMsg)}</div>` : '');
  el.scrollTop = el.scrollHeight;
}

function renderRunMeta() {
  const el = $('#run-meta');
  if (!el) return;
  const id = activeRun.runId ? activeRun.runId.slice(0, 8) : '—';
  el.innerHTML = activeRun.runId
    ? `Run ${id}… · ${activeRun.durationSec}s · <span class="status-pill ${escapeAttr(activeRun.status)}">${escapeHtml(activeRun.status)}</span>`
    : '<span class="muted">No active run</span>';
}

function renderTabContent() {
  const el = $('#tab-content');
  if (!el) return;
  if (activeRun.activeTab === 'graph') {
    el.innerHTML = `<div id="cy" class="graph-canvas"></div>`;
    if (window.cytoscape) initCytoscape();
  } else if (activeRun.activeTab === 'files') {
    el.innerHTML = `<div class="file-list" id="files"></div>`;
    loadFiles();
  } else {
    el.innerHTML = `<div class="diff-text" id="diff"></div>`;
    renderDiff();
  }
}

function initCytoscape() {
  if (!window.cytoscape) return;
  const cy = cytoscape({
    container: $('#cy'),
    elements: [
      ...activeRun.nodes.map(n => ({ data: { id: n.id, label: n.summary || n.id } })),
      ...activeRun.edges.map((e, i) => ({ data: { id: `e${i}`, source: e.source, target: e.target, label: e.relation } })),
    ],
    style: [
      { selector: 'node', style: { 'background-color': '#3b82f6', 'label': 'data(label)', 'color': '#f1f5f9', 'text-wrap': 'wrap', 'text-max-width': '120px', 'font-size': '9px' } },
      { selector: 'edge', style: { 'width': 1, 'line-color': '#64748b', 'target-arrow-color': '#64748b', 'target-arrow-shape': 'triangle', 'curve-style': 'bezier', 'label': 'data(label)', 'font-size': '7px', 'color': '#94a3b8' } },
    ],
    layout: { name: 'cose', animate: false, idealEdgeLength: 80, nodeRepulsion: 4000 },
  });
  activeRun.graph = cy;
}

function renderGraph() {
  if (activeRun.graph) {
    activeRun.graph.elements().remove();
    activeRun.graph.add([
      ...activeRun.nodes.map(n => ({ data: { id: n.id, label: n.summary || n.id } })),
      ...activeRun.edges.map((e, i) => ({ data: { id: `e${i}`, source: e.source, target: e.target, label: e.relation } })),
    ]);
    activeRun.graph.layout({ name: 'cose', animate: false }).run();
  } else if (activeRun.activeTab === 'graph') {
    initCytoscape();
  }
}

async function loadFiles() {
  try {
    const files = await api.filesChanged();
    activeRun.changedFiles = files;
    const el = $('#files');
    if (!el) return;
    if (!files.length) {
      el.innerHTML = '<div style="color:var(--muted)">No files changed.</div>';
      return;
    }
    el.innerHTML = files.map(f => `
      <div class="file-item" data-path="${escapeAttr(f.path)}">
        <span class="change-type">${escapeHtml(f.change_type)}</span>${escapeHtml(f.path)}
      </div>
    `).join('');
    el.querySelectorAll('.file-item').forEach(item => {
      item.addEventListener('click', () => selectFile(item.dataset.path));
    });
  } catch (e) {
    $('#files').innerHTML = `<div class="error-banner">${escapeHtml(String(e))}</div>`;
  }
}

async function selectFile(path) {
  activeRun.selectedFile = path;
  activeRun.activeTab = 'diff';
  $('.tabs button').forEach(b => b.classList.toggle('active', b.dataset.tab === 'diff'));
  renderTabContent();
  try {
    const result = await api.fileDiff(path);
    activeRun.fileDiffText = result.diff || '';
    renderDiff();
  } catch (e) {
    activeRun.fileDiffText = `error: ${e}`;
    renderDiff();
  }
}

function renderDiff() {
  const el = $('#diff');
  if (!el) return;
  if (!activeRun.selectedFile) {
    el.textContent = 'Select a file from the Files tab.';
    return;
  }
  const lines = (activeRun.fileDiffText || '').split('\n');
  el.innerHTML = lines.map(line => {
    const cls = line.startsWith('+') ? 'add' : line.startsWith('-') ? 'del' : line.startsWith('@@') ? 'hunk' : '';
    return `<div class="diff-line ${cls}">${escapeHtml(line)}</div>`;
  }).join('');
}

// ---------- Runs list ----------

async function renderRuns() {
  root.innerHTML = `<div class="list-page"><h1>Run History</h1><div id="runs-table">Loading…</div></div>`;
  try {
    const runs = await api.listRuns();
    if (!runs.length) {
      $('#runs-table').innerHTML = '<div style="color:var(--muted)">No runs yet.</div>';
      return;
    }
    $('#runs-table').innerHTML = `
      <table style="width:100%; border-collapse:collapse;">
        <thead><tr style="text-align:left; color:var(--muted); font-size:0.8rem;">
          <th>ID</th><th>Task</th><th>Status</th><th>Duration</th><th>Captured</th>
        </tr></thead>
        <tbody>
          ${runs.map(r => `
            <tr style="border-top:1px solid var(--border);">
              <td style="font-family:monospace; padding:0.4rem;">${r.id.slice(0, 8)}…</td>
              <td>${escapeHtml(r.task || '')}</td>
              <td><span class="status-pill ${escapeAttr(statusStr(r.status))}">${escapeHtml(statusStr(r.status))}</span></td>
              <td>${(r.duration_ms / 1000).toFixed(1)}s</td>
              <td>${r.captured_skill ? escapeHtml(r.captured_skill.slug) : '—'}</td>
            </tr>
          `).join('')}
        </tbody>
      </table>
    `;
  } catch (e) {
    $('#runs-table').innerHTML = `<div class="error-banner">${escapeHtml(String(e))}</div>`;
  }
}

function statusStr(s) {
  if (typeof s === 'string') return s;
  return Object.keys(s)[0] || '?';
}

// ---------- Skills list ----------

async function renderSkills() {
  root.innerHTML = `<div class="list-page"><h1>Skill Library</h1><div id="skills-list">Loading…</div></div>`;
  try {
    const skills = await api.listSkills();
    if (!skills.length) {
      $('#skills-list').innerHTML = '<div style="color:var(--muted)">No skills yet.</div>';
      return;
    }
    $('#skills-list').innerHTML = skills.map(s => `
      <div class="row" data-slug="${escapeAttr(s.slug)}">
        <div class="slug">${escapeHtml(s.slug)}</div>
        <div class="trigger">"${escapeHtml(s.trigger || '')}"</div>
      </div>
    `).join('');
    $('#skills-list').querySelectorAll('.row').forEach(row => {
      row.addEventListener('click', () => showSkillDetail(row.dataset.slug));
    });
  } catch (e) {
    $('#skills-list').innerHTML = `<div class="error-banner">${escapeHtml(String(e))}</div>`;
  }
}

async function showSkillDetail(slug) {
  let skill;
  try { skill = await api.getSkill(slug); } catch (e) { alert(`Failed to load skill: ${e}`); return; }
  const graphNodes = (skill.graph?.nodes || []).length;
  const graphEdges = (skill.graph?.edges || []).length;
  const ok = confirm(
    `Skill: ${skill.slug}\n` +
    `Trigger: ${skill.trigger}\n` +
    `Task: ${skill.task}\n` +
    `Graph: ${graphNodes} nodes / ${graphEdges} edges\n` +
    `Created: ${skill.meta?.created_at}\n` +
    `Model: ${skill.meta?.model_used}\n\n` +
    `OK → close\nCancel → delete skill`
  );
  if (!ok) {
    if (confirm(`Delete skill "${slug}"?`)) {
      try {
        await api.deleteSkill(slug);
        renderSkills();
      } catch (e) { alert(`Delete failed: ${e}`); }
    }
  }
}

// ---------- Files list (standalone) ----------

async function renderFiles() {
  root.innerHTML = `<div class="list-page"><h1>Changed Files</h1><div id="files-list">Loading…</div></div>`;
  try {
    const files = await api.filesChanged();
    if (!files.length) {
      $('#files-list').innerHTML = '<div style="color:var(--muted)">No changed files (or not in a git repo).</div>';
      return;
    }
    $('#files-list').innerHTML = files.map(f => `
      <div class="row" data-path="${escapeAttr(f.path)}">
        <span class="change-type" style="display:inline-block; width:5rem; color:var(--muted); font-size:0.7rem;">${escapeHtml(f.change_type)}</span>
        <span style="font-family:monospace; font-size:0.85rem;">${escapeHtml(f.path)}</span>
      </div>
    `).join('');
    $('#files-list').querySelectorAll('.row').forEach(row => {
      row.addEventListener('click', async () => {
        const path = row.dataset.path;
        try {
          const result = await api.fileDiff(path);
          const w = window.open('', '_blank');
          if (w) {
            w.document.write(`<title>${escapeHtml(path)}</title><pre style="font-family:monospace; white-space:pre-wrap; padding:1rem;">${escapeHtml(result.diff || '(no diff)')}</pre>`);
            w.document.close();
          }
        } catch (e) { alert(`Failed to get diff: ${e}`); }
      });
    });
  } catch (e) {
    $('#files-list').innerHTML = `<div class="error-banner">${escapeHtml(String(e))}</div>`;
  }
}

// ---------- Settings ----------

async function renderSettings() {
  root.innerHTML = `<div class="list-page"><h1>Settings</h1><div id="settings-body">Loading…</div></div>`;
  try {
    const h = await api.health();
    $('#settings-body').innerHTML = `
      <p>Backend: <span class="status-pill ${h.status === 'ok' ? 'Done' : 'Error'}">${escapeHtml(h.status)}</span></p>
      <p style="color:var(--muted); font-size:0.85rem;">
        Model config (<code>MODEL_BASE_URL</code>, <code>MODEL_API_KEY</code>, etc.) is read from
        <code>.env</code> at server startup. Edit <code>.env</code> and restart
        <code>bin/serve</code> to change.
      </p>
      <p style="color:var(--muted); font-size:0.85rem;">
        Bind address: <code>WEB_PORT</code> (default 8080). Static dir: <code>WEB_STATIC_DIR</code>.
      </p>
    `;
  } catch (e) {
    $('#settings-body').innerHTML = `<div class="error-banner">Backend unreachable: ${escapeHtml(String(e))}</div>`;
  }
}

// ---------- 404 ----------

function renderNotFound() {
  root.innerHTML = `<div class="list-page"><h1>Not found</h1><p><a href="#/">Go home</a></p></div>`;
}

// ---------- utils ----------

function escapeHtml(s) {
  if (s == null) return '';
  return String(s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}

function escapeAttr(s) {
  if (s == null) return '';
  return String(s).replace(/"/g, '&quot;');
}

// Expose for inline boot
window.mount = mount;
