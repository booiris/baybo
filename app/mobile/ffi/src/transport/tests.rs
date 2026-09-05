//! The transport test suite: drives the real supervisor + pump against a
//! loopback WS server. See `app/ios/docs/connection.md` § Testing.

use std::time::Instant;

use futures_util::SinkExt;

use super::pump::{PumpCtx, dispatch_inbound_frame};
use super::supervisor::Msg;
use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::{accept_async, client_async};

use crate::core::{decode, encode, user_message_frame};

/// Frames are built by decoding JSON rather than by struct literal: `Frame`
/// carries a `chrono` timestamp the FFI crate doesn't otherwise depend on.
fn frame(json: &str) -> Frame {
    serde_json::from_str(json).expect("decode test frame")
}

fn session_activity(session_id: &str, source: &str, at: &str) -> Frame {
    frame(&format!(
        r#"{{"kind":"session_activity","session_id":"{session_id}","source":"{source}","at":"{at}"}}"#
    ))
}

fn notice(session_id: &str, text: &str) -> Frame {
    frame(&format!(
        r#"{{"kind":"notice","session_id":"{session_id}","level":"info","text":"{text}"}}"#
    ))
}

fn approval_resolved(call_id: &str) -> Frame {
    frame(&format!(
        r#"{{"kind":"approval_resolved","call_id":"{call_id}","decision":"approve"}}"#
    ))
}

#[derive(Default)]
struct RecordingSink {
    frames: parking_lot::Mutex<Vec<String>>,
    disconnects: parking_lot::Mutex<Vec<String>>,
}

impl RecordingSink {
    fn frames(&self) -> Vec<String> {
        self.frames.lock().clone()
    }

    fn kinds(&self) -> Vec<String> {
        self.frames()
            .iter()
            .map(|json| {
                let value: serde_json::Value = serde_json::from_str(json).expect("parse frame");
                value["kind"].as_str().unwrap_or_default().to_string()
            })
            .collect()
    }

    fn disconnects(&self) -> Vec<String> {
        self.disconnects.lock().clone()
    }
}

impl FrameSink for RecordingSink {
    fn on_frame(&self, frame_json: String) {
        self.frames.lock().push(frame_json);
    }

    fn on_disconnected(&self, session_id: String) {
        self.disconnects.lock().push(session_id);
    }
}

#[derive(Default)]
struct RecordingListSink {
    activity: parking_lot::Mutex<Vec<(String, String, i64)>>,
    titles: parking_lot::Mutex<Vec<(String, String)>>,
    approvals: parking_lot::Mutex<Vec<(String, bool)>>,
    stale: parking_lot::Mutex<usize>,
}

impl SessionListSink for RecordingListSink {
    fn on_activity(&self, session_id: String, source: String, at_millis: i64) {
        self.activity.lock().push((session_id, source, at_millis));
    }

    fn on_title(&self, session_id: String, title: String) {
        self.titles.lock().push((session_id, title));
    }

    fn on_approval_pending(&self, session_id: String, pending: bool) {
        self.approvals.lock().push((session_id, pending));
    }

    fn on_list_stale(&self) {
        *self.stale.lock() += 1;
    }
}

#[derive(Default)]
struct RecordingDeckSink {
    cards: parking_lot::Mutex<Vec<(String, u32, String)>>,
    changed: parking_lot::Mutex<usize>,
}

impl DeckSink for RecordingDeckSink {
    fn on_card_data(&self, card_id: String, seq: u32, payload: String) {
        self.cards.lock().push((card_id, seq, payload));
    }

    fn on_deck_changed(&self) {
        *self.changed.lock() += 1;
    }
}

#[derive(Default)]
struct RecordingProjectSink {
    changed: parking_lot::Mutex<Vec<(String, String, Option<u32>)>>,
    stale: parking_lot::Mutex<usize>,
}

impl ProjectSink for RecordingProjectSink {
    fn on_project_changed(&self, project_id: String, scope: String, issue_number: Option<u32>) {
        self.changed.lock().push((project_id, scope, issue_number));
    }

    fn on_project_stale(&self) {
        *self.stale.lock() += 1;
    }
}

struct Fixture {
    ctx: PumpCtx,
    list: Arc<RecordingListSink>,
    deck: Arc<RecordingDeckSink>,
    project: Arc<RecordingProjectSink>,
}

impl Fixture {
    fn new(session_ids: &[&str]) -> (Self, Vec<Arc<RecordingSink>>) {
        let list = Arc::new(RecordingListSink::default());
        let deck = Arc::new(RecordingDeckSink::default());
        let project = Arc::new(RecordingProjectSink::default());
        let mut map: HashMap<String, Arc<dyn FrameSink>> = HashMap::new();
        let mut sinks = Vec::new();
        for session_id in session_ids {
            let sink = Arc::new(RecordingSink::default());
            map.insert((*session_id).to_string(), sink.clone());
            sinks.push(sink);
        }
        (
            Self {
                ctx: PumpCtx {
                    sinks: Arc::new(Mutex::new(map)),
                    list_sink: Arc::new(parking_lot::Mutex::new(Some(
                        list.clone() as Arc<dyn SessionListSink>
                    ))),
                    deck_sink: Arc::new(parking_lot::Mutex::new(Some(
                        deck.clone() as Arc<dyn DeckSink>
                    ))),
                    project_sink: Arc::new(parking_lot::Mutex::new(Some(
                        project.clone() as Arc<dyn ProjectSink>
                    ))),
                    last_inbound: Arc::new(parking_lot::Mutex::new(Instant::now())),
                    leg_id: 1,
                    events: mpsc::unbounded_channel().0,
                },
                list,
                deck,
                project,
            },
            sinks,
        )
    }

    /// Drop the connection-global list sink: a leg can pump before Swift has
    /// installed one.
    fn without_list_sink(self) -> Self {
        *self.ctx.list_sink.lock() = None;
        self
    }

    /// Drop the connection-global deck sink: a leg can pump before Swift
    /// has installed one (or the deck tab was never opened).
    fn without_deck_sink(self) -> Self {
        *self.ctx.deck_sink.lock() = None;
        self
    }

    /// Drop the connection-global project sink: a leg can pump before Swift
    /// has installed one (or the Projects tab was never opened).
    fn without_project_sink(self) -> Self {
        *self.ctx.project_sink.lock() = None;
        self
    }

    async fn dispatch(&self, frame: Frame) {
        dispatch_inbound_frame(&self.ctx, frame).await;
    }
}

