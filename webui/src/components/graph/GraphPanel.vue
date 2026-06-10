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
        { selector: 'node', style: { 'background-color': '#3b82f6', 'label': 'data(label)', 'color': '#e2e8f0', 'text-wrap': 'wrap', 'text-max-width': '110px', 'font-size': '9px' } },
        { selector: 'edge', style: { 'width': 1, 'line-color': '#64748b', 'target-arrow-color': '#64748b', 'target-arrow-shape': 'triangle', 'curve-style': 'bezier', 'label': 'data(label)', 'font-size': '7px', 'color': '#94a3b8' } },
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
.graph-panel { flex: 1; min-height: 300px; }
</style>
