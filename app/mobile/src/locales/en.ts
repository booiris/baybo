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
      title: "Connected",
      ready: "Paired and ready.",
      remembered: "Paired (remembered from a previous session).",
      directReady: "Connected directly to {{url}}.",
      rendezvous: "Rendezvous",
      relayNode: "Relay node",
      openChat: "Open chat",
      replace: "Replace pairing",
      forget: "Forget",
      forgetConfirm: "Forget this Baybo? Notifications and chat stop until you connect again.",
      replaceConfirm:
        "Reconnect to a different Baybo? You'll scan a new one; the current connection stays until the new one finishes.",
      forgetFailed: "Couldn't forget the pairing: {{error}}",
      disconnect: "Disconnect",
    },
    chat: {
      back: "Back",
      title: "Chat",
      placeholder: "Message…",
      send: "Send",
      connecting: "Connecting…",
      connectFailed: "Connect failed: {{error}}",
      sendFailed: "Send failed: {{error}}",
      recoverFailed: "Couldn't reload history: {{error}}",
      // Relay reset: no REST backfill, so live-only with the gap marked.
      historyTruncated: "Earlier messages unavailable — showing live messages.",
      // Direct reset: recovered the newest page, but older history exists and
      // mobile has no scroll-up paging yet.
      olderUnavailable: "Earlier messages aren't shown here.",
      // REST history reports only that a row had media, not its blob refs.
      attachmentPlaceholder: "[attachment]",
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
