<template>
  <div v-if="false"></div>
</template>

<script>
  import is from '@shims'
  import { mapState } from 'vuex'
  import api from '@/api'
  import { onTaskEvent } from '@shims/events'
  import {
    getTaskFullPath,
    showItemInFolder
  } from '@/utils/native'
  import { checkTaskIsBT, getTaskName } from '@shared/utils'

  export default {
    name: 'mo-engine-client',
    computed: {
      isRenderer: () => is.renderer(),
      ...mapState('app', {
        uploadSpeed: state => state.stat.uploadSpeed,
        downloadSpeed: state => state.stat.downloadSpeed,
        speed: state => state.stat.uploadSpeed + state.stat.downloadSpeed,
        downloading: state => state.stat.numActive > 0,
        progress: state => state.progress
      }),
      ...mapState('task', {
        messages: state => state.messages,
        seedingList: state => state.seedingList,
        taskDetailVisible: state => state.taskDetailVisible,
        enabledFetchPeers: state => state.enabledFetchPeers,
        currentTaskGid: state => state.currentTaskGid,
        currentTaskItem: state => state.currentTaskItem
      }),
      ...mapState('preference', {
        taskNotification: state => state.config.taskNotification
      }),
      currentTaskIsBT () {
        return checkTaskIsBT(this.currentTaskItem)
      }
    },
    watch: {
      speed (val) {
        const { uploadSpeed, downloadSpeed } = this
        this.$electron.ipcRenderer.send('event', 'speed-change', {
          uploadSpeed,
          downloadSpeed
        })
      },
      downloading (val, oldVal) {
        if (val !== oldVal && this.isRenderer) {
          this.$electron.ipcRenderer.send('event', 'download-status-change', val)
        }
      },
      progress (val) {
        this.$electron.ipcRenderer.send('event', 'progress-change', val)
      }
    },
    methods: {
      async fetchTaskItem ({ gid }) {
        return api.fetchTaskItem({ gid })
          .catch((e) => {
            console.warn(`fetchTaskItem fail: ${e.message}`)
          })
      },
      // Task 11：engine:task-event 事件分发（{ gid, event } → 对应通知处理方法）。
      // event 取值 start/stop/pause/complete/error/bt-complete，与原 aria2 通知语义一致。
      dispatchTaskEvent ({ gid, event }) {
        // 兼容原处理方法的签名（接收 [{ gid }] 数组）
        const payload = [{ gid }]
        switch (event) {
        case 'start':
          this.onDownloadStart(payload)
          break
        case 'pause':
          this.onDownloadPause(payload)
          break
        case 'stop':
          this.onDownloadStop(payload)
          break
        case 'complete':
          this.onDownloadComplete(payload)
          break
        case 'error':
          this.onDownloadError(payload)
          break
        case 'bt-complete':
          this.onBtDownloadComplete(payload)
          break
        default:
          console.warn(`[Motrix] 未识别的任务事件: ${event}`)
        }
      },
      onDownloadStart (event) {
        this.$store.dispatch('task/fetchList')
        this.$store.dispatch('app/resetInterval')
        this.$store.dispatch('task/saveSession')
        const [{ gid }] = event
        const { seedingList } = this
        if (seedingList.includes(gid)) {
          return
        }

        this.fetchTaskItem({ gid })
          .then((task) => {
            if (!task) {
              return
            }
            const { dir } = task
            this.$store.dispatch('preference/recordHistoryDirectory', dir)
            const taskName = getTaskName(task)
            const message = this.$t('task.download-start-message', { taskName })
            this.$msg.info(message)
          })
      },
      onDownloadPause (event) {
        const [{ gid }] = event
        const { seedingList } = this
        if (seedingList.includes(gid)) {
          return
        }

        this.fetchTaskItem({ gid })
          .then((task) => {
            if (!task) {
              return
            }
            const taskName = getTaskName(task)
            const message = this.$t('task.download-pause-message', { taskName })
            this.$msg.info(message)
          })
      },
      onDownloadStop (event) {
        const [{ gid }] = event
        this.fetchTaskItem({ gid })
          .then((task) => {
            if (!task) {
              return
            }
            const taskName = getTaskName(task)
            const message = this.$t('task.download-stop-message', { taskName })
            this.$msg.info(message)
          })
      },
      onDownloadError (event) {
        const [{ gid }] = event
        this.fetchTaskItem({ gid })
          .then((task) => {
            if (!task) {
              return
            }
            const taskName = getTaskName(task)
            const { errorCode, errorMessage } = task
            console.error(`[Motrix] download error gid: ${gid}, #${errorCode}, ${errorMessage}`)
            const message = this.$t('task.download-error-message', { taskName })
            const link = `<a target="_blank" href="https://github.com/agalwood/Motrix/wiki/Error#${errorCode}" rel="noopener noreferrer">${errorCode}</a>`
            this.$msg({
              type: 'error',
              showClose: true,
              duration: 5000,
              dangerouslyUseHTMLString: true,
              message: `${message} ${link}`
            })
          })
      },
      onDownloadComplete (event) {
        this.$store.dispatch('task/fetchList')
        const [{ gid }] = event
        this.$store.dispatch('task/removeFromSeedingList', gid)

        this.fetchTaskItem({ gid })
          .then((task) => {
            this.handleDownloadComplete(task, false)
          })
      },
      onBtDownloadComplete (event) {
        this.$store.dispatch('task/fetchList')
        const [{ gid }] = event
        const { seedingList } = this
        if (seedingList.includes(gid)) {
          return
        }

        this.$store.dispatch('task/addToSeedingList', gid)

        this.fetchTaskItem({ gid })
          .then((task) => {
            this.handleDownloadComplete(task, true)
          })
      },
      handleDownloadComplete (task, isBT) {
        if (!task) {
          return
        }
        this.$store.dispatch('task/saveSession')

        const path = getTaskFullPath(task)
        this.showTaskCompleteNotify(task, isBT, path)
        this.$electron.ipcRenderer.send('event', 'task-download-complete', task, path)
      },
      showTaskCompleteNotify (task, isBT, path) {
        const taskName = getTaskName(task)
        const message = isBT
          ? this.$t('task.bt-download-complete-message', { taskName })
          : this.$t('task.download-complete-message', { taskName })
        const tips = isBT
          ? '\n' + this.$t('task.bt-download-complete-tips')
          : ''

        this.$msg.success(`${message}${tips}`)

        if (!this.taskNotification) {
          return
        }

        const notifyMessage = isBT
          ? this.$t('task.bt-download-complete-notify')
          : this.$t('task.download-complete-notify')

        /* eslint-disable no-new */
        const notify = new Notification(notifyMessage, {
          body: `${taskName}${tips}`
        })
        notify.onclick = () => {
          showItemInFolder(path, {
            errorMsg: this.$t('task.file-not-exist')
          })
        }
      },
      showTaskErrorNotify (task) {
        const taskName = getTaskName(task)

        const message = this.$t('task.download-fail-message', { taskName })
        this.$msg.success(message)

        if (!this.taskNotification) {
          return
        }

        /* eslint-disable no-new */
        new Notification(this.$t('task.download-fail-notify'), {
          body: taskName
        })
      }
    },
    created () {
      // Task 11：订阅 engine:task-event（Rust 端经 TaskManager 事件通道转发），
      // 分发到 onDownloadStart / onDownloadPause / onDownloadStop / onDownloadComplete /
      // onDownloadError / onBtDownloadComplete 等通知处理方法（替代原 aria2 WS 通知绑定）
      this.unlistenTaskEvent = onTaskEvent((data) => {
        this.dispatchTaskEvent(data)
      })
    },
    mounted () {
      setTimeout(() => {
        this.$store.dispatch('app/fetchEngineInfo')
        this.$store.dispatch('app/fetchEngineOptions')
        // 状态同步已由 engine:* 事件驱动（见 src/shims/events.js），不再轮询
      }, 100)
    },
    destroyed () {
      this.$store.dispatch('task/saveSession')

      if (this.unlistenTaskEvent) {
        this.unlistenTaskEvent()
        this.unlistenTaskEvent = null
      }
    }
  }
</script>
