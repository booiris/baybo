// 简体中文 (Simplified Chinese) — mirrors the shape of `en.ts`.
import type en from "./en";

export const zh: typeof en = {
  translation: {
    chat: {
      loadOlder: "加载更早的消息",
      preCompaction: "已压缩",
      compacting: "正在压缩上下文…",
      compacted: "上下文已压缩",
      loadingThread: "对话加载中…",
      htmlPreview: "HTML 预览",
      loadingHtmlPreview: "正在加载预览…",
      invalidHtmlPreview: "HTML 预览的 blob id 无效",
      reloadHtmlPreview: "重新加载 HTML 预览",
      expandHtmlPreview: "全屏显示 HTML 预览",
      closeHtmlPreview: "关闭全屏 HTML 预览",
      htmlPreviewFrameTitle: "Agent 创建的 HTML 预览",
      loadingImage: "图片加载中…",
      tapToLoad: "点按加载图片",
      imageAlt: "图片",
      viewImage: "查看图片",
      audioPlay: "播放音频",
      audioPause: "暂停音频",
      videoPlay: "播放视频",
      videoDownload: "下载视频",
      recoverFailed: "无法重新加载历史记录：{{error}}",
      jumpNotFound: "没能定位到那条消息 —— 它比这个会话已加载的部分更靠前。",
      retrySend: "发送失败，点按重试",
      copied: "已复制",
      stopped: "已停止",
      working: "处理中",
      // NOT "处理了片刻": this is the label whenever the duration is UNKNOWN
      // (a mirror-restored turn with remarks in it, a gateway that sent no step
      // stamps), not only when it was genuinely brief — and "片刻" over a
      // five-minute turn is a claim the reader cannot check.
      worked: "处理了",
      approvalWaiting: "等待审批",
      approvalApproved: "已批准",
      approvalApprovedAlways: "始终批准",
      approvalDenied: "已拒绝",
      workedFor: "处理了 {{dur}}",
      cancelled: "已取消",
      cancelledFor: "已取消 · 处理了 {{dur}}",
      durS: "{{s}} 秒",
      durM: "{{m}} 分钟",
      durMS: "{{m}} 分 {{s}} 秒",
      durH: "{{h}} 小时",
      durHM: "{{h}} 小时 {{m}} 分",
    },
    issue: {
      loading: "正在加载卡片…",
      description: "描述",
      noDescription: "暂无描述。",
      attachments: "附件",
      subIssues: "子任务",
      activity: "动态",
      openedBy: "由 {{who}} 创建于",
      unreadFrom: "新动态",
      openRun: "查看运行",
      unassigned: "未指派",
      blocked: "已阻塞",
      system: "看板",
      you: "你",
      // Chinese has one plural form, so i18next only ever selects `_other`
      // here. `_one` is carried anyway because the parity test compares key
      // SETS — and a per-key exemption would be a hole in the check that
      // exists to catch the key somebody forgot.
      nEvents_one: "{{count}} 条记录",
      nEvents_other: "{{count}} 条记录",
      eventOpened: "创建了这张卡片",
      eventMoved: "把它从 {{from}} 移到了 {{to}}",
      eventRunStarted: "开始了第 {{attempt}} 次运行",
      eventRunSettled: "第 {{attempt}} 次运行 {{status}}",
      eventCancelled: "取消了这张卡片",
      eventMerged: "把 {{branch}} 合并进了 {{into}}",
      status: {
        backlog: "待办池",
        todo: "待办",
        in_progress: "进行中",
        review: "待评审",
        done: "已完成",
      },
      priority: {
        urgent: "紧急",
        high: "高",
        medium: "中",
        low: "低",
        none: "无优先级",
      },
      run: {
        queued: "排队中",
        held: "已暂停",
        running: "进行中",
        done: "已完成",
        failed: "失败",
        cancelled: "已取消",
      },
    },
    deck: {
      empty: "还没有卡片 — 在聊天里输入 /deck 创建一张。",
      quickSetup: "创建卡片",
      createCardInflight: "创建中 · 查看",
      quickSetupPrompt: "/deck 帮我做一个监控 baybo token 使用量的卡片",
      quarantined: "此卡片因反复失败已被停用。",
      disabled: "已暂停",
      reenable: "重新启用",
    },
  },
};

export default zh;
