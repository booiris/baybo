//! The gateway's browser-view hub: owns the link listener, counts web
//! viewers, turns the 0↔1 viewer edges into `StartScreencast` /
//! `StopScreencast`, caches the sidecar's latest state so a new viewer sees
//! it at once, and fans frames out latest-only.
//!
//! The gateway gets a [`BrowserViewer`] and nothing else: it can subscribe
//! a viewer, never reach the link or the cached state directly.

use std::collections::VecDeque;
use std::sync::Arc;

use bytes::Bytes;
use parking_lot::Mutex;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::clamp;
use crate::codec::LinkMessage;
use crate::error::ViewError;
use crate::limits::MAX_VIEWERS;
use crate::listener::LinkListener;
use crate::params::BrowserLinkParams;
use crate::wire::{BrowserStatus, LinkUp, StreamStatus, TargetsMsg, UnavailableReason, ViewerDown};

/// State updates a viewer may fall behind by before it is resynced from
/// the cached snapshot.
const VIEWER_EVENT_QUEUE: usize = 64;

/// What the gateway decided about the link before building the hub.
#[derive(Debug, Clone)]
pub enum BrowserLinkConfig {
    /// The view is off (`BrowserConfig::view_enabled`): viewers see
    /// `Link { up: false, unavailable: browser_disabled }`.
    Off,
    /// The view is on but no link can come up (no usable socket path, or no
    /// browser sidecar bundle in this build). Viewers see `Link { up: false }`
    /// with no reason, the same as a listener that failed to bind.
    Failed,
    /// Bind the listener here.
    Listen(BrowserLinkParams),
}

pub struct BrowserViewHubConfig {
    pub link: BrowserLinkConfig,
    /// Gateway shutdown: stops the listener and the link, and ends every
    /// viewer subscription with [`ViewEvent::Shutdown`].
    pub shutdown: CancellationToken,
}

/// What [`BrowserViewHub::from_config`] built.
pub struct BrowserViewHub {
    pub viewer: BrowserViewer,
    /// The link actually listening, to hand to the sidecar's env. `None`
    /// when the view is off or the listener failed to bind, so the sidecar
    /// never dials a socket nobody serves.
    pub link: Option<BrowserLinkParams>,
    /// The listener task, for the gateway's task tracker. It ends on
    /// shutdown and takes every link task with it. `None` without a link.
    pub listener_task: Option<JoinHandle<()>>,
}

