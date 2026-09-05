/// The one place the bundle knows how it talks to a native host.
///
/// The MESSAGE SHAPES are the contract and they do not vary — both shells parse
/// the same `{type, ...}` objects (see `docs/modules/mobile/web-bundle.md`).
/// What varies is the pipe:
///
/// - **iOS** — `WKScriptMessageHandler`, reached at
///   `window.webkit.messageHandlers.<name>`, which accepts a structured object.
/// - **Android** — `WebViewCompat.addWebMessageListener`, which injects
///   `window.<name>Host` and accepts a **string**, so the object is serialized
///   here rather than at every call site. The `Host` suffix is not decoration:
///   `window.baybo` is already the INBOUND surface native calls into, and an
///   outbound object of the same name would shadow it.
/// - **A plain dev browser** — neither exists, so posts degrade to `console.log`
///   and the page still runs.
///
/// iOS wins when both are present. That ordering is what lets the existing
/// suites keep stubbing `window.webkit` without knowing this module exists.

export type NativeChannelName = "baybo" | "deck";

export type NativeChannel = {
  /// True when a real host is listening. Callers use it to decide whether a
  /// feature that needs native (a file pick, a share sheet) is offered at all.
  readonly available: boolean;
  post(message: unknown): void;
};

declare global {
  interface Window {
    webkit?: {
      messageHandlers?: Partial<
        Record<NativeChannelName, { postMessage(message: unknown): void }>
      >;
    };
    /// Injected by the Android host per `addWebMessageListener` name.
    bayboHost?: { postMessage(message: string): void };
    deckHost?: { postMessage(message: string): void };
  }
}

export function nativeChannel(name: NativeChannelName): NativeChannel {
  const webkit = window.webkit?.messageHandlers?.[name];
  if (webkit) {
    return {
      available: true,
      post: (message) => {
        webkit.postMessage(message);
      },
    };
  }

  const android = name === "baybo" ? window.bayboHost : window.deckHost;
  if (android) {
    return {
      available: true,
      post: (message) => {
        android.postMessage(JSON.stringify(message));
      },
    };
  }

  return {
    available: false,
    post: (message) => {
      console.log(`[${name} bridge]`, message);
    },
  };
}

/// The origin this document was served from, which is the origin its native
/// host answers `/blob/`, `/html-preview/` and `/html-lib/` on.
///
/// Needed because a `sandbox="allow-scripts"` frame has an **opaque** origin, so
/// its CSP cannot say `'self'` and has to name a concrete one. Everywhere else
/// the bundle uses root-relative URLs, which resolve against this without
/// anyone having to know what it is: `baybo-transcript://localhost` under the
/// iOS scheme handler, `https://appassets.androidplatform.net` under the
/// Android asset interceptor.
export function hostOrigin(): string {
  return `${window.location.protocol}//${window.location.host}`;
}
