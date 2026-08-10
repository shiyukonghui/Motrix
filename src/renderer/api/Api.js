// Api.js：前端数据管线（Task 11 改造）
//
// 由「高频轮询 + WebSocket JSON-RPC（@shared/aria2）」切换为：
// - 操作类方法 → Tauri command（invoke）：add_uri / pause_task / resume_task /
//   remove_task / change_option / change_global_option / get_global_stat 等；
// - 查询类方法 → 事件数据 / 按需 invoke：任务列表来自 engine:snapshot 事件
//   （src/shims/events.js 驱动 Vuex 增量更新），详情按需 invoke('get_task_detail')，
//   全局统计按需 invoke('get_global_stat')；
// - 配置读写（get-app-config / application:save-preference）继续走 shim 通道。
//
// 保留类结构与全部方法名（store / commands.js / 视觉组件调用方无需改动），
// @shared/utils 的 formatOptionsForEngine / changeKeysToCamelCase 等继续使用。
// 对应 MIGRATION-TAURI.md 5.8 / 8.2 与 tasks.md Task 11。
import { invoke } from '@tauri-apps/api/core'
import { ipcRenderer } from '@shims'
import { isEmpty } from 'lodash'
import {
  separateConfig,
  formatOptionsForEngine,
  changeKeysToCamelCase,
  changeKeysToKebabCase
} from '@shared/utils'

// 批量方法 → 单任务 command 的映射（Task 11：优先循环调用，减少 Rust 批量 command）
const BATCH_METHOD_MAP = {
  'aria2.changeOption': (gid, options) => invoke('change_option', { gid, options }),
  'aria2.remove': (gid) => invoke('remove_task', { gid }),
  'aria2.unpause': (gid) => invoke('resume_task', { gid }),
  'aria2.pause': (gid) => invoke('pause_task', { gid }),
  'aria2.forcePause': (gid) => invoke('pause_task', { gid })
}

export default class Api {
  constructor (options = {}) {
    this.options = options

    this.init()
  }

  async init () {
    this.config = await this.loadConfig()
  }

  loadConfigFromLocalStorage () {
    // TODO
    const result = {}
    return result
  }

  async loadConfigFromNativeStore () {
    // get-app-config 仍走 shim（Phase 0 已打通 Tauri command 通道）
    const result = await ipcRenderer.invoke('get-app-config')
    return result
  }

  async loadConfig () {
    let result = await this.loadConfigFromNativeStore()
    result = changeKeysToCamelCase(result)
    return result
  }

  fetchPreference () {
    return new Promise((resolve) => {
      this.config = this.loadConfig()
      resolve(this.config)
    })
  }

  savePreference (params = {}) {
    const kebabParams = changeKeysToKebabCase(params)
    return this.savePreferenceToNativeStore(kebabParams)
  }

  savePreferenceToLocalStorage () {
    // TODO
  }

  savePreferenceToNativeStore (params = {}) {
    const { user, system, others } = separateConfig(params)
    const config = {}

    if (!isEmpty(user)) {
      console.info('[Motrix] save user config: ', user)
      config.user = user
    }

    if (!isEmpty(system)) {
      console.info('[Motrix] save system config: ', system)
      config.system = system
      this.updateActiveTaskOption(system)
    }

    if (!isEmpty(others)) {
      console.info('[Motrix] save config found illegal key: ', others)
    }

    ipcRenderer.send('command', 'application:save-preference', config)
  }

  // ==================================================================
  // 查询方法：Tauri command（按需 invoke）/ 事件数据
  // ==================================================================

  getVersion () {
    // get_engine_info 返回 { version, enabledFeatures }（与 aria2.getVersion 对齐）
    return invoke('get_engine_info')
  }

  changeGlobalOption (options) {
    const args = formatOptionsForEngine(options)

    return invoke('change_global_option', { options: args })
  }

  getGlobalOption () {
    // get_global_option 返回 kebab 键基础子集，转换回驼峰（与旧 JSON-RPC 行为一致）
    return invoke('get_global_option')
      .then((data) => {
        return changeKeysToCamelCase(data)
      })
  }

