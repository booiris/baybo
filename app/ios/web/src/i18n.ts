import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import en from "./locales/en";
import zh from "./locales/zh";

// Language is owned by native (init.language / setLanguage over the bridge) —
// no browser detector, and no localStorage (the bundle may run under file://).
void i18n.use(initReactI18next).init({
  resources: { en, zh },
  lng: "en",
  fallbackLng: "en",
  supportedLngs: ["en", "zh"],
  // Match only the base language so zh-CN / zh-Hans / en-US resolve to zh / en.
  load: "languageOnly",
  interpolation: { escapeValue: false },
});

export default i18n;
