<script setup lang="ts">
import { ref, watch, onMounted, onUnmounted, computed, nextTick } from 'vue'
import * as THREE from 'three'
import { OrbitControls } from 'three/examples/jsm/controls/OrbitControls.js'
import { CSS2DRenderer, CSS2DObject } from 'three/examples/jsm/renderers/CSS2DRenderer.js'
import { useGraphColors } from '../../composables/useGraphColors'
import { theme } from '../../composables/useTheme'

const props = defineProps<{ nodes: any[]; edges: any[]; scopeNodeIds?: string[]; fx?: { added: string[]; removed: string[]; replaced: boolean; ts: number } }>()
const container = ref<HTMLElement>()
const emit = defineEmits(['drillDown'])

let scene: THREE.Scene, camera: THREE.PerspectiveCamera, renderer: THREE.WebGLRenderer
let controls: OrbitControls, labelRenderer: CSS2DRenderer
let nodeGroups: Map<string, THREE.Group> = new Map()
let edgeGroups: Map<string, THREE.Group> = new Map()
let animFrame: number
const scopeSet = computed(() => new Set(props.scopeNodeIds || []))
const C = useGraphColors()
const NODE_R = 0.25, SCOPE_R = 0.35

function labelDiv(text: string, size = '10px', color = C.text): CSS2DObject {
  const div = document.createElement('div')
  div.textContent = text.length > 22 ? text.slice(0, 22) + '…' : text
  div.style.fontSize = size; div.style.color = color
  div.style.fontWeight = '500'; div.style.textShadow = `0 0 4px ${C.bg}`
  div.style.whiteSpace = 'nowrap'; div.style.fontFamily = 'system-ui, sans-serif'
  return new CSS2DObject(div)
}

function posForNode(idx: number): THREE.Vector3 {
  const a = (idx / Math.max(props.nodes.length || 1, 1)) * Math.PI * 2
  return new THREE.Vector3(Math.cos(a) * 4.5, (Math.random() - 0.5) * 2.5, Math.sin(a) * 4.5)
}

function initScene() {
  if (!container.value) return
  const w = container.value.clientWidth, h = container.value.clientHeight
  scene = new THREE.Scene(); scene.background = new THREE.Color(C.bg)
  scene.fog = new THREE.Fog(C.bg, 15, 40)
  camera = new THREE.PerspectiveCamera(50, w / h, 0.5, 60)
  camera.position.set(6, 4, 8); camera.lookAt(0, 0, 0)
  renderer = new THREE.WebGLRenderer({ antialias: true }); renderer.setSize(w, h); renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2))
  container.value.appendChild(renderer.domElement)
  labelRenderer = new CSS2DRenderer(); labelRenderer.setSize(w, h)
  labelRenderer.domElement.style.position = 'absolute'; labelRenderer.domElement.style.top = '0'; labelRenderer.domElement.style.pointerEvents = 'none'
  container.value.appendChild(labelRenderer.domElement)
  controls = new OrbitControls(camera, renderer.domElement)
  controls.enableDamping = true; controls.dampingFactor = 0.08; controls.target.set(0, 0, 0)
  scene.add(new THREE.AmbientLight(0xffffff, 2))
  const d = new THREE.DirectionalLight(0xffffff, 3); d.position.set(10, 20, 10); scene.add(d)
  scene.add(new THREE.GridHelper(20, 20, C.grid, C.grid))
  animate()
}

function animate() {
  animFrame = requestAnimationFrame(animate)
  controls.update()
  renderer.render(scene, camera)
  labelRenderer.render(scene, camera)
}