  getOption (params = {}) {
    // 无单任务选项 command：以全局选项近似（含 dir / split / header，
    // 供 Task/Index.vue handleRestartTask 重建任务选项）
    return invoke('get_global_option')
      .then((data) => {
        return changeKeysToCamelCase(data)
      })
  }

  updateActiveTaskOption (options) {
    this.fetchTaskList({ type: 'active' })
      .then((data) => {
        if (isEmpty(data)) {
          return
        }

        const gids = data.map((task) => task.gid)
        this.batchChangeOption({ gids, options })
      })
  }

  changeOption (params = {}) {
    const { gid, options = {} } = params

    const engineOptions = formatOptionsForEngine(options)

    return invoke('change_option', { gid, options: engineOptions })
  }

  getGlobalStat () {
    // get_global_stat 返回字符串数值（aria2 惯例），保持旧形状由调用方 Number() 转换
    return invoke('get_global_stat')
  }

  // —— 任务列表：按需 invoke('get_tasks') 后客户端按 status 过滤（分页切片） ——

  // 获取全部在册任务（get_tasks 返回 aria2 兼容 JSON 数组；不含 removed 历史）
  fetchAllTasks () {
    return invoke('get_tasks')
      .then((tasks) => {
        return Array.isArray(tasks) ? tasks : []
      })
  }

  // offset/num 分页切片（与旧 tellWaiting/tellStopped 的 offset/num 语义一致）
  sliceTaskList (tasks, offset = 0, num = 20) {
    return tasks.slice(offset, offset + num)
  }

  fetchDownloadingTaskList (params = {}) {
    // 下载中视图 = active + waiting（与旧 tellActive + tellWaiting 合并一致）
    const { offset = 0, num = 20 } = params
    return this.fetchAllTasks()
      .then((tasks) => {
        const filtered = tasks.filter((task) => {
          return task.status === 'active' || task.status === 'waiting'
        })
        return this.sliceTaskList(filtered, offset, num)
      })
  }

  fetchWaitingTaskList (params = {}) {
    const { offset = 0, num = 20 } = params
    return this.fetchAllTasks()
      .then((tasks) => {
        const filtered = tasks.filter((task) => task.status === 'waiting')
        return this.sliceTaskList(filtered, offset, num)
      })
  }

  fetchStoppedTaskList (params = {}) {
    const { offset = 0, num = 20 } = params
    return this.fetchAllTasks()
      .then((tasks) => {
        const filtered = tasks.filter((task) => {
          return ['complete', 'error', 'removed'].includes(task.status)
        })
        return this.sliceTaskList(filtered, offset, num)
      })
  }

  fetchActiveTaskList (params = {}) {
    // 供 app/fetchProgress 计算全局进度使用（基于当前仓库数据，非轮询）
    return this.fetchAllTasks()
      .then((tasks) => {
        return tasks.filter((task) => task.status === 'active')
      })
  }

  fetchTaskList (params = {}) {
    const { type } = params
    switch (type) {
    case 'active':
      return this.fetchDownloadingTaskList(params)
    case 'waiting':
      return this.fetchWaitingTaskList(params)
    case 'stopped':
      return this.fetchStoppedTaskList(params)
    default:
      return this.fetchDownloadingTaskList(params)
    }
  }

  // —— 任务详情：按需 invoke（详情面板打开时才调用，见 MIGRATION-TAURI.md 5.8） ——

  fetchTaskItem (params = {}) {
    const { gid } = params
    return invoke('get_task_detail', { gid })
  }

  fetchTaskItemWithPeers (params = {}) {
    const { gid } = params
    // BT peers 属 Phase 3：详情走 get_task_detail，peers 占位空数组
    return invoke('get_task_detail', { gid })
      .then((result) => {
        const task = result || {}
        task.peers = []
        return task
      })
  }

  fetchTaskItemPeers (params = {}) {
    // BT peers 属 Phase 3：返回空数组占位（保持旧形状，视觉组件不感知）
    return Promise.resolve([])
  }

  // ==================================================================
  // 操作类方法：Tauri command（invoke）
  // ==================================================================

