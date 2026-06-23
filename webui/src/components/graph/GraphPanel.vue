<script setup lang="ts">
import { ref, watch, onMounted, onUnmounted, computed } from 'vue'
import { useGraphColors } from '../../composables/useGraphColors'
import { theme } from '../../composables/useTheme'

const props = defineProps<{ nodes: any[]; edges: any[]; scopeNodeIds?: string[]; fx?: { added: string[]; removed: string[]; replaced: boolean; ts: number } }>()
const container = ref<HTMLElement>()
let cy: any = null
const breadcrumb = ref<string[]>([])  // stack of node IDs: ['project', 'auth.rs', 'login']
const selectedNode = ref<any>(null)

// Compute which nodes have Contains children.
const childMap = computed(() => {
  const m = new Map<string, string[]>()
  for (const e of props.edges) {
    if (e.relation === 'Contains') {
      if (!m.has(e.source)) m.set(e.source, [])
      m.get(e.source)!.push(e.target)
    }
  }
  return m
})

const parentMap = computed(() => {
  const m = new Map<string, string>()
  for (const e of props.edges) {
    if (e.relation === 'Contains') m.set(e.target, e.source)
  }
  return m
})

const hasChildren = (id: string) => childMap.value.has(id)

// A node is drillable only if it has children AND is not already the current
// breadcrumb tip. Without the second clause, clicking the parent node (which
// is itself rendered at its own level, see visibleIds) re-drills into itself
// forever: `deliverable › deliverable › deliverable › …`.
const canDrill = (id: string) =>
  hasChildren(id) && breadcrumb.value[breadcrumb.value.length - 1] !== id

// Filter nodes/edges to current breadcrumb level.
const visibleIds = computed(() => {
  if (breadcrumb.value.length === 0) {
    // Show only root-level nodes (no Contains parent)
    const childSet = new Set<string>()
    for (const ids of childMap.value.values()) {
      for (const id of ids) childSet.add(id)
    }
    return new Set(props.nodes.filter(n => !childSet.has(n.id)).map(n => n.id))
  }
  const parentId = breadcrumb.value[breadcrumb.value.length - 1]
  const children = childMap.value.get(parentId) || []
  return new Set([parentId, ...children])
})

const visibleNodes = computed(() =>
  props.nodes.filter(n => visibleIds.value.has(n.id))
)
const visibleEdges = computed(() =>
  props.edges.filter(e =>
    visibleIds.value.has(e.source) && visibleIds.value.has(e.target)
  )
)

const scopeSet = computed(() => new Set(props.scopeNodeIds || []))

function drillDown(nodeId: string) {
  if (canDrill(nodeId)) {
    breadcrumb.value.push(nodeId)
    cy?.center(cy.getElementById(nodeId))
  }
}

function goToLevel(idx: number) {
  breadcrumb.value = breadcrumb.value.slice(0, idx + 1)
}

function goRoot() {
  breadcrumb.value = []
  selectedNode.value = null
}

// ---- Cytoscape ----
const C = useGraphColors()

function cyStyle() {
  return [
    { selector: 'node', style: { 'background-color': C.node, 'label': 'data(label)', 'color': C.text, 'text-wrap': 'wrap', 'text-max-width': '100px', 'font-size': '9px', 'border-width': 1, 'border-color': C.node } },
    { selector: 'node.in-scope', style: { 'background-color': C.scope, 'border-color': C.scope, 'border-width': 3, 'text-outline-color': C.scope, 'text-outline-width': 1 } },
    { selector: 'node.complex', style: { 'border-width': 3, 'border-color': C.complex, 'border-style': 'double' } },
    { selector: 'node.selected', style: { 'border-color': C.text, 'border-width': 3, 'text-outline-color': C.text, 'text-outline-width': 1 } },
    { selector: 'edge', style: { 'width': 1.5, 'line-color': C.edge, 'target-arrow-color': C.edge, 'target-arrow-shape': 'triangle', 'curve-style': 'bezier', 'label': 'data(label)', 'font-size': '7px', 'color': C.text } },
    { selector: 'edge.in-scope', style: { 'line-color': C.edgeScope, 'target-arrow-color': C.edgeScope, 'width': 2 } },
    { selector: 'edge.Contains', style: { 'line-style': 'dashed', 'line-color': C.complex, 'target-arrow-color': C.complex } },
  ]
}

onMounted(() => {
  if (container.value && (window as any).cytoscape) {
    cy = (window as any).cytoscape({
      container: container.value,
      style: cyStyle(),
      layout: { name: 'cose', animate: true, idealEdgeLength: 100, nodeRepulsion: 6000 },
    })
    // Click: drill down or show detail
    cy.on('tap', 'node', (evt: any) => {
      const node = evt.target
      const id = node.id()
      if (canDrill(id)) {
        drillDown(id)
      } else {
        selectedNode.value = props.nodes.find(n => n.id === id) || null
      }
    })
    updateGraph()
  }
})

