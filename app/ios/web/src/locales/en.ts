// English strings (the source locale; `zh.ts` mirrors this shape). Only the
// web bundle's own strings live here (transcript + deck shell) — all other
// chrome is native SwiftUI. The deck entry imports this file as plain data;
// the i18next runtime stays out of that React-free chunk.
export const en = {
  translation: {
    chat: {
      loadOlder: "Load earlier messages",
      preCompaction: "Compacted",
      loadingThread: "Loading conversation…",
      loadingImage: "Loading image…",
      tapToLoad: "Tap to load image",
      imageAlt: "image",
      viewImage: "View image",
      audioPlay: "Play audio",
      audioPause: "Pause audio",
      videoPlay: "Play video",
      videoDownload: "Download video",
      recoverFailed: "Couldn't reload history: {{error}}",
      retrySend: "Send failed — tap to retry",
      copied: "Copied",
      stopped: "Stopped",
      working: "Working",
      worked: "Worked",
      approvalWaiting: "waiting for approval",
      approvalApproved: "approved",
      approvalApprovedAlways: "always approved",
      approvalDenied: "denied",
      workedFor: "Worked {{dur}}",
      durS: "{{s}}s",
      durM: "{{m}}m",
      durMS: "{{m}}m {{s}}s",
      durH: "{{h}}h",
      durHM: "{{h}}h {{m}}m",
    },
    deck: {
      empty: "No cards yet — type /deck in a chat to make one.",
      quickSetup: "Create a card",
      createCardInflight: "Creating · view",
      quickSetupPrompt: "/deck Make a card that monitors baybo's token usage",
      quarantined: "This card was stopped after repeated failures.",
      disabled: "Paused",
      reenable: "Re-enable",
    },
  },
};

export default en;