/// The unread badge for a session the device has NEVER opened rides entirely on
/// this: `SessionActivity` goes to the connection-global list sink and RETURNS.
/// Delete the return and the frame falls through to per-session routing, finds
/// no sink for an unopened session, and is dropped — the badge dies.
#[tokio::test]
async fn session_activity_goes_to_the_list_sink_and_never_to_a_session_sink() {
    let (fixture, sinks) = Fixture::new(&["s1"]);

    fixture
        .dispatch(session_activity("s1", "assistant", "2026-07-12T00:00:00Z"))
        .await;

    assert_eq!(
        fixture.list.activity.lock().as_slice(),
        [("s1".to_string(), "assistant".to_string(), 1_783_814_400_000)]
    );
    assert!(
        sinks[0].frames().is_empty(),
        "the activity ping must not reach the transcript sink"
    );
}

/// A ping for a session with no sink at all — the whole point of the special
/// case.
#[tokio::test]
async fn session_activity_for_a_never_opened_session_still_reaches_the_list() {
    let (fixture, sinks) = Fixture::new(&["s1"]);

    fixture
        .dispatch(session_activity("unopened", "user", "2026-07-12T00:00:00Z"))
        .await;

    assert_eq!(fixture.list.activity.lock().len(), 1);
    assert_eq!(fixture.list.activity.lock()[0].0, "unopened");
    assert!(sinks[0].frames().is_empty());
}

/// The mark for "this conversation is blocked waiting on you" has to reach
/// a device with NOTHING subscribed — that is the state the app is in
/// while the user is looking at the chat list, and it is the only state in
/// which the mark is useful. `Frame::ApprovalRequested` cannot do this
/// (the gateway dispatches it to a session's subscribers only), which is
/// why the bit rides a `SessionUpdated` broadcast instead.
#[tokio::test]
async fn an_approval_mark_for_a_never_opened_session_still_reaches_the_list() {
    let (fixture, _sinks) = Fixture::new(&[]);

    fixture
        .dispatch(Frame::SessionUpdated {
            session_id: "unopened".into(),
            patch: wire::SessionPatch {
                approval_pending: Some(true),
                ..Default::default()
            },
        })
        .await;

    assert_eq!(
        *fixture.list.approvals.lock(),
        vec![("unopened".to_string(), true)]
    );
}

/// The clear is load-bearing on its own: a gate nobody answers self-denies
/// after five minutes and broadcasts NO resolution, so `false` here is the
/// only thing that ever retires the mark on those turns.
#[tokio::test]
async fn an_approval_clear_rides_the_same_tee() {
    let (fixture, sinks) = Fixture::new(&["s1"]);

    fixture
        .dispatch(Frame::SessionUpdated {
            session_id: "s1".into(),
            patch: wire::SessionPatch {
                approval_pending: Some(false),
                ..Default::default()
            },
        })
        .await;

    assert_eq!(
        *fixture.list.approvals.lock(),
        vec![("s1".to_string(), false)]
    );
    // TEE, not a lane: the frame still reaches the session's own sink.
    assert_eq!(sinks[0].frames().len(), 1);
}

/// A patch with both fields fires both hops — the title path and the
/// approval path are independent taps on one frame, not an either/or.
#[tokio::test]
async fn a_patch_carrying_a_title_and_an_approval_flag_fires_both_hops() {
    let (fixture, _sinks) = Fixture::new(&[]);

    fixture
        .dispatch(Frame::SessionUpdated {
            session_id: "s1".into(),
            patch: wire::SessionPatch {
                title: Some("Reset password flow".into()),
                approval_pending: Some(true),
                ..Default::default()
            },
        })
        .await;

    assert_eq!(fixture.list.titles.lock().len(), 1);
    assert_eq!(fixture.list.approvals.lock().len(), 1);
}

/// A pin / archive / hide patch carries neither field and must stay silent
/// on both — an absent `approval_pending` means "no change", never `false`.
#[tokio::test]
async fn a_patch_without_the_approval_field_changes_nothing() {
    let (fixture, _sinks) = Fixture::new(&[]);

    fixture
        .dispatch(Frame::SessionUpdated {
            session_id: "s1".into(),
            patch: wire::SessionPatch {
                pinned: Some(true),
                ..Default::default()
            },
        })
        .await;

    assert!(fixture.list.approvals.lock().is_empty());
    assert!(fixture.list.titles.lock().is_empty());
}

/// `Gap { session_id: None }` is the gateway's "I dropped a session-less
/// broadcast" nudge — and the broadcast it most often drops is the
/// `SessionActivity` announcing a session the device has never seen (a cron
/// fire, say). It has no routing session id, so without the special case it
/// falls into the fan-out branch and reaches **nobody** when no session is
/// subscribed — which is exactly the state the app is in while the user is
/// looking at the chat list. Fixture with NO sinks is the whole point.
#[tokio::test]
async fn a_session_less_gap_reaches_the_list_sink_with_nothing_subscribed() {
    let (fixture, _sinks) = Fixture::new(&[]);

    fixture.dispatch(Frame::Gap { session_id: None }).await;

    assert_eq!(
        *fixture.list.stale.lock(),
        1,
        "a session-less Gap must nudge the chat list to refetch, or a new \
             session never appears while the list is on screen",
    );
}

/// A `Gap` that DOES name a session is a transcript concern: it must keep
/// its old route to that session's frame sink, and must NOT be mistaken for
/// a list-refetch nudge.
#[tokio::test]
async fn a_session_scoped_gap_still_routes_to_its_transcript_sink() {
    let (fixture, sinks) = Fixture::new(&["s1"]);

    fixture
        .dispatch(Frame::Gap {
            session_id: Some("s1".into()),
        })
        .await;

    assert_eq!(sinks[0].kinds(), ["gap"]);
    assert_eq!(
        *fixture.list.stale.lock(),
        0,
        "a session-scoped gap is not a list-plane nudge",
    );
}

fn deck_card_data(card_id: &str, seq: u32, payload: &str) -> Frame {
    Frame::DeckCardData {
        card_id: card_id.to_string(),
        seq,
        payload: payload.to_string(),
    }
}