function makeNodeGroup(id: string, label: string, inScope: boolean, hasKids: boolean): THREE.Group {
  const g = new THREE.Group(); g.name = id
  g.userData = { id, label, inScope, hasKids }
  const r = inScope ? SCOPE_R : NODE_R
  const c = inScope ? C.scope : hasKids ? C.complex : C.node
  g.add(new THREE.Mesh(new THREE.SphereGeometry(r, 32, 16), new THREE.MeshStandardMaterial({ color: c, roughness: 0.3, metalness: 0.1, emissive: c, emissiveIntensity: 0.2 })))
  if (hasKids) g.add(new THREE.Mesh(new THREE.TorusGeometry(r + 0.08, 0.04, 16, 32), new THREE.MeshStandardMaterial({ color: C.complex, emissive: C.complex, emissiveIntensity: 0.4 })))
  if (inScope) g.add(new THREE.Mesh(new THREE.SphereGeometry(r + 0.2, 32, 16), new THREE.MeshBasicMaterial({ color: C.scope, transparent: true, opacity: 0.15 })))
  const lbl = labelDiv(label || id); lbl.position.y = r + 0.4; g.add(lbl)
  return g
}

function makeEdgeGroup(from: THREE.Vector3, to: THREE.Vector3, inScope: boolean, label: string): THREE.Group {
  const g = new THREE.Group(); g.userData = { inScope }
  const color = inScope ? C.scope : C.edge
  // Line
  const lGeo = new THREE.BufferGeometry().setFromPoints([from, to])
  g.add(new THREE.Line(lGeo, new THREE.LineBasicMaterial({ color, transparent: true, opacity: inScope ? 0.7 : 0.3 })))
  // Arrow cone
  const dir = new THREE.Vector3().subVectors(to, from)
  const cone = new THREE.Mesh(new THREE.ConeGeometry(0.1, 0.25, 8, 4), new THREE.MeshStandardMaterial({ color, roughness: 0.3, emissive: color, emissiveIntensity: 0.2 }))
  cone.position.copy(from.clone().add(dir.clone().multiplyScalar(0.88))); cone.lookAt(to); cone.rotateX(Math.PI / 2)
  g.add(cone)
  // Midpoint label
  if (label) {
    const mid = new THREE.Vector3().addVectors(from, to).multiplyScalar(0.5)
    const lbl = labelDiv(label, '8px', '#787878'); lbl.position.copy(mid).add(new THREE.Vector3(0, 0.15, 0)); g.add(lbl)
  }
  return g
}

function disposeGroup(g: THREE.Group) {
  g.traverse((obj: any) => {
    if (obj.geometry) obj.geometry.dispose()
    if (obj.material) {
      if (Array.isArray(obj.material)) obj.material.forEach((m: any) => m.dispose())
      else obj.material.dispose()
    }
  })
}

function clearAll() {
  for (const [, g] of nodeGroups) { scene.remove(g); disposeGroup(g) }
  for (const [, g] of edgeGroups) { scene.remove(g); disposeGroup(g) }
  nodeGroups.clear(); edgeGroups.clear()
}

function updateGraph() {
  if (!scene) return
  const ns = props.nodes, es = props.edges
  // Full clear when switching to empty graph (new chat)
  if (ns.length === 0) { clearAll(); return }
  const newIds = new Set(ns.map((n: any) => n.id))
  // Remove stale
  for (const [id, g] of nodeGroups) { if (!newIds.has(id)) { scene.remove(g); disposeGroup(g); nodeGroups.delete(id) } }
  for (const [k, g] of edgeGroups) {
    const [s, t] = k.split('->')
    if (!newIds.has(s) || !newIds.has(t)) { scene.remove(g); disposeGroup(g); edgeGroups.delete(k) }
  }
  // Add new nodes
  ns.forEach((n: any, i: number) => {
    if (nodeGroups.has(n.id)) return
    const inS = scopeSet.value.has(n.id)
    const hasKids = es.some((e: any) => e.relation === 'Contains' && e.source === n.id)
    const g = makeNodeGroup(n.id, n.summary || n.id, inS, hasKids)
    g.position.copy(posForNode(i)); g.scale.set(0.01, 0.01, 0.01); g.userData.animS = 0.01
    scene.add(g); nodeGroups.set(n.id, g)
  })
  // Add new edges
  es.forEach((e: any) => {
    const k = `${e.source}->${e.target}`; if (edgeGroups.has(k)) return
    const sg = nodeGroups.get(e.source), tg = nodeGroups.get(e.target); if (!sg || !tg) return
    const inS = scopeSet.value.has(e.source) && scopeSet.value.has(e.target)
    const g = makeEdgeGroup(sg.position, tg.position, inS, e.relation)
    g.userData.src = e.source; g.userData.tgt = e.target
    scene.add(g); edgeGroups.set(k, g)
  })
}

