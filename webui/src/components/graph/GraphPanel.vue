<script setup lang="ts">
import { ref, watch, onMounted, onUnmounted } from 'vue'

const props = defineProps<{ nodes: any[]; edges: any[] }>()
const container = ref<HTMLElement>()
let cy: any = null

onMounted(() => {
  if (container.value && (window as any).cytoscape) {
    cy = (window as any).cytoscape({
      container: container.value,
      style: [
        { selector: 'node', style: { 'background-color': '#7c3aed', 'label': 'data(label)', 'color': '#1a1a2e', 'text-wrap': 'wrap', 'text-max-width': '110px', 'font-size': '9px', 'background-opacity': 0.9 } },
        { selector: 'edge', style: { 'width': 1.5, 'line-color': '#c4b5e0', 'target-arrow-color': '#a78bda', 'target-arrow-shape': 'triangle', 'curve-style': 'bezier', 'label': 'data(label)', 'font-size': '7px', 'color': '#787878' } },
      ],
      layout: { name: 'cose', animate: false, idealEdgeLength: 80, nodeRepulsion: 4000 },
    })
    updateGraph()
  }
})

onUnmounted(() => { if (cy) cy.destroy() })

watch(() => [props.nodes, props.edges], updateGraph, { deep: true })

function updateGraph() {
  if (!cy) return
  cy.elements().remove()
  cy.add([
    ...props.nodes.map((n: any) => ({ data: { id: n.id, label: n.summary || n.id } })),
    ...props.edges.map((e: any, i: number) => ({ data: { id: `e${i}`, source: e.source, target: e.target, label: e.relation } })),
  ])
  cy.layout({ name: 'cose', animate: false }).run()
}
</script>

<template>
  <div class="graph-panel" ref="container"></div>
</template>

<style scoped>
.graph-panel { flex: 1; min-height: 300px; background: var(--bg); border-radius: var(--radius); }
</style>