/// A deck push has no session to route by. Without the special case it
/// would fan out to every per-session transcript sink (as an unknown
/// frame) while a user parked on the Deck tab with nothing subscribed got
/// nothing — the exact hole the connection-global sink exists to close.
#[tokio::test]
async fn deck_card_data_goes_to_the_deck_sink_and_never_to_a_session_sink() {
    let (fixture, sinks) = Fixture::new(&["s1"]);

    fixture
        .dispatch(deck_card_data("c1", 41, r#"{"used":0.4}"#))
        .await;

    assert_eq!(
        fixture.deck.cards.lock().as_slice(),
        [("c1".to_string(), 41, r#"{"used":0.4}"#.to_string())]
    );
    assert!(
        sinks[0].frames().is_empty(),
        "a deck push must never reach a transcript sink"
    );
    assert_eq!(*fixture.deck.changed.lock(), 0);
}

/// Same routing for the structural nudge, which carries nothing at all.
#[tokio::test]
async fn deck_changed_goes_to_the_deck_sink_and_never_to_a_session_sink() {
    let (fixture, sinks) = Fixture::new(&["s1"]);

    fixture.dispatch(Frame::DeckChanged).await;

    assert_eq!(*fixture.deck.changed.lock(), 1);
    assert!(fixture.deck.cards.lock().is_empty());
    assert!(
        sinks[0].frames().is_empty(),
        "the deck nudge must never reach a transcript sink"
    );
}

/// Project broadcasts are session-less; they must use the global project sink
/// rather than fan out to transcript sinks or disappear when no chat is open.
#[tokio::test]
async fn project_changed_goes_to_the_project_sink_and_never_to_a_session_sink() {
    let (fixture, sinks) = Fixture::new(&["s1"]);

    fixture
        .dispatch(frame(
            r#"{"kind":"project_changed","project_id":"p1","scope":"timeline","issue_number":7}"#,
        ))
        .await;
    // No `issue_number`: the whole board moved, not one card.
    fixture
        .dispatch(frame(
            r#"{"kind":"project_changed","project_id":"p1","scope":"project"}"#,
        ))
        .await;

    assert_eq!(
        fixture.project.changed.lock().as_slice(),
        [
            ("p1".to_string(), "timeline".to_string(), Some(7)),
            ("p1".to_string(), "project".to_string(), None),
        ]
    );
    assert!(
        sinks[0].frames().is_empty(),
        "a board invalidation must never reach a transcript sink"
    );
}

/// A scope the wire decoded into its fallback arm still names its board, and
/// still says to refetch it — the client's answer to every scope is the same.
#[tokio::test]
async fn a_scope_this_build_cannot_name_still_reaches_the_project_sink() {
    let (fixture, _sinks) = Fixture::new(&["s1"]);

    fixture
        .dispatch(frame(
            r#"{"kind":"project_changed","project_id":"p1","scope":"swimlane"}"#,
        ))
        .await;

    assert_eq!(
        fixture.project.changed.lock().as_slice(),
        [("p1".to_string(), "unknown".to_string(), None)]
    );
}

#[tokio::test]
async fn a_session_less_gap_makes_the_board_stale_too() {
    let (fixture, sinks) = Fixture::new(&["s1"]);

    fixture.dispatch(Frame::Gap { session_id: None }).await;

    assert_eq!(*fixture.project.stale.lock(), 1);
    assert_eq!(*fixture.list.stale.lock(), 1);
    assert!(sinks[0].frames().is_empty());
}

#[tokio::test]
async fn a_project_frame_without_a_project_sink_is_dropped_not_broadcast() {
    let (fixture, sinks) = Fixture::new(&["s1"]);
    let fixture = fixture.without_project_sink();

    fixture
        .dispatch(frame(
            r#"{"kind":"project_changed","project_id":"p1","scope":"board"}"#,
        ))
        .await;

    assert!(sinks[0].frames().is_empty());
}

/// No deck sink installed (the Deck tab was never opened, or the leg
/// pumped before Swift registered one): deck frames are DROPPED, never
/// rerouted to the per-session sinks. The `GET /v1/deck` pull on the tab's
/// first open repaints from the stored snapshot, so nothing is lost.
#[tokio::test]
async fn a_deck_frame_without_a_deck_sink_is_dropped_not_broadcast() {
    let (fixture, sinks) = Fixture::new(&["s1"]);
    let fixture = fixture.without_deck_sink();

    fixture.dispatch(deck_card_data("c1", 1, "{}")).await;
    fixture.dispatch(Frame::DeckChanged).await;

    assert!(sinks[0].frames().is_empty());
}

/// The deck special-cases must not swallow the fan-out path: any OTHER
/// session-less frame (here the approval broadcast, standing in for every
/// unknown future one) still reaches every session sink exactly as before,
/// and never the deck sink.
#[tokio::test]
async fn a_session_less_non_deck_frame_still_broadcasts_past_the_deck_sink() {
    let (fixture, sinks) = Fixture::new(&["s1", "s2"]);

    fixture.dispatch(approval_resolved("call-1")).await;

    for sink in &sinks {
        assert_eq!(sink.kinds(), ["approval_resolved"]);
    }
    assert!(fixture.deck.cards.lock().is_empty());
    assert_eq!(*fixture.deck.changed.lock(), 0);
}

#[tokio::test]
async fn session_activity_maps_both_activity_kinds_to_their_wire_spellings() {
    let (fixture, _sinks) = Fixture::new(&[]);

    fixture
        .dispatch(session_activity("s1", "user", "2026-07-12T00:00:00Z"))
        .await;
    fixture
        .dispatch(session_activity("s1", "assistant", "2026-07-12T00:00:00Z"))
        .await;

    let sources: Vec<String> = fixture
        .list
        .activity
        .lock()
        .iter()
        .map(|(_, source, _)| source.clone())
        .collect();
    assert_eq!(sources, ["user", "assistant"]);
}

/// No list sink installed yet (a leg that pumped before Swift registered one):
/// the frame is still swallowed, never rerouted to a transcript.
#[tokio::test]
async fn session_activity_without_a_list_sink_is_dropped_not_broadcast() {
    let (fixture, sinks) = Fixture::new(&["s1"]);
    let fixture = fixture.without_list_sink();

    fixture
        .dispatch(session_activity("s1", "assistant", "2026-07-12T00:00:00Z"))
        .await;

    assert!(sinks[0].frames().is_empty());
}

/// `SessionUpdated{title}` fires `on_title` AND falls through to per-session
/// routing — the code comment says NOT a return, and the transcript webview
/// relies on still receiving the frame (it just ignores it).
#[tokio::test]
async fn a_title_patch_fires_on_title_and_still_falls_through_to_the_session_sink() {
    let (fixture, sinks) = Fixture::new(&["s1"]);

    fixture
        .dispatch(frame(
            r#"{"kind":"session_updated","session_id":"s1","patch":{"title":"A chat"}}"#,
        ))
        .await;

    assert_eq!(
        fixture.list.titles.lock().as_slice(),
        [("s1".to_string(), "A chat".to_string())]
    );
    assert_eq!(sinks[0].kinds(), ["session_updated"]);
}

/// Pin / archive / hide patches carry no title: no `on_title` hop, same
/// fall-through.
#[tokio::test]
async fn a_titleless_patch_fires_no_title_hop_but_still_routes() {
    let (fixture, sinks) = Fixture::new(&["s1"]);

    fixture
        .dispatch(frame(
            r#"{"kind":"session_updated","session_id":"s1","patch":{"pinned":true}}"#,
        ))
        .await;

    assert!(fixture.list.titles.lock().is_empty());
    assert_eq!(sinks[0].kinds(), ["session_updated"]);
}

/// A session-less frame broadcasts to EVERY sink. The approval card is matched
/// by prompt id precisely because of this: the gateway resolves a gate without
/// naming a session, and whichever store holds that prompt must see it.
#[tokio::test]
async fn a_session_less_frame_broadcasts_to_every_sink() {
    let (fixture, sinks) = Fixture::new(&["s1", "s2", "s3"]);

    fixture.dispatch(approval_resolved("call-1")).await;

    for sink in &sinks {
        assert_eq!(sink.kinds(), ["approval_resolved"]);
        let json: serde_json::Value = serde_json::from_str(&sink.frames()[0]).expect("parse frame");
        assert_eq!(json["call_id"], "call-1");
    }
}

/// The ghost-row leak: a frame for a session this connection has no sink for
/// (evicted by the LRU, or never opened) is DROPPED. Broadcasting it instead
/// would paint another session's rows into the open transcript.
#[tokio::test]
async fn a_frame_for_an_unknown_session_is_dropped_not_broadcast() {
    let (fixture, sinks) = Fixture::new(&["s1", "s2"]);

    fixture.dispatch(notice("evicted", "hello")).await;

    for sink in &sinks {
        assert!(
            sink.frames().is_empty(),
            "a frame for an unsubscribed session must never fan out"
        );
    }
}

#[tokio::test]
async fn a_session_frame_reaches_only_its_own_sink() {
    let (fixture, sinks) = Fixture::new(&["s1", "s2"]);

    fixture.dispatch(notice("s1", "for s1")).await;

    assert_eq!(sinks[0].kinds(), ["notice"]);
    assert!(sinks[1].frames().is_empty());
}

/// The sink receives the frame as JSON — the same shape the web transcript
/// consumes, tagged on `kind`.
#[tokio::test]
async fn a_routed_frame_arrives_as_kind_tagged_json() {
    let (fixture, sinks) = Fixture::new(&["s1"]);

    fixture.dispatch(notice("s1", "hello")).await;

    let json: serde_json::Value = serde_json::from_str(&sinks[0].frames()[0]).expect("parse frame");
    assert_eq!(json["kind"], "notice");
    assert_eq!(json["session_id"], "s1");
    assert_eq!(json["text"], "hello");
}

/// The loopback leg: a real WebSocket on 127.0.0.1 speaking the direct leg's
/// raw-MessagePack codec, so the supervisor + pump under test are the
/// production ones and only the dial is local.
struct LoopbackDialer {
    addr: std::net::SocketAddr,
    dials: Arc<AtomicUsize>,
}

struct LoopbackCodec;

impl FrameCodec for LoopbackCodec {
    fn encode_outbound(&mut self, frame: &Frame) -> Result<Vec<Vec<u8>>, TransportError> {
        Ok(vec![encode(frame).map_err(MobileError::from)?])
    }

    fn decode_inbound(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, TransportError> {
        Ok(decode(bytes).ok().into_iter().collect())
    }
}

impl LegDialer for LoopbackDialer {
    fn establish(&self) -> futures_util::future::BoxFuture<'_, Result<Connection, TransportError>> {
        Box::pin(async move {
            self.dials.fetch_add(1, Ordering::Relaxed);
            let tcp = TcpStream::connect(self.addr)
                .await
                .map_err(|e| TransportError::Other(format!("tcp: {e}")))?;
            let url = format!("ws://{}/v1/channel-ws", self.addr);
            let (ws, _) = client_async(url, MaybeTlsStream::Plain(tcp))
                .await
                .map_err(|e| TransportError::Other(format!("ws: {e}")))?;
            let user_frame: UserFrameFn = Box::new(|session_id, text, msg_id, attachments| {
                user_message_frame(session_id, "device-1", text, msg_id, attachments)
            });
            Ok(Connection {
                ws,
                codec: Box::new(LoopbackCodec),
                user_frame,
            })
        })
    }
}

/// What the test pushes at the client.
enum ServerAction {
    Frame(Frame),
    Close(CloseFrame<'static>),
}

/// The gateway side of the loopback: serves connections (one at a time, so a
/// redial is picked up), mirrors every frame the client sends into a channel
/// the test drains, and — like the real gateway — answers each `Subscribe`
/// with the `SubscribeState` bundle that acknowledges it.
///
/// That ack is not decoration. `SessionRegistry::connect` now waits for it,
/// so a loopback that stayed silent would no longer be a fake gateway; it
/// would be a broken one, and every test here would sit out
/// `SUBSCRIBE_ACK_TIMEOUT`. `Server::silent` is the deliberate opposite,
/// used to test what happens when the ack never comes.
struct Server {
    addr: std::net::SocketAddr,
    inbound: mpsc::UnboundedReceiver<Frame>,
    outbound: mpsc::UnboundedSender<ServerAction>,
}

impl Server {
    async fn start() -> Self {
        Self::spawn(true).await
    }

    /// A gateway that takes the `Subscribe` and never acknowledges it.
    async fn silent() -> Self {
        Self::spawn(false).await
    }

    async fn spawn(auto_ack: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (inbound_tx, inbound) = mpsc::unbounded_channel();
        let (outbound, mut outbound_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let ws = accept_async(tcp).await.expect("ws handshake");
                let (mut sink, mut stream) = ws.split();
                loop {
                    tokio::select! {
                        message = stream.next() => {
                            let Some(Ok(Message::Binary(bytes))) = message else {
                                break;
                            };
                            let frame = decode(&bytes).expect("decode client frame");
                            if auto_ack
                                && let Frame::Subscribe { session_id } = &frame
                            {
                                let ack = encode(&subscribe_state(session_id.as_str()))
                                    .expect("encode ack");
                                if sink.send(Message::Binary(ack)).await.is_err() {
                                    break;
                                }
                            }
                            if inbound_tx.send(frame).is_err() {
                                return;
                            }
                        }
                        action = outbound_rx.recv() => {
                            match action {
                                Some(ServerAction::Frame(frame)) => {
                                    let bytes = encode(&frame).expect("encode server frame");
                                    if sink.send(Message::Binary(bytes)).await.is_err() {
                                        break;
                                    }
                                }
                                Some(ServerAction::Close(close)) => {
                                    let _ = sink.send(Message::Close(Some(close))).await;
                                    break;
                                }
                                None => return,
                            }
                        }
                    }
                }
            }
        });
        Self {
            addr,
            inbound,
            outbound,
        }
    }

    /// A registry dialing this server, plus the dial counter the tests
    /// that assert socket reuse read.
    fn registry(&self) -> (SessionRegistry, Arc<AtomicUsize>) {
        let dials = Arc::new(AtomicUsize::new(0));
        let registry = SessionRegistry::new(Arc::new(LoopbackDialer {
            addr: self.addr,
            dials: dials.clone(),
        }));
        (registry, dials)
    }

    /// The next `Frame` the client sent, failing rather than hanging if the
    /// socket goes quiet.
    async fn next_frame(&mut self) -> Frame {
        tokio::time::timeout(Duration::from_secs(5), self.inbound.recv())
            .await
            .expect("client sends a frame in time")
            .expect("server task is alive")
    }

    /// Whether the client sent anything at all within `window`.
    async fn stayed_quiet(&mut self, window: Duration) -> bool {
        tokio::time::timeout(window, self.inbound.recv())
            .await
            .is_err()
    }

    fn send(&self, frame: Frame) {
        self.outbound
            .send(ServerAction::Frame(frame))
            .expect("server task is alive");
    }

    fn close(&self, close: CloseFrame<'static>) {
        self.outbound
            .send(ServerAction::Close(close))
            .expect("server task is alive");
    }
}

/// The acknowledgement bundle the gateway sends the moment a `Subscribe`
/// registers (`crates/gateway/src/channel/route.rs`, `send_subscribe_state`).
fn subscribe_state(session_id: &str) -> Frame {
    Frame::SubscribeState {
        session_id: session_id.into(),
        as_of_ordinal: None,
        turn: wire::TurnSnapshot {
            active: false,
            started_at: None,
        },
        work_steps: Vec::new(),
        pending_approvals: Vec::new(),
        tasks: Vec::new(),
    }
}

/// Subscribe MUST precede the first Message on a draft session's leg — the
/// gateway drops a message for a session this connection isn't subscribed to,
/// and this file has shipped that regression before.
#[tokio::test]
async fn connect_and_send_puts_subscribe_before_the_message() {
    let mut server = Server::start().await;
    let (registry, _dials) = server.registry();
    let sink = Arc::new(RecordingSink::default());

    registry
        .connect_and_send(
            "s1",
            sink,
            OutboundMessage {
                text: "hello".to_string(),
                msg_id: "m1".to_string(),
                attachments: Vec::new(),
            },
        )
        .await
        .expect("connect and send");

    assert!(matches!(
        server.next_frame().await,
        Frame::Subscribe { session_id } if session_id.as_str() == "s1"
    ));
    match server.next_frame().await {
        Frame::Message(message) => {
            assert_eq!(message.session_id.as_str(), "s1");
            assert_eq!(message.content, "hello");
            assert_eq!(message.platform_msg_id, "m1");
        }
        other => panic!("expected the user message, got {other:?}"),
    }
}

/// `preconnect` warms the leg WITHOUT subscribing anything; the first Subscribe
/// is the one `connect` sends. The warmed socket is then REUSED — opening a
/// session must not redial (nor must switching between sessions, below).
#[tokio::test]
async fn preconnect_opens_the_leg_without_subscribing_a_session() {
    let mut server = Server::start().await;
    let (registry, dials) = server.registry();

    registry.preconnect().await.expect("preconnect");

    assert!(
        server.stayed_quiet(Duration::from_millis(200)).await,
        "preconnect must not send a Subscribe"
    );

    registry
        .connect("s1", Arc::new(RecordingSink::default()))
        .await
        .expect("connect");
    assert!(matches!(
        server.next_frame().await,
        Frame::Subscribe { session_id } if session_id.as_str() == "s1"
    ));
    assert_eq!(
        dials.load(Ordering::Relaxed),
        1,
        "opening a session must reuse the warmed leg, not redial"
    );
}

/// One leg carries many sessions: subscribing a second session sends another
/// Subscribe on the SAME socket and leaves the first subscription alone.
#[tokio::test]
async fn switching_sessions_reuses_the_one_leg() {
    let mut server = Server::start().await;
    let (registry, dials) = server.registry();

    registry
        .connect("s1", Arc::new(RecordingSink::default()))
        .await
        .expect("connect s1");
    registry
        .connect("s2", Arc::new(RecordingSink::default()))
        .await
        .expect("connect s2");

    let subscribed: Vec<String> = vec![server.next_frame().await, server.next_frame().await]
        .into_iter()
        .map(|frame| match frame {
            Frame::Subscribe { session_id } => session_id.as_str().to_owned(),
            other => panic!("expected a Subscribe, got {other:?}"),
        })
        .collect();

    assert_eq!(subscribed, ["s1", "s2"]);
    assert_eq!(
        dials.load(Ordering::Relaxed),
        1,
        "a second session must not open a second socket"
    );
}

/// The gateway's application keepalive is answered locally and NEVER forwarded:
/// a `Ping` reaching the transcript would render as an unknown frame.
#[tokio::test]
async fn a_ping_is_answered_with_a_pong_and_never_forwarded_to_a_sink() {
    let mut server = Server::start().await;
    let (registry, _dials) = server.registry();
    let sink = Arc::new(RecordingSink::default());

    registry.connect("s1", sink.clone()).await.expect("connect");
    assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));

    server.send(Frame::Ping);

    assert!(matches!(server.next_frame().await, Frame::Pong));
    // The subscribe ack DOES reach the sink (the transcript replaces its
    // state from it); the keepalive must not.
    assert_eq!(sink.kinds(), ["subscribe_state"]);
}

/// A deliberate teardown aborts the pump BEFORE it can report death, so logout
/// doesn't kick the reconnect ladder against credentials that were just wiped.
#[tokio::test]
async fn a_deliberate_disconnect_never_fires_on_disconnected() {
    let mut server = Server::start().await;
    let (registry, _dials) = server.registry();
    let sink = Arc::new(RecordingSink::default());

    registry.connect("s1", sink.clone()).await.expect("connect");
    assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));

    registry.disconnect().await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        sink.disconnects().is_empty(),
        "a deliberate teardown must not look like an unsolicited drop"
    );
}

