/** Lark CardKit v1 builders for the OAuth UAT auth flow.
 *
 * Three states tracked by mutating one card via `channel.updateCard`:
 *   - pending  → orange header, "Authorize" button + manual fallback
 *   - authorized → green header, "you're signed in" copy
 *   - failed → red header, terminal reason ("denied", "expired", …)
 *
 * v1 (not v2 streaming) because:
 *   1. The card is short-lived and edited at most a handful of times —
 *      streaming buys nothing.
 *   2. Some tenants haven't enabled CardKit v2; v1 stays universal. */

export interface AuthPendingCardOpts {
  /** What the agent was trying to do — drives the explanatory line.
   * E.g. "read your Feishu calendar" or just the tool name. */
  reason: string;
  /** Lark's hosted verification page with `user_code` pre-filled. */
  verificationUriComplete: string;
  /** Lark's hosted verification page (manual code entry). */
  verificationUri: string;
  /** Short code the user types at `verificationUri` if the deep
   * link doesn't open (mobile fallback, copy-paste UX). */
  userCode: string;
  /** Unix-millis when the device_code expires. Rendered as countdown
   * in the card body so the user knows they have a few minutes. */
  expiresAtMs: number;
}

export function buildAuthPendingCard(opts: AuthPendingCardOpts): object {
  const minutesLeft = Math.max(
    1,
    Math.ceil((opts.expiresAtMs - Date.now()) / 60_000),
  );
  const lines = [
    `Aura needs your authorization to ${opts.reason}.`,
    "",
    `Granting will let Aura **read and write** the Feishu surfaces it has scope for (calendar, docs, bitable, etc.) on your behalf for the lifetime of the token. Destructive writes (delete, overwrite) require an extra in-chat confirmation per call.`,
    "",
    `Tap **Authorize** to open the Feishu sign-in page (or visit ${opts.verificationUri} and enter the code below). This page expires in ~${minutesLeft} minute${minutesLeft === 1 ? "" : "s"}.`,
    "",
    `**Code:** \`${opts.userCode}\``,
  ];
  return {
    config: { wide_screen_mode: true },
    header: {
      template: "orange",
      title: { tag: "plain_text", content: "🔐 Authorization required" },
    },
    elements: [
      { tag: "div", text: { tag: "lark_md", content: lines.join("\n") } },
      {
        tag: "action",
        actions: [
          {
            tag: "button",
            text: { tag: "plain_text", content: "Authorize" },
            type: "primary",
            url: opts.verificationUriComplete,
          },
        ],
      },
    ],
  };
}

export interface AuthAuthorizedCardOpts {
  /** Optional human-readable user name from `/authen/v1/user_info` —
   * if we have it, the card shows "Signed in as <name>". Falls back
   * to a generic line when omitted (we don't always fetch user info). */
  userName?: string;
}

export function buildAuthorizedCard(opts: AuthAuthorizedCardOpts): object {
  const subject = opts.userName
    ? `Signed in as **${escapeMarkdown(opts.userName)}**.`
    : "You're now signed in.";
  return {
    config: { wide_screen_mode: true },
    header: {
      template: "green",
      title: { tag: "plain_text", content: "✅ Authorized" },
    },
    elements: [
      {
        tag: "div",
        text: {
          tag: "lark_md",
          content: `${subject} The agent will retry your request automatically.`,
        },
      },
    ],
  };
}

export type AuthFailureKind = "denied" | "expired" | "error";

export interface AuthFailedCardOpts {
  kind: AuthFailureKind;
  /** Optional detail line. For `kind: "error"` this is the only place
   * the user sees what went wrong. */
  detail?: string;
}

export function buildAuthFailedCard(opts: AuthFailedCardOpts): object {
  const { title, body } = describeFailure(opts);
  return {
    config: { wide_screen_mode: true },
    header: {
      template: "red",
      title: { tag: "plain_text", content: title },
    },
    elements: [
      { tag: "div", text: { tag: "lark_md", content: body } },
    ],
  };
}

function describeFailure(opts: AuthFailedCardOpts): { title: string; body: string } {
  switch (opts.kind) {
    case "denied":
      return {
        title: "❌ Authorization denied",
        body: "You declined the request. Ask the agent again to retry.",
      };
    case "expired":
      return {
        title: "⏱ Authorization expired",
        body: "The authorization page timed out. Ask the agent again to start a fresh sign-in.",
      };
    case "error": {
      const detail = opts.detail?.trim();
      return {
        title: "❌ Authorization failed",
        body: detail
          ? `Something went wrong: ${escapeMarkdown(detail)}. Ask the agent again to retry.`
          : "Something went wrong. Ask the agent again to retry.",
      };
    }
  }
}

function escapeMarkdown(s: string): string {
  return s.replace(/[\\`*_{}\[\]()#+\-.!|>]/g, (m) => `\\${m}`);
}
