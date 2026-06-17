<script setup lang="ts">
import { ref, watch, onMounted, onUnmounted, computed, nextTick } from 'vue'
import * as THREE from 'three'
import { OrbitControls } from 'three/examples/jsm/controls/OrbitControls.js'

const props = defineProps<{ nodes: any[]; edges: any[]; scopeNodeIds?: string[] }>()
const container = ref<HTMLElement>()
const emit = defineEmits(['drillDown'])

let scene: THREE.Scene, camera: THREE.PerspectiveCamera, renderer: THREE.WebGLRenderer, controls: OrbitControls
let nodeMeshes: Map<string, THREE.Mesh> = new Map()
let edgeLines: Map<string, THREE.Line> = new Map()
let animFrame: number

const scopeSet = computed(() => new Set(props.scopeNodeIds || []))
const NODE_R = 0.25, SCOPE_R = 0.35

function posForNode(id: string, idx: number): THREE.Vector3 {
  const angle = (idx / Math.max(props.nodes.length, 1)) * Math.PI * 2
  const radius = 4 + Math.random() * 2
  const h = (Math.random() - 0.5) * 3
  return new THREE.Vector3(Math.cos(angle) * radius, h, Math.sin(angle) * radius)
}

function initScene() {
  if (!container.value) return
  const w = container.value.clientWidth, h = container.value.clientHeight

  scene = new THREE.Scene()
  scene.background = new THREE.Color('#f5f5f0')
  scene.fog = new THREE.Fog('#f5f5f0', 15, 40)

  camera = new THREE.PerspectiveCamera(50, w / h, 0.5, 60)
  camera.position.set(8, 5, 10)
  camera.lookAt(0, 0, 0)

  renderer = new THREE.WebGLRenderer({ antialias: true })
  renderer.setSize(w, h)
  renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2))
  container.value.appendChild(renderer.domElement)

  controls = new OrbitControls(camera, renderer.domElement)
  controls.enableDamping = true
  controls.dampingFactor = 0.08
  controls.target.set(0, 0, 0)

  scene.add(new THREE.AmbientLight(0xffffff, 2))
  const dir = new THREE.DirectionalLight(0xffffff, 3)
  dir.position.set(10, 20, 10)
  scene.add(dir)

  // Grid helper for orientation
  const grid = new THREE.GridHelper(20, 20, '#e0ddd6', '#e8e5df')
  scene.add(grid)

  animate()
}

function animate() {
  animFrame = requestAnimationFrame(animate)
  controls.update()
  renderer.render(scene, camera)
}

function makeNode(id: string, label: string, inScope: boolean, hasChildren: boolean): THREE.Mesh {
  const r = inScope ? SCOPE_R : NODE_R
  const color = inScope ? '#059669' : hasChildren ? '#f59e0b' : '#7c3aed'
  const geo = new THREE.SphereGeometry(r, 32, 16)
  const mat = new THREE.MeshStandardMaterial({ color, roughness: 0.3, metalness: 0.1, emissive: color, emissiveIntensity: 0.2 })
  const mesh = new THREE.Mesh(geo, mat)
  mesh.name = id
  mesh.userData = { id, label, inScope, hasChildren }

  // Ring for complex nodes
  if (hasChildren) {
    const ring = new THREE.Mesh(
      new THREE.TorusGeometry(r + 0.08, 0.04, 16, 32),
      new THREE.MeshStandardMaterial({ color: '#f59e0b', emissive: '#f59e0b', emissiveIntensity: 0.4 })
    )
    mesh.add(ring)
  }

  // Glow
  if (inScope) {
    const glow = new THREE.Mesh(
      new THREE.SphereGeometry(r + 0.2, 32, 16),
      new THREE.MeshBasicMaterial({ color: '#059669', transparent: true, opacity: 0.15 })
    )
    mesh.add(glow)
  }

  return mesh
}

function makeEdge(source: THREE.Vector3, target: THREE.Vector3, inScope: boolean): THREE.Line {
  const pts = [source, target]
  const geo = new THREE.BufferGeometry().setFromPoints(pts)
  const mat = new THREE.LineBasicMaterial({
    color: inScope ? '#059669' : '#c4b5e0',
    transparent: true,
    opacity: inScope ? 0.8 : 0.4,
  })
  return new THREE.Line(geo, mat)
}