/// The other half of that contract: an unsolicited drop (the peer closing) MUST
/// fire `on_disconnected` for every subscribed session — that is what arms the
/// reconnect ladder.
#[tokio::test]
async fn an_unsolicited_close_fires_on_disconnected_for_every_session() {
    let mut server = Server::start().await;
    let (registry, _dials) = server.registry();
    let first = Arc::new(RecordingSink::default());
    let second = Arc::new(RecordingSink::default());

    registry
        .connect("s1", first.clone())
        .await
        .expect("connect s1");
    registry
        .connect("s2", second.clone())
        .await
        .expect("connect s2");
    assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));
    assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));

    server.close(CloseFrame {
        code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Away,
        reason: "bye".into(),
    });

    for (sink, session_id) in [(first, "s1"), (second, "s2")] {
        let mut waited = Duration::ZERO;
        while sink.disconnects().is_empty() && waited < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(20)).await;
            waited += Duration::from_millis(20);
        }
        assert_eq!(sink.disconnects(), [session_id.to_string()]);
    }
}

/// The cold-start bug this ack exists for: enqueueing a `Subscribe` proves
/// only that a process-local channel accepted it. A leg that is a black hole
/// — established, never aborted, silently carrying nothing — used to leave
/// `connect` returning `Ok`, the UI saying "connected", and nothing noticing
/// until `INBOUND_LIVENESS_TIMEOUT` 45s later. It must fail instead.
/// The ack budget for the two tests whose point is that the budget EXPIRES.
/// Nothing races it — the server is silent, so however slow the box is, the
/// timeout is still what fires — which is why it can be this short.
///
/// Real time, not a paused clock: these dial a real loopback socket, and
/// `start_paused` fast-forwards straight through a pending TCP connect — far
/// enough to trip `CONNECT_TIMEOUT` before the dial lands.
const TEST_ACK_BUDGET: Duration = Duration::from_millis(300);

