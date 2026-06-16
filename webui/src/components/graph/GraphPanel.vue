<script setup lang="ts">
import { ref, watch, onMounted, onUnmounted, computed } from 'vue'

const props = defineProps<{ nodes: any[]; edges: any[]; scopeNodeIds?: string[] }>()
const container = ref<HTMLElement>()
let cy: any = null

const scopeSet = computed(() => new Set(props.scopeNodeIds || []))

const NODE_COLOR = '#7c3aed'
const NODE_COLOR_MUTED = '#c4b5e0'
const SCOPE_COLOR = '#059669'
const SCOPE_BG = '#d1fae5'

onMounted(() => {
  if (container.value && (window as any).cytoscape) {
    cy = (window as any).cytoscape({
      container: container.value,
      style: [
        {
          selector: 'node',
          style: {
            'background-color': NODE_COLOR,
            'label': 'data(label)',
            'color': '#1a1a2e',
            'text-wrap': 'wrap',
            'text-max-width': '110px',
            'font-size': '9px',
            'background-opacity': 0.9,
            'border-width': 1,
            'border-color': NODE_COLOR,
          },
        },
        {
          selector: 'node.in-scope',
          style: {
            'background-color': SCOPE_COLOR,
            'border-color': SCOPE_COLOR,
            'border-width': 2,
            'text-outline-color': SCOPE_BG,
            'text-outline-width': 3,
          },
        },
        {
          selector: 'node.muted',
          style: {
            'background-color': NODE_COLOR_MUTED,
            'background-opacity': 0.4,
            'border-color': NODE_COLOR_MUTED,
            'color': '#94a3b8',
          },
        },
        {
          selector: 'edge',
          style: {
            'width': 1.5,
            'line-color': '#c4b5e0',
            'target-arrow-color': '#a78bda',
            'target-arrow-shape': 'triangle',
            'curve-style': 'bezier',
            'label': 'data(label)',
            'font-size': '7px',
            'color': '#787878',
          },
        },
        {
          selector: 'edge.in-scope',
          style: {
            'line-color': SCOPE_COLOR,
            'target-arrow-color': SCOPE_COLOR,
            'width': 2,
          },
        },
        {
          selector: 'edge.muted',
          style: {
            'line-color': '#e0d8f0',
            'target-arrow-color': '#e0d8f0',
            'width': 0.8,
          },
        },
      ],
      layout: { name: 'cose', animate: false, idealEdgeLength: 80, nodeRepulsion: 4000 },
    })
    updateGraph()
  }
})

onUnmounted(() => { if (cy) cy.destroy() })

watch(() => [props.nodes, props.edges, props.scopeNodeIds], updateGraph, { deep: true })

function updateGraph() {
  if (!cy) return
  cy.elements().remove()
  const hasScope = scopeSet.value.size > 0

  cy.add([
    ...props.nodes.map((n: any) => ({
      data: {
        id: n.id,
        label: n.summary || n.id,
      },
      classes: hasScope
        ? (scopeSet.value.has(n.id) ? 'in-scope' : 'muted')
        : '',
    })),
    ...props.edges.map((e: any, i: number) => ({
      data: { id: `e${i}`, source: e.source, target: e.target, label: e.relation },
      classes: hasScope
        ? (scopeSet.value.has(e.source) && scopeSet.value.has(e.target) ? 'in-scope' : 'muted')
        : '',
    })),
  ])
  cy.layout({ name: 'cose', animate: false }).run()
}
</script>

<template>
  <div class="graph-panel" ref="container">
    <div v-if="scopeNodeIds && scopeNodeIds.length" class="scope-legend">
      <span class="dot in"></span> in scope ({{ scopeNodeIds.length }})
      <span class="dot out"></span> out of scope
    </div>
  </div>
</template>

<style scoped>
.graph-panel { flex: 1; min-height: 300px; background: var(--bg); border-radius: var(--radius); position: relative; }
.scope-legend {
  position: absolute; bottom: 8px; right: 8px; z-index: 10;
  display: flex; gap: 12px; align-items: center;
  font-size: 0.65rem; color: var(--text-muted);
  background: var(--bg-panel); padding: 4px 8px; border-radius: 4px;
  border: 1px solid var(--border);
}
.dot { display: inline-block; width: 8px; height: 8px; border-radius: 50%; margin-right: 3px; }
.dot.in { background: #059669; }
.dot.out { background: #c4b5e0; }
</style>