watch(theme, () => { if (cy) { cy.style(cyStyle()) } })

onUnmounted(() => { if (cy) cy.destroy() })

watch(() => [props.nodes, props.edges, props.scopeNodeIds, breadcrumb.value, props.fx?.ts], updateGraph, { deep: true })

function updateGraph() {
  if (!cy) return
  // Node ids that already exist in the graph BEFORE this update. Used to
  // lock only genuinely-pre-existing nodes during re-layout (so their
  // positions are preserved) while letting newly-added nodes be placed.
  // Must NOT rely on the patch event's `added` list: on the snapshot path
  // (opening a run, first load) that list is empty, which previously caused
  // every just-added node to be locked at (0,0) → all nodes overlapped.
  const priorNodeIds = new Set<string>(cy.nodes().map((n: any) => n.id()))
  const wantNodeIds = new Set(visibleNodes.value.map((n: any) => n.id))
  const wantEdgeKeys = new Map<string, any>()
  visibleEdges.value.forEach((e: any, i: number) => wantEdgeKeys.set(`${e.source}->${e.target}`, { e, i }))

  // Remove nodes/edges no longer present.
  cy.nodes().forEach((n: any) => { if (!wantNodeIds.has(n.id())) n.remove() })
  cy.edges().forEach((ed: any) => {
    const k = `${ed.data('source')}->${ed.data('target')}`
    if (!wantEdgeKeys.has(k)) ed.remove()
  })

  // Add new nodes (and refresh classes on existing ones).
  const addedIds = new Set(props.fx?.added || [])
  const animatingIds = new Set<string>()  // nodes whose fade-in we start this round
  let structuralChange = false
  let staggerIdx = 0  // position of each new node within this batch
  for (const n of visibleNodes.value) {
    const klass = [
      scopeSet.value.has(n.id) ? 'in-scope' : '',
      hasChildren(n.id) ? 'complex' : '',
      selectedNode.value?.id === n.id ? 'selected' : '',
    ].filter(Boolean).join(' ')
    const existing = cy.getElementById(n.id)
    if (existing.nonempty()) {
      existing.classes(klass)
      continue
    }
    structuralChange = true
    const el = cy.add({ group: 'nodes', data: { id: n.id, label: n.summary || n.id }, classes: klass })
    // Entrance animation: stagger so a batch of new nodes appears one by
    // one rather than all at once. Each subsequent new node starts 90ms
    // later (capped). Failure-replan replacements flash in faster with no
    // stagger.
    //
    // Robustness: `style('opacity', 0)` sets an inline bypass. If the
    // animation is interrupted (re-render / watch re-fire / re-layout),
    // the node would be stranded at opacity 0 and stay invisible (only
    // its edges would show). So on completion we removeStyle('opacity')
    // to fall back to the stylesheet default (1), and we capture `el` so
    // a stale closure can't matter.
    if (addedIds.has(n.id)) {
      const node = el
      node.style('opacity', 0)
      animatingIds.add(n.id)
      const replaced = props.fx?.replaced
      const delay = replaced ? 0 : Math.min(staggerIdx * 90, 900)
      node.animate(
        { style: { opacity: 1 } },
        { duration: replaced ? 120 : 260, delay, complete: () => node.removeStyle('opacity') },
      )
      staggerIdx++
    }
  }

  // Add new edges.
  for (const [, { e, i }] of wantEdgeKeys) {
    const id = `e${i}`
    if (cy.getElementById(id).nonempty()) continue
    const dup = cy.edges().some((ed: any) => `${ed.data('source')}->${ed.data('target')}` === `${e.source}->${e.target}`)
    if (dup) continue
    structuralChange = true
    cy.add({
      group: 'edges',
      data: { id, source: e.source, target: e.target, label: e.relation },
      classes: [
        scopeSet.value.has(e.source) && scopeSet.value.has(e.target) ? 'in-scope' : '',
        e.relation === 'Contains' ? 'Contains' : '',
      ].filter(Boolean).join(' '),
    })
  }

  // Only re-layout when the graph structure actually changed (nodes/edges
  // added or removed). Lock pre-existing nodes so they keep their positions
  // — only the new nodes get placed — and `fit: false` so the viewport
  // doesn't recenter/zoom. This removes the "whole panel refreshes" flicker:
  // a class-only change (scope/selected) no longer triggers a full re-layout.
  if (structuralChange) {
    // Lock only nodes that existed before this update (preserve their
    // positions); newly-added nodes are left free so the layout places
    // them. Using priorNodeIds — not the patch `added` list — ensures
    // snapshot-path additions (where `added` is empty) still get placed
    // instead of being locked at the origin and overlapping.
    const existingNodes = cy.nodes().filter((n: any) => priorNodeIds.has(n.id()))
    existingNodes.lock()
    cy.layout({ name: 'cose', animate: true, fit: false, randomize: false, idealEdgeLength: 100, nodeRepulsion: 6000 })
      .run()
    existingNodes.unlock()
  }

  // Safety sweep: clear any stranded opacity bypass on nodes that are NOT
  // animating this round. The entrance animation sets opacity:0 then fades
  // to 1 via a `complete` callback — but if a subsequent updateGraph()
  // (e.g. the snapshot that follows every patch) interrupts the animation
  // before it completes, the callback never fires and the node stays
  // invisible at opacity 0 forever (only its edges show). This guarantees
  // every settled node is visible regardless of animation interruption.
  cy.nodes().forEach((n: any) => {
    if (!animatingIds.has(n.id()) && n.style('opacity') !== '1') {
      n.removeStyle('opacity')
    }
  })
}
</script>