/// The budget for the one test that needs traffic to BEAT it. A loopback
/// round trip is microseconds, so this is ~4 orders of magnitude of headroom
/// — the test must not turn into a stopwatch on a loaded runner.
const TEST_ACK_BUDGET_BEATABLE: Duration = Duration::from_secs(2);

#[tokio::test]
async fn an_unacknowledged_subscribe_fails_instead_of_reporting_connected() {
    let mut server = Server::silent().await;
    let (registry, _dials) = server.registry();
    let registry = registry.with_subscribe_ack_timeout(TEST_ACK_BUDGET);

    let err = registry
        .connect("s1", Arc::new(RecordingSink::default()))
        .await
        .expect_err("an unacknowledged subscribe is not a connection");

    assert!(matches!(err, TransportError::SessionClosed), "got {err:?}");
    assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));
}

/// …and the silent leg is RETIRED, not merely failed: every other session on
/// it is subscribed on a socket that carries nothing, so each one has to be
/// told to redial through the death transition.
#[tokio::test]
async fn a_silent_leg_is_retired_so_every_session_on_it_redials() {
    let server = Server::silent().await;
    let (registry, _dials) = server.registry();
    let registry = registry.with_subscribe_ack_timeout(TEST_ACK_BUDGET);
    let bystander = Arc::new(RecordingSink::default());

    // `s2` is already riding the leg when `s1`'s subscribe goes unanswered.
    registry.preconnect().await.expect("preconnect");
    ride_leg(&registry, "s2", bystander.clone()).await;

    registry
        .connect("s1", Arc::new(RecordingSink::default()))
        .await
        .expect_err("unacknowledged");

    let mut waited = Duration::ZERO;
    while bystander.disconnects().is_empty() && waited < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += Duration::from_millis(20);
    }
    assert_eq!(
        bystander.disconnects(),
        ["s2".to_string()],
        "a bystander on the retired leg must be told to redial"
    );
}