function updateGraph() {
  if (!scene) return
  const ns = props.nodes, es = props.edges
  const newIds = new Set(ns.map((n: any) => n.id))

  // Remove old
  for (const [id, mesh] of nodeMeshes) {
    if (!newIds.has(id)) {
      scene.remove(mesh)
      nodeMeshes.delete(id)
    }
  }
  for (const [key, line] of edgeLines) {
    const [s, t] = key.split('->')
    if (!newIds.has(s) || !newIds.has(t)) {
      scene.remove(line)
      edgeLines.delete(key)
    }
  }

  // Add new nodes with animation
  ns.forEach((n: any, i: number) => {
    if (nodeMeshes.has(n.id)) return
    const inScope = scopeSet.value.has(n.id)
    const hasChildren = es.some((e: any) => e.relation === 'Contains' && e.source === n.id)
    const mesh = makeNode(n.id, n.summary || n.id, inScope, hasChildren)
    const pos = posForNode(n.id, i)
    mesh.position.copy(pos)
    mesh.userData.targetPos = pos.clone()
    mesh.scale.set(0.01, 0.01, 0.01)  // start tiny → animate in
    mesh.userData.animScale = 0.01
    scene.add(mesh)
    nodeMeshes.set(n.id, mesh)
  })

  // Update edges
  const edgeKeys = new Set<string>()
  es.forEach((e: any) => {
    const key = `${e.source}->${e.target}`
    edgeKeys.add(key)
    if (edgeLines.has(key)) return
    const sMesh = nodeMeshes.get(e.source), tMesh = nodeMeshes.get(e.target)
    if (!sMesh || !tMesh) return
    const inScope = scopeSet.value.has(e.source) && scopeSet.value.has(e.target)
    const line = makeEdge(sMesh.position, tMesh.position, inScope)
    line.userData = { source: e.source, target: e.target, inScope }
    scene.add(line)
    edgeLines.set(key, line)
  })

  // Animate new nodes (frame loop updates scale)
}

function updateEdgePositions() {
  for (const [, line] of edgeLines) {
    const { source, target } = line.userData
    const sMesh = nodeMeshes.get(source), tMesh = nodeMeshes.get(target)
    if (sMesh && tMesh) {
      const pts = [sMesh.position, tMesh.position]
      ;(line.geometry as THREE.BufferGeometry).setFromPoints(pts)
    }
  }
}

// Animation loop: smooth scale-in for new nodes, edge position updates
function animLoop() {
  requestAnimationFrame(animLoop)
  let changed = false
  for (const [, mesh] of nodeMeshes) {
    if (mesh.userData.animScale < 1) {
      mesh.userData.animScale = Math.min(1, mesh.userData.animScale + 0.05)
      mesh.scale.setScalar(mesh.userData.animScale)
      changed = true
    }
  }
  if (changed) updateEdgePositions()
}

// Raycaster for click → drill down
const raycaster = new THREE.Raycaster()
const mouse = new THREE.Vector2()
function onClick(e: MouseEvent) {
  if (!container.value || !renderer) return
  const rect = container.value.getBoundingClientRect()
  mouse.x = ((e.clientX - rect.left) / rect.width) * 2 - 1
  mouse.y = -((e.clientY - rect.top) / rect.height) * 2 + 1
  raycaster.setFromCamera(mouse, camera)
  const hits = raycaster.intersectObjects([...nodeMeshes.values()])
  if (hits.length > 0) {
    const id = hits[0].object.name
    const hasChildren = props.edges.some((e: any) => e.relation === 'Contains' && e.source === id)
    if (hasChildren) emit('drillDown', id)
  }
}

watch(() => [props.nodes, props.edges], () => nextTick(updateGraph), { deep: true })

onMounted(() => {
  initScene()
  updateGraph()
  animLoop()
  if (container.value) container.value.addEventListener('click', onClick)
})
onUnmounted(() => {
  cancelAnimationFrame(animFrame)
  if (container.value) container.value.removeEventListener('click', onClick)
  renderer?.dispose()
})
</script>

<template>
  <div class="graph-3d" ref="container">
    <div class="legend-3d">
      <span class="dot scope"></span> in scope ({{ scopeNodeIds?.length || 0 }})
      <span class="dot node"></span> node
      <span class="dot complex"></span> drill-down
    </div>
  </div>
</template>

<style scoped>
.graph-3d { flex: 1; min-height: 300px; position: relative; cursor: grab; }
.graph-3d:active { cursor: grabbing; }
.legend-3d {
  position: absolute; bottom: 10px; right: 10px; z-index: 10;
  display: flex; gap: 12px; font-size: 0.65rem; color: #787878;
  background: #fff; padding: 4px 10px; border-radius: 6px; box-shadow: var(--shadow);
}
.dot { display: inline-block; width: 8px; height: 8px; border-radius: 50%; margin-right: 3px; }
.dot.scope { background: #059669; }
.dot.node { background: #7c3aed; }
.dot.complex { background: #f59e0b; border: 1px solid #f59e0b; }
</style>
