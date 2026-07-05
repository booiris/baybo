// 简体中文 (Simplified Chinese) — mirrors the shape of `en.ts`.
import type en from "./en";

export const zh: typeof en = {
  translation: {
    chat: {
      loadOlder: "加载更早的消息",
      loadingImage: "图片加载中…",
      tapToLoad: "点按加载图片",
      imageAlt: "图片",
      recoverFailed: "无法重新加载历史记录：{{error}}",
      working: "思考中",
      worked: "思考了片刻",
      workedFor: "思考了 {{dur}}",
      durS: "{{s}} 秒",
      durM: "{{m}} 分钟",
      durMS: "{{m}} 分 {{s}} 秒",
      durH: "{{h}} 小时",
      durHM: "{{h}} 小时 {{m}} 分",
    },
  },
};

export default zh;
