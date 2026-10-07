// Pure state for the browser live view: folding `ViewerDown` messages into
// one snapshot, and deciding what the overlay over the canvas says.

import type {
  BrowserStatus,
  DockerPhase,
  LinkMsg,
  StreamStatus,
  TargetInfo,
  TargetsMsg,
  ViewerDown,
  ViewerErrorCode,
} from '../../api/browserViewTypes';
import type { BrowserViewConnStatus } from '../../api/browserViewWs';

export interface ViewState {
  conn: BrowserViewConnStatus;
  link: LinkMsg | null;
  browser: BrowserStatus | null;
  targets: TargetsMsg | null;
  stream: StreamStatus | null;
  error: ViewerErrorCode | null;
}

export const INITIAL_VIEW_STATE: ViewState = {
  conn: { state: 'connecting' },
  link: null,
  browser: null,
  targets: null,
  stream: null,
  error: null,
};

/** Which frame the canvas is showing, as far as the overlay cares. */
export interface ShownFrame {
  target_id: string;
  browser_gen: number;
}

export function applyConnStatus(state: ViewState, conn: BrowserViewConnStatus): ViewState {
  // Everything below the link is per-connection: a reconnect replays it.
  if (conn.state !== 'connected') return { ...INITIAL_VIEW_STATE, conn, error: state.error };
  return { ...INITIAL_VIEW_STATE, conn };
}

export function applyViewerDown(state: ViewState, msg: ViewerDown): ViewState {
  switch (msg.type) {
    case 'link': {
      const link = msg;
      // A new link epoch is a new sidecar session: nothing it said before
      // still holds.
      const sameLink = state.link !== null && state.link.link_epoch === link.link_epoch && link.up;
      return sameLink
        ? { ...state, link }
        : { ...state, link, browser: null, targets: null, stream: null };
    }
    case 'status': {
      const { type: _type, ...browser } = msg;
      return { ...state, browser };
    }
    case 'targets': {
      const { type: _type, ...targets } = msg;
      return { ...state, targets };
    }
    case 'stream': {
      const { type: _type, ...stream } = msg;
      return { ...state, stream };
    }
    case 'error':
      return { ...state, error: msg.code };
    case 'pong':
      return state;
  }
}

/** The page the view follows, for the URL bar: the stream's own target,
 *  else the followed entry of the tab list. */
export function followedTarget(state: ViewState): TargetInfo | null {
  if (state.stream?.target) return state.stream.target;
  const followed = state.targets?.followed;
  if (followed == null) return null;
  return state.targets?.targets.find((t) => t.target_id === followed) ?? null;
}

export type OverlayTone = 'info' | 'warn' | 'err';

export interface Overlay {
  tone: OverlayTone;
  title: string;
  detail?: string;
  /** 0–100, for a progress bar. */
  progress?: number;
}

const DOCKER_PHASE_TEXT: Record<DockerPhase, string> = {
  'docker-checking': 'Checking Docker…',
  'docker-building-image': 'Building the browser image…',
  'docker-starting-container': 'Starting the browser container…',
  'docker-waiting-for-cdp': 'Waiting for the browser to accept connections…',
};

const NOT_LAUNCHED: Overlay = {
  tone: 'info',
  title: 'Browser not launched yet',
  detail: "It starts on the agent's first browser tool call.",
};

