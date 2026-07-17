import { createApp } from 'vue'
import { createRouter, createWebHashHistory } from 'vue-router'
import App from './App.vue'
import RunView from './components/run/RunView.vue'
import HistoryView from './components/history/HistoryView.vue'
import SkillsView from './components/skills/SkillsView.vue'
import FilesView from './components/files/FilesView.vue'
import SettingsView from './components/config/SettingsView.vue'
import UsageView from './components/usage/UsageView.vue'
import { applyTheme, applyStyle } from './composables/useTheme'
import './styles/themes.css'
import './styles/main.css'

const routes = [
  { path: '/', component: RunView },
  { path: '/runs', component: HistoryView },
  { path: '/usage', component: UsageView },
  { path: '/skills', component: SkillsView },
  { path: '/files', component: FilesView },
  { path: '/settings', component: SettingsView },
]

const router = createRouter({ history: createWebHashHistory(), routes })
applyTheme()  // 在挂载前应用主题,避免首帧闪白
applyStyle()  // 同理:首帧前同步 data-style,避免默认态闪现
createApp(App).use(router).mount('#app')