<template>
  <div class="graph-container">
    <!-- Breadcrumb -->
    <div class="breadcrumb">
      <button class="bc-btn" @click="goRoot" :class="{ active: !breadcrumb.length }">⬡ Project</button>
      <template v-for="(id, i) in breadcrumb" :key="id">
        <span class="bc-sep">›</span>
        <button class="bc-btn" :class="{ active: i === breadcrumb.length - 1 }" @click="goToLevel(i)">
          {{ id.length > 24 ? id.slice(0, 24) + '…' : id }}
        </button>
      </template>
      <span class="bc-hint" v-if="breadcrumb.length">
        · {{ visibleNodes.length }} nodes · {{ visibleEdges.length }} edges
      </span>
    </div>
    <!-- Legend -->
    <div class="legend-bar">
      <span class="dot complex"></span> drill-down
      <span class="dot scope"></span> in scope ({{ scopeNodeIds?.length || 0 }})
      <span class="dot normal"></span> file/function
    </div>
    <!-- Cytoscape canvas -->
    <div class="graph-canvas" ref="container"></div>
    <!-- Node detail popup -->
    <div v-if="selectedNode" class="node-detail">
      <div class="nd-header">
        <strong>{{ selectedNode.summary || selectedNode.id }}</strong>
        <button @click="selectedNode = null" class="nd-close">&times;</button>
      </div>
      <div class="nd-body">
        <div v-if="selectedNode.l1"><b>L1:</b> {{ selectedNode.l1 }}</div>
        <div v-if="selectedNode.kind"><b>Kind:</b> {{ selectedNode.kind }}</div>
        <div v-if="hasChildren(selectedNode.id)">
          <b>Children:</b> {{ childMap.get(selectedNode.id)!.length }} sub-nodes
          <br><button class="primary" @click="drillDown(selectedNode.id); selectedNode = null">🔍 Drill down</button>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.graph-container { position: relative; flex: 1; display: flex; flex-direction: column; min-height: 0; }
.breadcrumb {
  display: flex; align-items: center; gap: 2px; padding: 4px 8px;
  font-size: 0.7rem; background: var(--bg-panel); border-bottom: 1px solid var(--border);
  flex-wrap: wrap; min-height: 30px;
}
.bc-btn { background: none; color: var(--text-muted); padding: 2px 6px; font-size: 0.7rem; border-radius: 3px; }
.bc-btn:hover { color: var(--text); }
.bc-btn.active { color: var(--accent); font-weight: 600; }
.bc-sep { color: var(--text-muted); }
.bc-hint { color: var(--text-muted); font-size: 0.65rem; margin-left: 8px; }
.legend-bar { display: flex; gap: 12px; align-items: center; padding: 2px 10px; font-size: 0.6rem; color: var(--text-muted); background: var(--bg); border-bottom: 1px solid var(--border); }
.dot { display: inline-block; width: 7px; height: 7px; border-radius: 50%; margin-right: 3px; }
.dot.complex { background: #f59e0b; border: 1px solid #f59e0b; }
.dot.scope { background: #059669; }
.dot.normal { background: #7c3aed; }
.graph-canvas { flex: 1; min-height: 200px; }
.node-detail {
  position: absolute; bottom: 8px; right: 8px; width: 260px; max-height: 200px; overflow-y: auto;
  background: var(--bg-panel); border: 1px solid var(--border); border-radius: var(--radius);
  padding: 8px 10px; box-shadow: var(--shadow-md); z-index: 20; font-size: 0.72rem;
}
.nd-header { display: flex; justify-content: space-between; align-items: flex-start; margin-bottom: 6px; }
.nd-close { background: none; color: var(--text-muted); font-size: 1rem; padding: 0 4px; line-height: 1; }
.nd-body > div { margin: 3px 0; }
.nd-body button { margin-top: 4px; font-size: 0.65rem; padding: 3px 8px; }
</style>
