// English strings (the source locale; `zh.ts` mirrors this shape). Keys are
// grouped by screen. Interpolated values use the i18next `{{name}}` syntax.
export const en = {
  translation: {
    landing: {
      subtitle: "Connect to your Baybo",
      scan: "Scan QR",
      direct: "Connect with URL & token",
    },
    direct: {
      hint: "Use your Baybo's address and access token.",
      urlLabel: "Baybo address",
      tokenLabel: "Access token",
      tokenPlaceholder: "Paste the access token",
      tokenHint: "Get this from the computer running Baybo.",
      connect: "Connect",
      connecting: "Connecting…",
      invalidUrl: "Enter a valid http:// or https:// address.",
      invalidToken: "Invalid token — check it and try again.",
      failed: "Connection failed: {{error}}",
      back: "Back",
    },
    scan: {
      cancel: "Cancel",
      permissionOff: "Camera access is off — enable it for Baybo in Settings, then try again.",
      notBaybo: "That QR isn't a Baybo pairing QR. Scan the one shown by `baybo device pair`.",
      failed: "Scan failed: {{error}}",
    },
    pair: {
      connecting: "Connecting…",
      confirmTitle: "Confirm pairing",
      confirmHint: "Check this code matches the one shown on the computer running `baybo device pair`, then pair on both.",
      pair: "Pair",
      cancel: "Cancel",
      confirming: "Confirming…",
      cancelling: "Cancelling…",
      cancelled: "Pairing cancelled.",
      cancelledReason: "Pairing cancelled: {{reason}}",
      failed: "Pairing failed: {{error}}",
    },
    connected: {
      logout: "Log out",
      logoutConfirm: "Log out of this Baybo? Notifications and chat stop until you connect again.",
    },
    chat: {
      title: "Chat",
      placeholder: "Message…",
      send: "Send",
      connecting: "Connecting…",
      connectFailed: "Connect failed: {{error}}",
      startFailed: "Couldn't start the chat: {{error}}",
      sendFailed: "Send failed: {{error}}",
      recoverFailed: "Couldn't reload history: {{error}}",
      // Scroll-up pagination affordance for older transcript pages.
      loadOlder: "Load earlier messages",
      waitingUpload: "Waiting for the image to finish uploading…",
      loadingImage: "Loading image…",
      tapToLoad: "Tap to load image",
      tooLarge: "Too large (max 100 MB)",
      addImage: "Add image",
      remove: "Remove",
      imageAlt: "image",
    },
    lang: {
      label: "Language",
    },
  },
};

export default en;