/// The other side of that judgement call: when the leg is demonstrably
/// carrying traffic, an unanswered `Subscribe` is this SESSION's problem (a
/// cross-channel session, which the gateway answers with a `Notice` and no
/// bundle). Tearing the leg down there would put every other session through
/// a redial on its behalf, forever.
#[tokio::test]
async fn a_live_leg_survives_one_sessions_unanswered_subscribe() {
    let mut server = Server::silent().await;
    let (registry, dials) = server.registry();
    let registry = registry.with_subscribe_ack_timeout(TEST_ACK_BUDGET_BEATABLE);

    registry.preconnect().await.expect("preconnect");
    let connect = {
        let registry = &registry;
        async move {
            registry
                .connect("s1", Arc::new(RecordingSink::default()))
                .await
        }
    };
    let traffic = async {
        assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));
        // Not the ack — just proof the socket is alive. The Pong is the
        // client confirming it processed the Ping, which is the same poll
        // that restamps `last_inbound`; waiting for it makes the test assert
        // on a fact rather than on a sleep.
        server.send(Frame::Ping);
        assert!(matches!(server.next_frame().await, Frame::Pong));
        tokio::time::sleep(TEST_ACK_BUDGET_BEATABLE).await;
    };
    let (result, ()) = tokio::join!(connect, traffic);

    assert!(
        matches!(result, Err(TransportError::NotConnected)),
        "got {result:?}"
    );
    // The spared leg is still the live one: a later probe reuses it
    // instead of redialing.
    registry.preconnect().await.expect("preconnect probe");
    assert_eq!(
        dials.load(Ordering::Relaxed),
        1,
        "a leg that is delivering must survive one session's unanswered subscribe"
    );
}

/// `HANDSHAKE_REPLY_TIMEOUT` belongs to the handshake CALL SITE, never to
/// the shared reader. `recv_binary` is also `relay::tunnel::NoiseFrames::recv`
/// — every REST-over-relay response frame and every blob chunk — where the
/// budgets are `POOLED_LEG_FIRST_BYTE_TIMEOUT` (15s), `TUNNEL_REQUEST_TIMEOUT`
/// (30s) and `TUNNEL_HANDSHAKE_TIMEOUT` (15s), all wider than 6s, plus a
/// 100 MiB upload's post-transfer wait which is deliberately uncapped. Bound
/// the shared reader and you silently cap all of them.
///
/// The tunnel's own tests drive a `ScriptedFrames` fake and never reach
/// `recv_binary`, so this is the only place the separation can be seen.
/// Paused clock is safe here (unlike the registry tests): nothing in this
/// test wraps a dial in a timeout, so there is no budget for the
/// auto-advance to consume before the socket is up. A server apiece —
/// `Server` serves one connection at a time, so a second dial into the same
/// one would never be accepted.
#[tokio::test(start_paused = true)]
async fn only_the_handshake_reader_is_time_bounded() {
    let handshake_server = Server::silent().await;
    let mut ws = dial_raw(handshake_server.addr).await;
    assert!(
        tokio::time::timeout(HANDSHAKE_REPLY_TIMEOUT * 2, recv_binary_handshake(&mut ws))
            .await
            .expect("the handshake reader must give up on its own")
            .is_err(),
        "a peer that accepts and never answers must fail the handshake"
    );

    let tunnel_server = Server::silent().await;
    let mut ws = dial_raw(tunnel_server.addr).await;
    assert!(
        tokio::time::timeout(HANDSHAKE_REPLY_TIMEOUT * 2, recv_binary(&mut ws))
            .await
            .is_err(),
        "the shared reader must still be waiting — a tunnel response or blob \
             chunk is allowed to take longer than a handshake"
    );
}

