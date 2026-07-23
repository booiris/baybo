// 简体中文 (Simplified Chinese) — mirrors the shape of `en.ts`.
import type en from "./en";

export const zh: typeof en = {
  translation: {
    chat: {
      loadOlder: "加载更早的消息",
      preCompaction: "已压缩",
      loadingThread: "对话加载中…",
      loadingImage: "图片加载中…",
      tapToLoad: "点按加载图片",
      imageAlt: "图片",
      viewImage: "查看图片",
      audioPlay: "播放音频",
      audioPause: "暂停音频",
      videoPlay: "播放视频",
      videoDownload: "下载视频",
      recoverFailed: "无法重新加载历史记录：{{error}}",
      retrySend: "发送失败，点按重试",
      copied: "已复制",
      stopped: "已停止",
      working: "思考中",
      worked: "思考了片刻",
      approvalWaiting: "等待审批",
      approvalApproved: "已批准",
      approvalApprovedAlways: "始终批准",
      approvalDenied: "已拒绝",
      workedFor: "思考了 {{dur}}",
      durS: "{{s}} 秒",
      durM: "{{m}} 分钟",
      durMS: "{{m}} 分 {{s}} 秒",
      durH: "{{h}} 小时",
      durHM: "{{h}} 小时 {{m}} 分",
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
