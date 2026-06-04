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
  createRun: (task, initialGraph, initialTranscript) => {
    const body = { task };
    if (initialGraph && (initialGraph.nodes?.length || initialGraph.edges?.length)) {
      body.initial_graph = initialGraph;
    }
    if (initialTranscript && initialTranscript.length) {
      body.initial_transcript = initialTranscript;
    }
    return fetch('/api/runs', {
      method: 'POST', headers: {'content-type': 'application/json'},
      body: JSON.stringify(body),
    }).then(r => r.json());
  },
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
  window.addEventListener('keydown', onGlobalKeydown);
  dispatch();
  highlightNav();
}

function onGlobalKeydown(e) {
  // Cmd+K / Ctrl+K → focus the chat input (works on every page).
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
    e.preventDefault();
    const input = document.getElementById('task-input');
    if (input) {
      input.focus();
    } else {
      // Not on the run view — navigate to it first; the next render
      // will create the input and a follow-up call to focus it
      // would land in the run view. Skip the second hop for now.
      location.hash = '#/';
    }
  }
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
        <h2>Chat <button id="reset-btn" class="secondary" style="float:right; font-size:0.75rem; padding:0.2rem 0.5rem;">Reset</button></h2>
        <div id="transcript" class="transcript"></div>
        <div id="thinking" style="display:none; padding:0.5rem; color:var(--muted); font-size:0.85rem;">💭 thinking…</div>
        <div class="composer">
          <input id="task-input" placeholder="Type a message…" />
          <button id="run-btn">Send</button>
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
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      submitTask();
    }
  });
  $('#run-btn').addEventListener('click', () => {
    // Only the actively-Running case maps to "Stop". A Paused run is
    // waiting for the agent to continue, but the user is starting a
    // new conversation turn — Send should create a fresh run, not
    // cancel the old one. (Use the × next to the status pill to
    // actually cancel.)
    if (activeRun.status === 'Running') {
      stopRun();
    } else {
      submitTask();
    }
  });
  $('#reset-btn').addEventListener('click', () => {
    if (activeRun.status === 'Running' || activeRun.status === 'Paused') {
      if (!confirm('Reset will cancel the current run. Continue?')) return;
      stopRun();
    }
    activeRun.transcript = [];
    activeRun.nodes = [];
    activeRun.edges = [];
    activeRun.status = 'idle';
    activeRun.errorMsg = null;
    activeRun.runId = null;
    activeRun.durationSec = 0;
    renderTranscript();
    renderRunMeta();
    if (activeRun.graph && activeRun.activeTab === 'graph') renderGraph();
  });

  renderRunMeta();
  renderTabContent();
}

async function submitTask() {
  const input = $('#task-input');
  const task = input.value.trim();
  if (!task) return;
  input.value = '';
  input.disabled = true;
  document.getElementById('run-btn').textContent = 'Stop';
  document.getElementById('run-btn').classList.add('danger');

  // Echo the user message into the chat immediately so they see what
  // they sent, and so the chat reads as a real conversation even
  // before the agent responds.
  activeRun.transcript.push({ role: 'user', content: task });
  activeRun.status = 'Running';
  activeRun.errorMsg = null;
  if (!activeRun.durationSec) {
    activeRun.durationSec = 0;
  }
  activeRun.durationTimer = setInterval(() => {
    activeRun.durationSec++;
    renderRunMeta();
  }, 1000);
  document.getElementById('thinking').style.display = 'block';
  renderTranscript();
  renderRunMeta();

  // Build the seed graph from whatever the latest SSE events told us
  // — the agent will continue from this state instead of starting empty.
  // Also send the prior transcript so the agent's Conversation starts
  // with the chat history (matching what the user has seen).
  const initial_graph = (activeRun.nodes.length || activeRun.edges.length)
    ? { nodes: activeRun.nodes, edges: activeRun.edges }
    : null;
  const initial_transcript = activeRun.transcript.length
    ? activeRun.transcript.map(m => ({ role: m.role, content: m.content }))
    : null;

  try {
    const { id } = await api.createRun(task, initial_graph, initial_transcript);
    activeRun.runId = id;
    attachSse(id);
  } catch (e) {
    activeRun.errorMsg = String(e);
    activeRun.status = 'Error';
    document.getElementById('thinking').style.display = 'none';
    renderTranscript();
    renderRunMeta();
    clearInterval(activeRun.durationTimer);
    input.disabled = false;
    document.getElementById('run-btn').textContent = 'Send';
    document.getElementById('run-btn').classList.remove('danger');
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
  const input = document.getElementById('task-input');
  if (input) input.disabled = false;
  const btn = document.getElementById('run-btn');
  if (btn) { btn.textContent = 'Send'; btn.classList.remove('danger'); }
  document.getElementById('thinking').style.display = 'none';
  renderRunMeta();
}

function attachSse(runId) {
  const es = new EventSource(`/api/runs/${runId}/events`);
  activeRun.es = es;
  const finishRun = (newStatus) => {
    activeRun.status = newStatus;
    clearInterval(activeRun.durationTimer);
    const input = document.getElementById('task-input');
    if (input) {
      input.disabled = false;
      input.focus();
    }
    const btn = document.getElementById('run-btn');
    if (btn) {
      btn.textContent = 'Send';
      btn.classList.remove('danger');
    }
    document.getElementById('thinking').style.display = 'none';
    renderRunMeta();
  };
  const handlers = {
    transcript: data => {
      document.getElementById('thinking').style.display = 'none';
      activeRun.transcript.push(data);
      renderTranscript();
    },
    graph: data => {
      document.getElementById('thinking').style.display = 'none';
      activeRun.nodes = data.nodes || [];
      activeRun.edges = data.edges || [];
      renderGraph();
    },
    loop_state: data => {
      document.getElementById('thinking').style.display = 'none';
      activeRun.status = data.kind;
      // When the agent pauses to ask the user a question, re-enable
      // the input + reset the Send button so the next message is
      // routed to either a follow-up question or a fresh run.
      if (data.kind === 'Paused' || data.kind === 'GraphInvalid') {
        const input = document.getElementById('task-input');
        if (input) input.disabled = false;
        const btn = document.getElementById('run-btn');
        if (btn) {
          btn.textContent = 'Send';
          btn.classList.remove('danger');
        }
      }
      renderRunMeta();
    },
    skill_captured: data => {
      activeRun.transcript.push({
        role: 'skill_captured',
        content: `📚 skill captured: ${data.slug} — ${data.trigger}`,
      });
      renderTranscript();
    },
    done: data => {
      finishRun('Done');
    },
    error: data => {
      activeRun.errorMsg = data.message;
      finishRun('Error');
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
    if (activeRun.status === 'Running') finishRun('Cancelled');
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
  const nodeCount = activeRun.nodes.length;
  const edgeCount = activeRun.edges.length;
  el.innerHTML = activeRun.runId || activeRun.transcript.length
    ? `${activeRun.durationSec}s · graph: ${nodeCount} nodes / ${edgeCount} edges · <span class="status-pill ${escapeAttr(activeRun.status)}">${escapeHtml(activeRun.status)}</span>`
    : '<span class="muted">Send a message to start building the relationship graph.</span>';
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

// Auto-mount on script load.
mount();
