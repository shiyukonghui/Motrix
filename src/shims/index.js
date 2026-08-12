// Unified entry for the Tauri shims.
// Installs `$electron` on Vue.prototype (replacing `vue-electron`) and exposes
// every shim object so the renderer code can import them directly.
import { is } from './electron-is'
import { ipcRenderer } from './ipcRenderer'
import { shell } from './remote-shell'
import { nativeTheme } from './remote-nativeTheme'
import { dialog, app, webContents, getCurrentWindow } from './remote'

// Tauri WebView 不提供 Node.js 全局，补齐 setImmediate / clearImmediate：
// Electron 版渲染进程有 Node 集成，部分组件（如 AddTask.vue 的 URI 粘贴事件
// `setImmediate(() => …)` 延迟读取输入值）直接依赖这两个全局；浏览器环境没有，
// 用 setTimeout 0 等价模拟，避免 "ReferenceError: setImmediate is not defined"。
if (typeof window !== 'undefined') {
  if (typeof window.setImmediate !== 'function') {
    window.setImmediate = (fn, ...args) => setTimeout(() => fn(...args), 0)
  }
  if (typeof window.clearImmediate !== 'function') {
    window.clearImmediate = (id) => clearTimeout(id)
  }
}

export function install (Vue) {
  Vue.prototype.$electron = { ipcRenderer }
  window.__shims__ = { is, ipcRenderer, shell, nativeTheme }
}

export {
  is,
  ipcRenderer,
  shell,
  nativeTheme,
  dialog,
  app,
  webContents,
  getCurrentWindow
}

export default is
