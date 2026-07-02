// 简体中文 (Simplified Chinese) — mirrors the shape of `en.ts`.
import type en from "./en";

export const zh: typeof en = {
  translation: {
    landing: {
      subtitle: "连接到你的 Baybo",
      scan: "扫码连接",
      direct: "使用 URL 与令牌连接",
    },
    direct: {
      hint: "输入 Baybo 的地址和访问令牌。",
      urlLabel: "Baybo 地址",
      tokenLabel: "访问令牌",
      tokenPlaceholder: "粘贴访问令牌",
      tokenHint: "在运行 Baybo 的电脑上获取。",
      connect: "连接",
      connecting: "连接中…",
      invalidUrl: "请输入有效的 http:// 或 https:// 地址。",
      invalidToken: "令牌无效——请检查后重试。",
      failed: "连接失败：{{error}}",
      back: "返回",
    },
    scan: {
      cancel: "取消",
      permissionOff: "相机权限已关闭——请在「设置」中为 Baybo 开启后重试。",
      notBaybo: "这不是 Baybo 的配对二维码。请扫描 `baybo device pair` 显示的那个。",
      failed: "扫描失败：{{error}}",
    },
    pair: {
      connecting: "连接中…",
      confirmTitle: "确认配对",
      confirmHint: "请核对此代码与运行 `baybo device pair` 的电脑上显示的一致，然后在两端确认配对。",
      pair: "配对",
      cancel: "取消",
      confirming: "确认中…",
      cancelling: "取消中…",
      cancelled: "配对已取消。",
      cancelledReason: "配对已取消：{{reason}}",
      failed: "配对失败：{{error}}",
    },
    connected: {
      logout: "退出登录",
      logoutConfirm: "退出此 Baybo？在重新连接前，通知和聊天将停止。",
    },
    chat: {
      placeholder: "输入消息…",
      send: "发送",
      voice: "语音输入",
      connecting: "连接中…",
      connected: "已连接",
      offline: "离线",
      startFailed: "无法开始聊天：{{error}}",
      sendFailed: "发送失败：{{error}}",
      recoverFailed: "无法重新加载历史记录：{{error}}",
      loadOlder: "加载更早的消息",
      jumpToLatest: "跳到最新消息",
      waitingUpload: "正在等待图片上传完成…",
      loadingImage: "图片加载中…",
      tapToLoad: "点按加载图片",
      tooLarge: "文件过大（最大 100 MB）",
      addImage: "添加图片",
      remove: "移除",
      imageAlt: "图片",
    },
    lang: {
      label: "语言",
    },
  },
};

export default zh;