/// A bare client socket, no registry or pump — the two readers under test
/// take a `WsStream` directly.
async fn dial_raw(addr: std::net::SocketAddr) -> WsStream {
    let tcp = TcpStream::connect(addr).await.expect("tcp");
    let (ws, _) = client_async(
        format!("ws://{addr}/v1/channel-ws"),
        MaybeTlsStream::Plain(tcp),
    )
    .await
    .expect("ws");
    ws
}

/// `send` refuses a session with no registered sink rather than writing into a
/// leg nobody is listening on.
#[tokio::test]
async fn a_send_without_a_subscribed_sink_is_refused() {
    let server = Server::start().await;
    let (registry, _dials) = server.registry();

    let err = registry
        .send(
            "s1".to_string(),
            "hi".to_string(),
            "m1".to_string(),
            Vec::new(),
        )
        .await
        .expect_err("must refuse");
    assert!(matches!(err, TransportError::NotConnected));
}

/// Model a session already riding the current leg without running a
/// `connect` (the server may be deliberately silent): install it as Proven
/// on the live leg through the supervisor's test seam.
async fn ride_leg(registry: &SessionRegistry, session_id: &str, sink: Arc<dyn FrameSink>) {
    let (tx, rx) = oneshot::channel();
    registry
        .supervisor()
        .send(Msg::InjectProvenForTest {
            session_id: session_id.to_string(),
            sink,
            reply: tx,
        })
        .expect("supervisor is alive");
    rx.await
        .expect("supervisor replies")
        .expect("a live leg to ride");
}

/// Abort the live pump so it can neither pump nor report its own death —
/// the corpse shape a panicked pump (or one aborted mid-poll) leaves
/// behind, which only the OTHER discovery channels can find.
async fn kill_pump_silently(registry: &SessionRegistry) {
    let (tx, rx) = oneshot::channel();
    registry
        .supervisor()
        .send(Msg::AbortPumpForTest { reply: tx })
        .expect("supervisor is alive");
    rx.await.expect("supervisor replies").expect("abort");
}

/// The happy path of the send gate: a send on the very leg that proved the
/// subscription is admitted and reaches the wire. (The gate exists to refuse
/// stale legs — this pins that it never over-refuses the live one.)
#[tokio::test]
async fn a_send_on_the_leg_that_proved_the_subscription_is_admitted() {
    let mut server = Server::start().await;
    let (registry, _dials) = server.registry();

    registry
        .connect("s1", Arc::new(RecordingSink::default()))
        .await
        .expect("connect");
    assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));

    registry
        .send(
            "s1".to_string(),
            "hi".to_string(),
            "m1".to_string(),
            Vec::new(),
        )
        .await
        .expect("send on the proven leg");

    match server.next_frame().await {
        Frame::Message(message) => assert_eq!(message.platform_msg_id, "m1"),
        other => panic!("expected the user message, got {other:?}"),
    }
}

/// The cold-start black hole of 2026-08-16: the old leg dies, a foreground
/// `preconnect` installs a fresh leg that subscribes NOTHING, and a send for
/// the still-sink-holding session used to return `Ok` while the gateway
/// silently dropped it as not-subscribed. The send gate must refuse instead,
/// so the client redials (subscribe + send) rather than spinning forever.
#[tokio::test]
async fn a_send_on_a_fresh_unsubscribed_leg_is_refused_not_black_holed() {
    let mut server = Server::start().await;
    let (registry, _dials) = server.registry();
    let sink = Arc::new(RecordingSink::default());

    registry.connect("s1", sink.clone()).await.expect("connect");
    assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));

    // The leg dies out from under the session…
    server.close(CloseFrame {
        code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Away,
        reason: "gone".into(),
    });
    let mut waited = Duration::ZERO;
    while sink.disconnects().is_empty() && waited < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += Duration::from_millis(20);
    }
    assert_eq!(sink.disconnects(), ["s1".to_string()]);

    // …and a foreground probe warms a replacement that carries no
    // subscriptions.
    registry.preconnect().await.expect("preconnect");
    assert!(
        server.stayed_quiet(Duration::from_millis(200)).await,
        "preconnect must not subscribe anything"
    );

    let err = registry
        .send(
            "s1".to_string(),
            "hi".to_string(),
            "m1".to_string(),
            Vec::new(),
        )
        .await
        .expect_err("a send on a leg that never saw this session's Subscribe");
    assert!(matches!(err, TransportError::NotConnected), "got {err:?}");
    assert!(
        server.stayed_quiet(Duration::from_millis(200)).await,
        "the refused send must never reach the wire"
    );
}

/// Death is handled exactly once however many channels report it: after
/// the pump's own tail already announced the death, a probe walking over
/// the same corpse must not re-deliver `on_disconnected` (the late report
/// no-ops on the leg-id mismatch).
#[tokio::test]
async fn a_duplicate_death_report_is_not_redelivered() {
    let mut server = Server::start().await;
    let (registry, _dials) = server.registry();
    let sink = Arc::new(RecordingSink::default());

    registry.connect("s1", sink.clone()).await.expect("connect");
    assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));

    server.close(CloseFrame {
        code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Away,
        reason: "gone".into(),
    });
    let mut waited = Duration::ZERO;
    while sink.disconnects().is_empty() && waited < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += Duration::from_millis(20);
    }

    // The probe redials a fresh leg; the old death must not echo again.
    registry.preconnect().await.expect("preconnect");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        sink.disconnects(),
        ["s1".to_string()],
        "exactly one on_disconnected across every discovery channel"
    );
}

/// The corpse a pump leaves when it dies WITHOUT reporting (a panic, or an
/// abort landing mid-poll): the enqueue-failure discovery channel must
/// still deliver `on_disconnected` — a silently-swallowed death is the
/// cold-start black hole's client half.
#[tokio::test]
async fn a_corpse_found_by_a_send_still_tells_the_riders() {
    let mut server = Server::start().await;
    let (registry, _dials) = server.registry();
    let sink = Arc::new(RecordingSink::default());

    registry.connect("s1", sink.clone()).await.expect("connect");
    assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));

    kill_pump_silently(&registry).await;

    let err = registry
        .send(
            "s1".to_string(),
            "hi".to_string(),
            "m1".to_string(),
            Vec::new(),
        )
        .await
        .expect_err("the enqueue must discover the corpse");
    assert!(matches!(err, TransportError::NotConnected), "got {err:?}");
    let mut waited = Duration::ZERO;
    while sink.disconnects().is_empty() && waited < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += Duration::from_millis(20);
    }
    assert_eq!(
        sink.disconnects(),
        ["s1".to_string()],
        "a death only a failed enqueue can see must still be announced"
    );
}

