// engine:* 事件订阅：把 Rust 端广播的引擎状态映射为 Vuex mutation
//
// 对应 MIGRATION-TAURI.md 5.8 / 8.2 节的事件通道设计（主进程 → 渲染进程）：
//   engine:global-stat → app/UPDATE_GLOBAL_STAT（全局统计，托盘/标题速度用）
//   engine:snapshot     → task/MERGE_TASKS（按 gid 增量合并，替代高频轮询全量覆盖）
//   engine:task-event   → onTaskEvent(cb)（onDownloadStart/Stop/Pause/Complete/Error 对应事件）
// 数值字段按 aria2 惯例由 Rust 端以字符串输出，此处统一 Number() 转换
// （与 app.js fetchGlobalStat 的转换行为保持一致）。
import { listen } from '@tauri-apps/api/event'

// 快照任务中需要转为 Number 的数值字段（aria2 字符串惯例字段）
const NUMERIC_FIELDS = [
  'totalLength',
  'completedLength',
  'uploadLength',
  'downloadSpeed',
  'uploadSpeed',
  'connections',
  'numSeeders'
]

// 视图 → 允许的任务状态集合（与 Api.js fetchTaskList 的 status 分组语义一致）
const TASK_VIEW_STATUS = {
  active: ['active', 'waiting'],
  waiting: ['waiting'],
  stopped: ['complete', 'error', 'removed']
}

// 把全局统计对象中的字符串数值转成 Number（与 app.js fetchGlobalStat 一致）
const normalizeStat = (data) => {
  const stat = {}
  Object.keys(data).forEach((key) => {
    stat[key] = Number(data[key])
  })
  return stat
}

// 把单个任务的数值字段转成 Number，其余字段原样保留（供组件直接消费）
const normalizeTask = (task) => {
  const normalized = { ...task }
  NUMERIC_FIELDS.forEach((key) => {
    if (normalized[key] !== undefined && normalized[key] !== null) {
      normalized[key] = Number(normalized[key])
    }
  })
  return normalized
}

// 按当前视图过滤快照任务：增量合并不破坏 currentList 视图语义
const filterTasksByView = (tasks, currentList) => {
  const statuses = TASK_VIEW_STATUS[currentList] || TASK_VIEW_STATUS.active
  return tasks.filter((task) => statuses.includes(task.status))
}

// —— engine:task-event 订阅者列表（onTaskEvent 注册 / 取消） ——
const taskEventHandlers = []

// 订阅 engine:task-event 并回调 { gid, event }（event 取值：
// start/stop/pause/complete/error/bt-complete，对应原 aria2 通知语义）。
// 返回取消订阅函数（组件 destroyed 时调用）。
export function onTaskEvent (cb) {
  taskEventHandlers.push(cb)
  return () => {
    const idx = taskEventHandlers.indexOf(cb)
    if (idx !== -1) {
      taskEventHandlers.splice(idx, 1)
    }
  }
}

// 订阅 Rust 端 engine:* 事件并驱动 Vuex
// 订阅失败仅 console.warn，不阻塞应用启动与其余功能
export function initEngineEvents (store) {
  // 全局统计：数值字符串 → Number 后写入 app.stat
  listen('engine:global-stat', (event) => {
    const payload = event.payload || {}
    store.commit('app/UPDATE_GLOBAL_STAT', normalizeStat(payload))
  }).catch((err) => {
    console.warn('[Motrix] 订阅 engine:global-stat 失败:', err)
  })

  // 任务快照：按当前视图过滤后经 MERGE_TASKS 增量合并（以 gid 为键，
  // 仅替换/新增/删除，不做全量数组替换，减少 Vue 重渲染 —— Task 11.4）
  listen('engine:snapshot', (event) => {
    const payload = event.payload || {}
    const tasks = Array.isArray(payload.tasks) ? payload.tasks : []
    const { currentList, taskDetailVisible, currentTaskGid } = store.state.task
    const viewTasks = filterTasksByView(tasks, currentList)
    store.commit('task/MERGE_TASKS', viewTasks.map(normalizeTask))

    // 详情面板打开且当前任务在快照中：同步更新 currentTaskItem，
    // 保证详情进度/状态与列表一致（peers 保持不变，BT Phase 3 再补充）
    if (taskDetailVisible && currentTaskGid) {
      const current = tasks.find((task) => task.gid === currentTaskGid)
      if (current) {
        const normalized = normalizeTask(current)
        store.commit('task/UPDATE_CURRENT_TASK_ITEM', normalized)
        store.commit('task/UPDATE_CURRENT_TASK_FILES', normalized.files || [])
      }
    }
  }).catch((err) => {
    console.warn('[Motrix] 订阅 engine:snapshot 失败:', err)
  })

  // 任务状态变化事件：转发给 onTaskEvent 注册的回调（EngineClient.vue 分发到 toast/通知）
  listen('engine:task-event', (event) => {
    const payload = event.payload || {}
    if (!payload.gid || !payload.event) {
      return
    }
    const data = { gid: payload.gid, event: payload.event }
    taskEventHandlers.forEach((cb) => {
      cb(data)
    })
  }).catch((err) => {
    console.warn('[Motrix] 订阅 engine:task-event 失败:', err)
  })
}

export default initEngineEvents