/** What the overlay says, or `null` when the live picture speaks for itself. */
export function describeView(state: ViewState, shown: ShownFrame | null): Overlay | null {
  const { conn, link, browser, stream } = state;
  switch (conn.state) {
    case 'hidden':
      return {
        tone: 'info',
        title: 'Paused',
        detail: 'The view disconnects while this tab is hidden.',
      };
    case 'connecting':
      return { tone: 'info', title: 'Connecting to the gateway…' };
    case 'disconnected':
      return {
        tone: 'warn',
        title: 'Disconnected from the gateway',
        detail:
          state.error === 'too_many_viewers'
            ? `Too many viewers are open. Retrying in ${secs(conn.retryInMs)}s.`
            : `Retrying in ${secs(conn.retryInMs)}s.`,
      };
    case 'connected':
      break;
  }
  if (!link) return { tone: 'info', title: 'Waiting for the gateway…' };
  switch (link.unavailable) {
    case 'browser_disabled':
      return {
        tone: 'info',
        title: 'Browser is disabled',
        detail: 'Turn on browser.enable and browser.view.enable in the gateway config.',
      };
    case 'cddm_version_mismatch':
      return {
        tone: 'err',
        title: 'Live view unavailable',
        detail:
          'The bundled chrome-devtools-mcp is a version the live view does not support. Browser tools still work.',
      };
    case 'tap_not_fired':
      return {
        tone: 'warn',
        title: 'Live view unavailable',
        detail:
          'The sidecar could not hook into the browser session. Browser tools still work.',
      };
    case null:
      break;
  }
  if (!link.up) {
    return {
      tone: 'warn',
      title: 'Browser sidecar not connected',
      detail: 'The view starts once the browser sidecar connects to the gateway.',
    };
  }
  if (!browser) return { tone: 'info', title: 'Waiting for browser status…' };
  switch (browser.phase.type) {
    case 'idle':
      return NOT_LAUNCHED;
    case 'installing': {
      const percent = clampPercent(browser.phase.percent);
      return { tone: 'info', title: 'Installing the browser…', detail: `${percent}%`, progress: percent };
    }
    case 'docker':
      return {
        tone: 'info',
        title: 'Starting the Docker browser',
        detail: DOCKER_PHASE_TEXT[browser.phase.phase],
      };
    case 'recovering':
      return {
        tone: 'warn',
        title: 'Browser recovering…',
        detail: 'The view resumes when the browser is back.',
      };
    case 'failed':
      return { tone: 'err', title: 'Browser failed', detail: browser.phase.error };
    case 'ready':
      break;
  }
  if (!stream) return { tone: 'info', title: 'Waiting for the stream…' };
  switch (stream.state) {
    case 'idle':
      return NOT_LAUNCHED;
    case 'paused':
      return {
        tone: 'warn',
        title: 'Stream paused',
        detail: 'The browser is recovering; frames resume when it is back.',
      };
    case 'background':
      return {
        tone: 'info',
        title: 'Tab in background',
        detail: 'Chrome is not painting the followed tab while another tab is in front.',
      };
    case 'target_gone':
      return {
        tone: 'warn',
        title: 'Followed tab closed',
        detail: 'The view switches when the agent uses another tab.',
      };
    case 'unavailable':
      return { tone: 'err', title: 'Live view unavailable' };
    case 'live':
      break;
  }
  const current =
    shown !== null &&
    shown.browser_gen === stream.browser_gen &&
    (stream.target === null || shown.target_id === stream.target.target_id);
  return current ? null : { tone: 'info', title: 'Waiting for the first frame…' };
}

export function viewerErrorText(code: ViewerErrorCode): string {
  switch (code) {
    case 'too_many_viewers':
      return 'Too many viewers are connected to this gateway.';
    case 'bad_message':
      return 'The gateway rejected a message from this page.';
    case 'takeover_disabled':
      return 'Taking control of the browser is not available.';
  }
}

function secs(ms: number): number {
  return Math.ceil(ms / 1000);
}

function clampPercent(p: number): number {
  return Math.round(Math.min(100, Math.max(0, p)));
}

const LOOPBACK_HOSTS: ReadonlySet<string> = new Set(['localhost', '[::1]', '::1']);

/** True when the gateway is reached over plain http on a host other than
 *  this machine: the frames (and the admin token) cross the network in
 *  the clear. */
export function isInsecureRemote(baseUrl: string): boolean {
  let u: URL;
  try {
    u = new URL(baseUrl);
  } catch {
    return false;
  }
  if (u.protocol !== 'http:') return false;
  const host = u.hostname.toLowerCase();
  return !(LOOPBACK_HOSTS.has(host) || host.endsWith('.localhost') || /^127\.\d+\.\d+\.\d+$/.test(host));
}
