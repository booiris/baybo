import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import LanguageDetector from "i18next-browser-languagedetector";
import en from "./locales/en";
import zh from "./locales/zh";

// Languages the UI ships with. `short` is the compact label shown in the
// landing's language switcher; `label` is the full name.
export const SUPPORTED_LANGUAGES = [
  { code: "en", label: "English", short: "EN" },
  { code: "zh", label: "简体中文", short: "中" },
] as const;

export type LangCode = (typeof SUPPORTED_LANGUAGES)[number]["code"];

void i18n
  .use(LanguageDetector)
  .use(initReactI18next)
  .init({
    resources: { en, zh },
    fallbackLng: "en",
    supportedLngs: SUPPORTED_LANGUAGES.map((l) => l.code),
    // Match only the base language so zh-CN / zh-TW / en-US resolve to zh / en.
    load: "languageOnly",
    detection: {
      order: ["localStorage", "navigator"],
      caches: ["localStorage"],
      lookupLocalStorage: "baybo.lang",
    },
    interpolation: { escapeValue: false },
  });

export default i18n;