/// The death fan-out is targeted: a session mid-redial has already been
/// withdrawn from the dying leg by its own open, so the death its open
/// discovers is announced to the OTHER riders and never to the dial it is
/// about to prove — a spurious report would make the client distrust a
/// perfectly good reconnect.
#[tokio::test]
async fn a_death_found_by_a_reopen_spares_the_reopening_session() {
    let mut server = Server::start().await;
    let (registry, _dials) = server.registry();
    let first_sink = Arc::new(RecordingSink::default());
    let bystander = Arc::new(RecordingSink::default());

    registry
        .connect("s1", first_sink.clone())
        .await
        .expect("connect s1");
    assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));
    registry
        .connect("s2", bystander.clone())
        .await
        .expect("connect s2");
    assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));

    kill_pump_silently(&registry).await;

    // s1 reopens; its open discovers the corpse, which kicks s2's redial
    // but not s1's own fresh dial.
    let second_sink = Arc::new(RecordingSink::default());
    registry
        .connect("s1", second_sink.clone())
        .await
        .expect("reopen s1 over the corpse");
    assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));

    let mut waited = Duration::ZERO;
    while bystander.disconnects().is_empty() && waited < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += Duration::from_millis(20);
    }
    assert_eq!(bystander.disconnects(), ["s2".to_string()]);
    assert!(
        first_sink.disconnects().is_empty() && second_sink.disconnects().is_empty(),
        "the reopening session must not hear the death its own open discovered"
    );
}

/// The ack-failure withdrawal: a subscribe the gateway never acknowledged
/// must leave NO leg binding behind, or the send gate would `Ok` sends
/// onto a live leg whose gateway subscription does not exist — the silent
/// server-side drop again.
#[tokio::test]
async fn a_send_after_an_unacknowledged_subscribe_is_refused() {
    let mut server = Server::silent().await;
    let (registry, _dials) = server.registry();
    let registry = registry.with_subscribe_ack_timeout(TEST_ACK_BUDGET_BEATABLE);

    // Keep the leg alive across the failed connect so the retire branch
    // doesn't fire — this is the "leg is live; not touching it" path.
    registry.preconnect().await.expect("preconnect");
    let connect = {
        let registry = &registry;
        async move {
            registry
                .connect("s1", Arc::new(RecordingSink::default()))
                .await
        }
    };
    let traffic = async {
        assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));
        server.send(Frame::Ping);
        assert!(matches!(server.next_frame().await, Frame::Pong));
        tokio::time::sleep(TEST_ACK_BUDGET_BEATABLE).await;
    };
    let (result, ()) = tokio::join!(connect, traffic);
    assert!(result.is_err(), "the subscribe was never acknowledged");

    let err = registry
        .send(
            "s1".to_string(),
            "hi".to_string(),
            "m1".to_string(),
            Vec::new(),
        )
        .await
        .expect_err("no proven subscription to send on");
    assert!(matches!(err, TransportError::NotConnected), "got {err:?}");
    assert!(
        server.stayed_quiet(Duration::from_millis(200)).await,
        "the refused send must never reach the wire"
    );
}

/// A dialer whose establish panics — the foreign dial stack has panicked
/// before (a mis-featured rustls once panicked building its ClientConfig).
struct PanickingDialer;

impl LegDialer for PanickingDialer {
    fn establish(&self) -> futures_util::future::BoxFuture<'_, Result<Connection, TransportError>> {
        Box::pin(async { panic!("dial stack exploded") })
    }
}

/// `DialFinished` is the only exit from `Dialing` and its waiters' replies
/// are held in the leg — so a dial child that dies without reporting must
/// still produce the message (the send-on-drop report), or every parked
/// and future connect hangs until app relaunch.
#[tokio::test]
async fn a_panicking_dial_child_fails_the_open_instead_of_hanging_it() {
    let registry = SessionRegistry::new(Arc::new(PanickingDialer));

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        registry.connect("s1", Arc::new(RecordingSink::default())),
    )
    .await
    .expect("the open must resolve, not hang");
    assert!(result.is_err(), "a panicked dial is a failed dial");

    // And the leg exited `Dialing`: the next operation gets its own
    // (failing) dial instead of parking forever behind the corpse.
    let again = tokio::time::timeout(Duration::from_secs(5), registry.preconnect())
        .await
        .expect("the follow-up must resolve, not hang");
    assert!(again.is_err());
}

/// A dialer that fails every attempt; the first attempt parks on a gate so
/// a test can arrange latecomers deterministically.
struct FailingDialer {
    dials: Arc<AtomicUsize>,
    first_gate: parking_lot::Mutex<Option<oneshot::Receiver<()>>>,
}

impl LegDialer for FailingDialer {
    fn establish(&self) -> futures_util::future::BoxFuture<'_, Result<Connection, TransportError>> {
        Box::pin(async move {
            self.dials.fetch_add(1, Ordering::Relaxed);
            let gate = self.first_gate.lock().take();
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            Err(TransportError::Other("dial refused".into()))
        })
    }
}

/// The per-batch retry contract: opens arriving DURING a failed dial share
/// exactly ONE fresh attempt (adopters of the retry), and no more —
/// recovery beyond that is the client's redial ladder.
#[tokio::test]
async fn latecomers_of_a_failed_dial_share_one_fresh_dial() {
    let (gate_tx, gate_rx) = oneshot::channel();
    let dials = Arc::new(AtomicUsize::new(0));
    let registry = Arc::new(SessionRegistry::new(Arc::new(FailingDialer {
        dials: dials.clone(),
        first_gate: parking_lot::Mutex::new(Some(gate_rx)),
    })));

    let adopter = tokio::spawn({
        let registry = registry.clone();
        async move {
            registry
                .connect("s1", Arc::new(RecordingSink::default()))
                .await
        }
    });
    let mut waited = Duration::ZERO;
    while dials.load(Ordering::Relaxed) == 0 && waited < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(5)).await;
        waited += Duration::from_millis(5);
    }
    let latecomer = tokio::spawn({
        let registry = registry.clone();
        async move {
            registry
                .connect("s2", Arc::new(RecordingSink::default()))
                .await
        }
    });
    // Let the latecomer's Open park on the in-flight dial before it fails.
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate_tx
        .send(())
        .expect("the first dial is parked on the gate");

    let adopter = adopter.await.expect("adopter task");
    let latecomer = latecomer.await.expect("latecomer task");
    assert!(
        adopter.is_err(),
        "the adopter shares the first dial's failure"
    );
    assert!(
        latecomer.is_err(),
        "the latecomer shares the retry's failure"
    );
    assert_eq!(
        dials.load(Ordering::Relaxed),
        2,
        "the latecomer batch gets exactly one fresh dial"
    );
}