  addUri (params) {
    const {
      uris,
      outs,
      options
    } = params
    // 每个 URL 单独 invoke（Rust 端 add_uri 对每个 URL 创建独立任务）；
    // outs 存在时把对应文件名写入该 URL 的 options.out（与旧 multicall 行为一致）
    const tasks = uris.map((uri, index) => {
      const engineOptions = formatOptionsForEngine(options)
      if (outs && outs[index]) {
        engineOptions.out = outs[index]
      }
      return invoke('add_uri', { uris: [uri], options: engineOptions })
    })
    return Promise.all(tasks).then((results) => {
      // 返回 gid 数组（各 URL 的 gid 按序扁平合并，保持旧返回形状）
      return [].concat(...results)
    })
  }

  addTorrent (params) {
    const {
      torrent,
      options
    } = params
    const engineOptions = formatOptionsForEngine(options)
    // add_torrent 在 Phase 2 返回明确占位错误（BT 属 Phase 3），前端已有处理
    return invoke('add_torrent', { torrent, options: engineOptions })
  }

  addMetalink (params) {
    // addMetalink 前端无触发入口（commands.js 中为 TODO），返回明确占位错误
    return Promise.reject(new Error('addMetalink 未支持（Phase 3 提供）'))
  }

  pauseTask (params = {}) {
    const { gid } = params
    return invoke('pause_task', { gid })
  }

  pauseAllTask (params = {}) {
    // aria2.pauseAll：暂停全部下载中/排队任务（无批量 command，逐个 invoke）
    return this.fetchDownloadingTaskList()
      .then((tasks) => {
        return Promise.all(tasks.map((task) => this.pauseTask({ gid: task.gid })))
      })
  }

  forcePauseTask (params = {}) {
    const { gid } = params
    // KGet abort 本身就是强停，普通/强制暂停行为一致（pause_task 无 force 参数）
    return invoke('pause_task', { gid })
  }

  forcePauseAllTask (params = {}) {
    return this.fetchDownloadingTaskList()
      .then((tasks) => {
        return Promise.all(tasks.map((task) => this.forcePauseTask({ gid: task.gid })))
      })
  }

  resumeTask (params = {}) {
    const { gid } = params
    return invoke('resume_task', { gid })
  }

  resumeAllTask (params = {}) {
    // aria2.unpauseAll：恢复全部暂停/排队任务（无批量 command，逐个 invoke）
    return this.fetchAllTasks()
      .then((tasks) => {
        const resumed = tasks.filter((task) => {
          return task.status === 'paused' || task.status === 'waiting'
        })
        return Promise.all(resumed.map((task) => this.resumeTask({ gid: task.gid })))
      })
  }

  removeTask (params = {}) {
    const { gid } = params
    return invoke('remove_task', { gid })
  }

  forceRemoveTask (params = {}) {
    const { gid } = params
    return invoke('remove_task', { gid, force: true })
  }

  saveSession (params = {}) {
    return invoke('save_session')
  }

  purgeTaskRecord (params = {}) {
    return invoke('purge')
  }

  removeTaskRecord (params = {}) {
    const { gid } = params
    // 无独立 removeDownloadResult command：对在册任务等价 remove_task
    // （终态 complete/error 任务可移除进入 stopped 历史）
    return invoke('remove_task', { gid })
  }

  // —— 批量方法：循环调用单任务 command（优先循环，减少 Rust 批量改动） ——

  multicall (method, params = {}) {
    let { gids, options = {} } = params
    options = formatOptionsForEngine(options)

    const caller = BATCH_METHOD_MAP[method]
    if (!caller) {
      return Promise.reject(new Error(`未支持的批量方法: ${method}`))
    }
    return Promise.all(gids.map((gid) => caller(gid, options)))
  }

  batchChangeOption (params = {}) {
    return this.multicall('aria2.changeOption', params)
  }

  batchRemoveTask (params = {}) {
    return this.multicall('aria2.remove', params)
  }

  batchResumeTask (params = {}) {
    return this.multicall('aria2.unpause', params)
  }

  batchPauseTask (params = {}) {
    return this.multicall('aria2.pause', params)
  }

  batchForcePauseTask (params = {}) {
    return this.multicall('aria2.forcePause', params)
  }
}
