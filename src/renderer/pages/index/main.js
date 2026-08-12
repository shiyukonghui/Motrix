import { is, ipcRenderer, install } from '@shims'
import { initEngineEvents } from '@shims/events'
import Vue from 'vue'
import VueI18Next from '@panter/vue-i18next'
import { sync } from 'vuex-router-sync'
import Element, { Loading, Message } from 'element-ui'
import axios from 'axios'

import App from './App'
import router from '@/router'
import store from '@/store'
import { getLocaleManager } from '@/components/Locale'
import Icon from '@/components/Icons/Icon'
import Msg from '@/components/Msg'
import { commands } from '@/components/CommandManager/instance'
import TrayWorker from '@/workers/tray.worker'

import '@/components/Theme/Index.scss'

const updateTray = is.renderer()
  ? async (payload) => {
    const { tray } = payload
    if (!tray) {
      return
    }

    const ab = await tray.arrayBuffer()
    // —— Tauri 桥接修正（Phase 4）：emit 无法正确序列化 ArrayBuffer（JSON.stringify 后为空对象），
    //    改为发送 { width, height, data: [...] } 结构化载荷。
    //    尺寸约定：TRAY_CANVAS_CONFIG（66×16，src/shared/constants.js）× scale(2) = 132×32，
    //    Rust 端按 132×32 约定构造托盘图标（载荷为 PNG 编码时 Rust 端自动解码出真实宽高）。
    //    ab 为 PNG 编码字节（worker 的 convertToBlob 输出），宽度/高度仅作原始 RGBA 回退用。
    const bytes = new Uint8Array(ab)
    ipcRenderer.send('command', 'application:update-tray', {
      width: 132,
      height: 32,
      data: Array.from(bytes)
    })
  }
  : () => {}

function initTrayWorker () {
  const worker = new TrayWorker()

  worker.addEventListener('message', (event) => {
    const { type, payload } = event.data

    switch (type) {
    case 'initialized':
    case 'log':
      console.log('[Motrix] Log from Tray Worker: ', payload)
      break
    case 'tray:drawed':
      updateTray(payload)
      break
    default:
      console.warn('[Motrix] Tray Worker unhandled message type:', type, payload)
    }
  })

  return worker
}

function init (config) {
  if (is.renderer()) {
    Vue.use(install)
  }

  Vue.http = Vue.prototype.$http = axios
  Vue.config.productionTip = false

  const { locale } = config
  const localeManager = getLocaleManager()
  localeManager.changeLanguageByLocale(locale)

  Vue.use(VueI18Next)
  const i18n = new VueI18Next(localeManager.getI18n())
  Vue.use(Element, {
    size: 'mini',
    i18n: (key, value) => i18n.t(key, value)
  })
  Vue.use(Msg, Message, {
    showClose: true
  })
  Vue.component('mo-icon', Icon)

  const loading = Loading.service({
    fullscreen: true,
    background: 'rgba(0, 0, 0, 0.1)'
  })

  sync(store, router)

  // 订阅 Rust 端 engine:* 事件（全局统计 / 任务快照 → Vuex），
  // 替代 Electron 版"轮询 + JSON-RPC"状态同步（见 MIGRATION-TAURI.md 5.8）
  initEngineEvents(store)

  /* eslint-disable no-new */
  global.app = new Vue({
    components: { App },
    router,
    store,
    i18n,
    template: '<App/>'
  }).$mount('#app')

  global.app.commands = commands
  require('./commands')

  global.app.trayWorker = initTrayWorker()

  setTimeout(() => {
    loading.close()
  }, 400)
}

store.dispatch('preference/fetchPreference')
  .then((config) => {
    console.info('[Motrix] load preference:', config)
    init(config)
  })
  .catch((err) => {
    alert(err)
  })