impl BrowserViewHub {
    /// Bind the link listener (before the sidecar spawns) and start
    /// accepting. Must run inside a tokio runtime. A bind failure degrades
    /// to a hub without a link rather than failing gateway boot.
    pub fn from_config(config: BrowserViewHubConfig) -> Self {
        let BrowserViewHubConfig { link, shutdown } = config;
        let unlinked = |mode| Self {
            viewer: BrowserViewer::new(mode, shutdown.clone()),
            link: None,
            listener_task: None,
        };
        let params = match link {
            BrowserLinkConfig::Off => return unlinked(LinkMode::Disabled),
            BrowserLinkConfig::Failed => return unlinked(LinkMode::Unbound),
            BrowserLinkConfig::Listen(params) => params,
        };
        match LinkListener::bind(params.socket()) {
            Ok(listener) => {
                let viewer = BrowserViewer::new(LinkMode::Listening, shutdown.clone());
                let listener_task = tokio::spawn(listener.run(
                    Arc::clone(&viewer.shared),
                    params.secret().clone(),
                    shutdown,
                ));
                Self {
                    viewer,
                    link: Some(params),
                    listener_task: Some(listener_task),
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "browser view: link listener did not bind; live view unavailable");
                unlinked(LinkMode::Unbound)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkMode {
    Disabled,
    /// Enabled, but no link will come up (no socket path, no sidecar
    /// bundle, or the listener could not bind).
    Unbound,
    Listening,
}

/// The gateway's port into the hub. Cheap to clone.
#[derive(Clone)]
pub struct BrowserViewer {
    shared: Arc<HubShared>,
}

impl BrowserViewer {
    fn new(mode: LinkMode, shutdown: CancellationToken) -> Self {
        let (events, _) = broadcast::channel(VIEWER_EVENT_QUEUE);
        Self {
            shared: Arc::new(HubShared {
                mode,
                state: Mutex::new(HubState::default()),
                events,
                frames: watch::Sender::new(None),
                want_stream: watch::Sender::new(false),
                shutdown,
            }),
        }
    }

    /// Register a viewer. The subscription first yields the current
    /// snapshot (`Link`, then any cached `Status` / `Targets` / `Stream`),
    /// then live updates and frames. Dropping it unregisters the viewer.
    pub fn subscribe(&self) -> Result<ViewSubscription, ViewError> {
        let mut state = self.shared.state.lock();
        if state.viewers >= MAX_VIEWERS {
            return Err(ViewError::TooManyViewers { max: MAX_VIEWERS });
        }
        state.viewers += 1;
        if state.viewers == 1 {
            self.shared.want_stream.send_replace(true);
        }
        let pending = self.shared.snapshot(&state);
        let events = self.shared.events.subscribe();
        let mut frames = self.shared.frames.subscribe();
        frames.mark_changed();
        drop(state);
        Ok(ViewSubscription {
            shared: Arc::clone(&self.shared),
            pending,
            events,
            frames,
        })
    }
}

/// One item for a viewer's writer.
#[derive(Debug, Clone, PartialEq)]
pub enum ViewEvent {
    /// A JSON text message.
    State(ViewerDown),
    /// A binary message, `[u32 BE hdr_len][FrameHeader JSON][JPEG]` exactly
    /// as the sidecar sent it.
    Frame(Bytes),
    /// The gateway is shutting down.
    Shutdown,
}

/// A registered viewer. Frames are latest-only: a viewer that is slow to
/// call [`Self::recv`] skips to the newest frame and never holds up the
/// link.
pub struct ViewSubscription {
    shared: Arc<HubShared>,
    pending: VecDeque<ViewerDown>,
    events: broadcast::Receiver<ViewerDown>,
    frames: watch::Receiver<Option<Bytes>>,
}

impl ViewSubscription {
    /// Next item to send. State messages take priority over frames.
    /// Cancel-safe.
    pub async fn recv(&mut self) -> ViewEvent {
        loop {
            if let Some(msg) = self.pending.pop_front() {
                return ViewEvent::State(msg);
            }
            tokio::select! {
                biased;
                _ = self.shared.shutdown.cancelled() => return ViewEvent::Shutdown,
                event = self.events.recv() => match event {
                    Ok(msg) => return ViewEvent::State(msg),
                    Err(broadcast::error::RecvError::Lagged(_)) => self.resync(),
                    Err(broadcast::error::RecvError::Closed) => return ViewEvent::Shutdown,
                },
                changed = self.frames.changed() => {
                    if changed.is_err() {
                        return ViewEvent::Shutdown;
                    }
                    if let Some(frame) = self.frames.borrow_and_update().clone() {
                        return ViewEvent::Frame(frame);
                    }
                }
            }
        }
    }

    fn resync(&mut self) {
        let state = self.shared.state.lock();
        self.pending = self.shared.snapshot(&state);
        self.events = self.shared.events.subscribe();
    }
}

impl Drop for ViewSubscription {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock();
        state.viewers = state.viewers.saturating_sub(1);
        if state.viewers == 0 {
            self.shared.want_stream.send_replace(false);
            self.shared.frames.send_replace(None);
            // The sidecar says nothing on stop, so a cached `live` would
            // greet the next viewer with a target the agent may have closed.
            state.snapshot.stream = None;
        }
    }
}

pub(crate) struct HubShared {
    mode: LinkMode,
    state: Mutex<HubState>,
    /// Published under `state`'s lock so a snapshot plus a fresh receiver
    /// taken under the same lock neither miss nor repeat an update.
    events: broadcast::Sender<ViewerDown>,
    frames: watch::Sender<Option<Bytes>>,
    /// `true` while any viewer is subscribed; each link's writer turns its
    /// edges into `StartScreencast` / `StopScreencast`.
    want_stream: watch::Sender<bool>,
    shutdown: CancellationToken,
}

#[derive(Default)]
struct HubState {
    viewers: usize,
    link_epoch: u32,
    link_up: bool,
    snapshot: Snapshot,
}

#[derive(Default)]
struct Snapshot {
    unavailable: Option<UnavailableReason>,
    status: Option<BrowserStatus>,
    targets: Option<TargetsMsg>,
    stream: Option<StreamStatus>,
}

impl HubShared {
    fn link_message(&self, state: &HubState) -> ViewerDown {
        let unavailable = match self.mode {
            LinkMode::Disabled => Some(UnavailableReason::BrowserDisabled),
            LinkMode::Unbound | LinkMode::Listening => state.snapshot.unavailable,
        };
        ViewerDown::link(state.link_up, state.link_epoch, unavailable)
    }

    fn snapshot(&self, state: &HubState) -> VecDeque<ViewerDown> {
        let snap = &state.snapshot;
        [Some(self.link_message(state))]
            .into_iter()
            .chain([
                snap.status.clone().map(ViewerDown::Status),
                snap.targets.clone().map(ViewerDown::Targets),
                snap.stream.clone().map(ViewerDown::Stream),
            ])
            .flatten()
            .collect()
    }

    fn publish(&self, msg: ViewerDown) {
        // No receivers is fine: nobody is watching.
        let _ = self.events.send(msg);
    }

    /// First-wins: claim the link slot for a freshly authenticated sidecar.
    /// `None` while another link is live.
    pub(crate) fn claim_link(self: &Arc<Self>) -> Option<LinkLease> {
        let mut state = self.state.lock();
        if state.link_up {
            return None;
        }
        state.link_up = true;
        state.link_epoch = state.link_epoch.wrapping_add(1);
        state.snapshot = Snapshot::default();
        self.frames.send_replace(None);
        self.publish(self.link_message(&state));
        Some(LinkLease {
            shared: Arc::clone(self),
            epoch: state.link_epoch,
            want_stream: self.want_stream.subscribe(),
        })
    }

    fn release_link(&self, epoch: u32) {
        let mut state = self.state.lock();
        if !state.link_up || state.link_epoch != epoch {
            return;
        }
        state.link_up = false;
        state.snapshot = Snapshot::default();
        self.frames.send_replace(None);
        self.publish(self.link_message(&state));
    }

    fn on_link_message(&self, epoch: u32, msg: LinkMessage) {
        let mut state = self.state.lock();
        if !state.link_up || state.link_epoch != epoch {
            return;
        }
        match msg {
            LinkMessage::Frame(frame) => {
                if state.viewers > 0 {
                    self.frames.send_replace(Some(frame.header_and_jpeg));
                }
            }
            LinkMessage::Json(LinkUp::Status(status)) => {
                let status = clamp::status(status);
                state.snapshot.status = Some(status.clone());
                self.publish(ViewerDown::Status(status));
            }
            LinkMessage::Json(LinkUp::Targets(targets)) => {
                let targets = clamp::targets(targets);
                state.snapshot.targets = Some(targets.clone());
                self.publish(ViewerDown::Targets(targets));
            }
            LinkMessage::Json(LinkUp::Stream(stream)) => {
                // Stream state means something only while streaming; one that
                // lands after the last viewer left must not be cached for the
                // next one (see `ViewSubscription::drop`).
                if state.viewers > 0 {
                    let stream = clamp::stream(stream);
                    state.snapshot.stream = Some(stream.clone());
                    self.publish(ViewerDown::Stream(stream));
                }
            }
            LinkMessage::Json(LinkUp::Availability { unavailable }) => {
                state.snapshot.unavailable = unavailable;
                self.publish(self.link_message(&state));
            }
            LinkMessage::Json(LinkUp::Hello { .. }) => {
                tracing::debug!("browser link: ignoring a repeated hello");
            }
            LinkMessage::Malformed { kind, reason } => {
                tracing::debug!(?kind, %reason, "browser link: skipping a malformed message");
            }
        }
    }
}

/// The live link's hold on the hub. Dropping it marks the link down, so a
/// link task that ends any way at all frees the slot for the next sidecar.
pub(crate) struct LinkLease {
    shared: Arc<HubShared>,
    epoch: u32,
    want_stream: watch::Receiver<bool>,
}

impl LinkLease {
    pub(crate) fn on_message(&self, msg: LinkMessage) {
        self.shared.on_link_message(self.epoch, msg);
    }

    pub(crate) fn want_stream(&mut self) -> &mut watch::Receiver<bool> {
        &mut self.want_stream
    }
}

impl Drop for LinkLease {
    fn drop(&mut self) {
        self.shared.release_link(self.epoch);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::codec::FrameMsg;
    use crate::wire::{BrowserMode, BrowserPhase, StreamState, TargetId, TargetInfo};

    fn hub() -> BrowserViewer {
        BrowserViewer::new(LinkMode::Listening, CancellationToken::new())
    }

    fn frame(seq: u32) -> LinkMessage {
        LinkMessage::Frame(FrameMsg {
            header_and_jpeg: Bytes::from(seq.to_be_bytes().to_vec()),
        })
    }

    fn status() -> BrowserStatus {
        BrowserStatus {
            mode: BrowserMode::Host,
            phase: BrowserPhase::Ready,
            browser_gen: 1,
        }
    }

    async fn next(sub: &mut ViewSubscription) -> ViewEvent {
        tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("view event")
    }

    #[tokio::test]
    async fn disabled_hub_reports_browser_disabled() {
        let viewer = BrowserViewer::new(LinkMode::Disabled, CancellationToken::new());
        let mut sub = viewer.subscribe().unwrap();
        assert_eq!(
            next(&mut sub).await,
            ViewEvent::State(ViewerDown::link(
                false,
                0,
                Some(UnavailableReason::BrowserDisabled)
            ))
        );
    }

    #[tokio::test]
    async fn want_stream_flips_once_per_edge() {
        let viewer = hub();
        let mut want = viewer.shared.want_stream.subscribe();
        assert!(!*want.borrow_and_update());
        let a = viewer.subscribe().unwrap();
        assert!(want.has_changed().unwrap());
        assert!(*want.borrow_and_update());
        let b = viewer.subscribe().unwrap();
        assert!(!want.has_changed().unwrap(), "second viewer is no edge");
        drop(a);
        assert!(!want.has_changed().unwrap(), "one viewer left is no edge");
        drop(b);
        assert!(want.has_changed().unwrap());
        assert!(!*want.borrow_and_update());
    }

    #[tokio::test]
    async fn max_viewers_enforced_and_slot_freed_on_drop() {
        let viewer = hub();
        let mut subs: Vec<_> = (0..MAX_VIEWERS)
            .map(|_| viewer.subscribe().unwrap())
            .collect();
        assert_eq!(
            viewer.subscribe().err(),
            Some(ViewError::TooManyViewers { max: MAX_VIEWERS })
        );
        subs.pop();
        assert!(viewer.subscribe().is_ok());
    }

    #[tokio::test]
    async fn new_viewer_gets_cached_snapshot() {
        let viewer = hub();
        let lease = viewer.shared.claim_link().unwrap();
        lease.on_message(LinkMessage::Json(LinkUp::Status(status())));
        let mut sub = viewer.subscribe().unwrap();
        assert_eq!(
            next(&mut sub).await,
            ViewEvent::State(ViewerDown::link(true, 1, None))
        );
        assert_eq!(
            next(&mut sub).await,
            ViewEvent::State(ViewerDown::Status(status()))
        );
    }

    #[tokio::test]
    async fn slow_viewer_only_sees_latest_frame() {
        let viewer = hub();
        let lease = viewer.shared.claim_link().unwrap();
        let mut sub = viewer.subscribe().unwrap();
        assert!(matches!(next(&mut sub).await, ViewEvent::State(_)));
        for seq in 0..50 {
            lease.on_message(frame(seq));
        }
        assert_eq!(
            next(&mut sub).await,
            ViewEvent::Frame(Bytes::from(49u32.to_be_bytes().to_vec()))
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), sub.recv())
                .await
                .is_err(),
            "no backlog of older frames"
        );
    }

    #[tokio::test]
    async fn stale_epoch_messages_are_dropped() {
        let viewer = hub();
        let old = viewer.shared.claim_link().unwrap();
        let old_epoch = old.epoch;
        drop(old);
        let _new = viewer.shared.claim_link().unwrap();
        let mut sub = viewer.subscribe().unwrap();
        viewer.shared.on_link_message(old_epoch, frame(1));
        viewer
            .shared
            .on_link_message(old_epoch, LinkMessage::Json(LinkUp::Status(status())));
        assert_eq!(
            next(&mut sub).await,
            ViewEvent::State(ViewerDown::link(true, 2, None))
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), sub.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn link_down_clears_snapshot() {
        let viewer = hub();
        let lease = viewer.shared.claim_link().unwrap();
        lease.on_message(LinkMessage::Json(LinkUp::Status(status())));
        drop(lease);
        let mut sub = viewer.subscribe().unwrap();
        assert_eq!(
            next(&mut sub).await,
            ViewEvent::State(ViewerDown::link(false, 1, None))
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), sub.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn lagging_viewer_is_resynced_from_snapshot() {
        let viewer = hub();
        let lease = viewer.shared.claim_link().unwrap();
        let mut sub = viewer.subscribe().unwrap();
        assert!(matches!(next(&mut sub).await, ViewEvent::State(_)));
        for gen_ in 0..(VIEWER_EVENT_QUEUE as u32 * 2) {
            lease.on_message(LinkMessage::Json(LinkUp::Status(BrowserStatus {
                browser_gen: gen_,
                ..status()
            })));
        }
        let last = BrowserStatus {
            browser_gen: VIEWER_EVENT_QUEUE as u32 * 2 - 1,
            ..status()
        };
        assert_eq!(
            next(&mut sub).await,
            ViewEvent::State(ViewerDown::link(true, 1, None))
        );
        assert_eq!(
            next(&mut sub).await,
            ViewEvent::State(ViewerDown::Status(last))
        );
    }

    #[tokio::test]
    async fn shutdown_ends_subscriptions() {
        let token = CancellationToken::new();
        let viewer = BrowserViewer::new(LinkMode::Listening, token.clone());
        let mut sub = viewer.subscribe().unwrap();
        assert!(matches!(next(&mut sub).await, ViewEvent::State(_)));
        token.cancel();
        assert_eq!(next(&mut sub).await, ViewEvent::Shutdown);
    }

    #[tokio::test]
    async fn availability_updates_the_link_message() {
        let viewer = hub();
        let lease = viewer.shared.claim_link().unwrap();
        let mut sub = viewer.subscribe().unwrap();
        assert!(matches!(next(&mut sub).await, ViewEvent::State(_)));
        lease.on_message(LinkMessage::Json(LinkUp::Availability {
            unavailable: Some(UnavailableReason::TapNotFired),
        }));
        assert_eq!(
            next(&mut sub).await,
            ViewEvent::State(ViewerDown::link(
                true,
                1,
                Some(UnavailableReason::TapNotFired)
            ))
        );
    }

    fn live() -> StreamStatus {
        StreamStatus {
            state: StreamState::Live,
            browser_gen: 1,
            target: Some(TargetInfo {
                target_id: TargetId::new("T1"),
                url: "https://a.example/".into(),
                title: "a".into(),
            }),
        }
    }

    #[tokio::test]
    async fn last_viewer_leaving_forgets_the_stream_state() {
        let viewer = hub();
        let lease = viewer.shared.claim_link().unwrap();
        let first = viewer.subscribe().unwrap();
        lease.on_message(LinkMessage::Json(LinkUp::Stream(live())));
        drop(first);
        // A late message after the stop is not cached either.
        lease.on_message(LinkMessage::Json(LinkUp::Stream(live())));
        let mut sub = viewer.subscribe().unwrap();
        assert_eq!(
            next(&mut sub).await,
            ViewEvent::State(ViewerDown::link(true, 1, None))
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), sub.recv())
                .await
                .is_err(),
            "no stale stream state"
        );
    }

    #[tokio::test]
    async fn inbound_state_is_clamped_to_the_announced_limits() {
        let viewer = hub();
        let lease = viewer.shared.claim_link().unwrap();
        let mut sub = viewer.subscribe().unwrap();
        assert!(matches!(next(&mut sub).await, ViewEvent::State(_)));
        let mut stream = live();
        if let Some(t) = stream.target.as_mut() {
            t.title = "t".repeat(10_000);
        }
        lease.on_message(LinkMessage::Json(LinkUp::Stream(stream)));
        match next(&mut sub).await {
            ViewEvent::State(ViewerDown::Stream(s)) => assert_eq!(
                s.target.map(|t| t.title.len()),
                Some(crate::limits::MAX_LINK_TEXT_CHARS as usize)
            ),
            other => panic!("expected a stream, got {other:?}"),
        }
    }
}