function updateEdgePositions() {
  for (const [, g] of edgeGroups) {
    const sg = nodeGroups.get(g.userData.src), tg = nodeGroups.get(g.userData.tgt)
    if (!sg || !tg) continue
    // Rebuild line + cone positions
    const from = sg.position, to = tg.position
    const line = g.children.find((c: any) => c.isLine) as THREE.Line
    if (line) (line.geometry as THREE.BufferGeometry).setFromPoints([from, to])
    const cone = g.children.find((c: any) => c.isMesh && (c as THREE.Mesh).geometry.type === 'ConeGeometry') as THREE.Mesh
    if (cone) {
      const dir = new THREE.Vector3().subVectors(to, from)
      cone.position.copy(from.clone().add(dir.clone().multiplyScalar(0.88))); cone.lookAt(to); cone.rotateX(Math.PI / 2)
    }
  }
}

function animLoop() {
  requestAnimationFrame(animLoop)
  let changed = false
  for (const [, g] of nodeGroups) {
    if (g.userData.animS < 1) { g.userData.animS = Math.min(1, g.userData.animS + 0.05); g.scale.setScalar(g.userData.animS); changed = true }
  }
  if (changed) updateEdgePositions()
}

const raycaster = new THREE.Raycaster(), mouse = new THREE.Vector2()
function onClick(e: MouseEvent) {
  if (!container.value) return
  const rect = container.value.getBoundingClientRect()
  mouse.x = ((e.clientX - rect.left) / rect.width) * 2 - 1
  mouse.y = -((e.clientY - rect.top) / rect.height) * 2 + 1
  raycaster.setFromCamera(mouse, camera)
  const hits = raycaster.intersectObjects([...nodeGroups.values()].flatMap(g => g.children.filter((c: any) => c.isMesh)), true)
  if (hits.length > 0) {
    let obj = hits[0].object
    while (obj && !obj.name) obj = obj.parent as THREE.Object3D
    if (obj && obj.name) {
      const hasKids = props.edges.some((e: any) => e.relation === 'Contains' && e.source === obj.name)
      if (hasKids) emit('drillDown', obj.name)
    }
  }
}

watch(() => [props.nodes, props.edges], () => nextTick(updateGraph), { deep: true })
onMounted(() => { initScene(); updateGraph(); animLoop(); container.value?.addEventListener('click', onClick) })
watch(theme, () => {
  if (!scene) return
  scene.background = new THREE.Color(C.bg)
  scene.fog = new THREE.Fog(C.bg, 15, 40)
  clearAll()
  updateGraph()
})
onUnmounted(() => { cancelAnimationFrame(animFrame); renderer?.dispose(); container.value?.removeEventListener('click', onClick) })
</script>

<template>
  <div class="graph-3d" ref="container">
    <div class="legend-3d">
      <span class="dot scope"></span> scope ({{ scopeNodeIds?.length || 0 }})
      <span class="dot node"></span> node
      <span class="dot complex"></span> drill
    </div>
  </div>
</template>

<style scoped>
.graph-3d { flex: 1; min-height: 300px; position: relative; cursor: grab; }
.graph-3d:active { cursor: grabbing; }
.legend-3d { position: absolute; bottom: 10px; right: 10px; z-index: 10; display: flex; gap: 12px; font-size: 0.65rem; color: var(--text-muted); background: var(--bg-panel); padding: 4px 10px; border-radius: 6px; box-shadow: var(--shadow); }
.dot { display: inline-block; width: 8px; height: 8px; border-radius: 50%; margin-right: 3px; }
.dot.scope { background: var(--success); } .dot.node { background: var(--accent); } .dot.complex { background: var(--warning); border: 1px solid var(--warning); }
</style>
