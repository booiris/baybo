//! Background jobs as a capability a tool composes in, instead of
//! re-implementing.
//!
//! A tool that can hand slow work to the background declares a
//! [`BackgroundSupport`] from [`crate::Tool::background_support`]. Everything
//! that is the same for every such tool lives here, once:
//!
//! - **The options.** [`declare_options`] adds `timeout_ms` and `on_timeout`
//!   (and, where the tool can start work detached, `background` and `group`)
//!   to the tool's schema; the registry strips the background ones again
//!   wherever the session cannot run background work ([`withhold_options`]),
//!   so the model is never offered a choice the runtime would refuse.
//! - **Their parsing.** [`requested_run_time`] and
//!   [`BackgroundRequest::parse`] are the one reading of those options,
//!   strict for every tool.
//! - **The lifecycle.** [`run`] owns the foreground run time: it waits on the
//!   work, stops it with the turn, and on overrun either kills it or hands it
//!   to the [`BackgroundJobSink`] — which registers it, escorts it to its end
//!   and delivers the result as a notification turn. It also writes the
//!   "moved to the background" answer the model gets back ([`ack`]).
//!
//! What stays with the tool is what genuinely differs: how its work is run
//! so it can outlive the turn (a [`Job`]'s `terminal` future, which must not
//! depend on anything the turn owns), how it is cancelled (the job's
//! `cancel` token), and what its result says (`into_result`, plus however
//! the tool renders a [`Ran`] that finished in the foreground).

use std::time::Duration;

use async_trait::async_trait;
use baybo_model::{
    BACKGROUND_DISPATCH_YIELD_GUIDANCE, BackgroundGroupName, BackgroundHandle, CohortKey,
    OnTimeout, PendingBackgroundResult, SessionId,
};
use futures::FutureExt;
use futures::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::ToolContext;

/// Parameter that chooses between detaching and killing a call that
/// outlasts its foreground window.
pub const ON_TIMEOUT_PARAM: &str = "on_timeout";
/// Parameter that starts the work detached straight away.
pub const BACKGROUND_PARAM: &str = "background";
/// Parameter naming a barrier cohort: detached calls sharing it within one
/// turn deliver one merged notification.
pub const GROUP_PARAM: &str = "group";

const ON_TIMEOUT_BACKGROUND: &str = "background";
const ON_TIMEOUT_KILL: &str = "kill";

/// Parameter bounding how long the call runs in the foreground, in
/// milliseconds.
pub const TIMEOUT_PARAM: &str = "timeout_ms";
/// A call's run time when it names none, and the longest it waits in the
/// foreground before it may move to the background.
pub const FOREGROUND_WINDOW: Duration = Duration::from_millis(FOREGROUND_WINDOW_MS);
/// The longest a call may ask to run in the foreground.
pub const MAX_RUN_TIME: Duration = Duration::from_millis(MAX_RUN_TIME_MS);

const FOREGROUND_WINDOW_MS: u64 = 10 * 60 * 1000;
const MAX_RUN_TIME_MS: u64 = 60 * 60 * 1000;

/// A tool's declaration that it can run work past the turn that started it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackgroundSupport {
    /// Whether the tool accepts [`BACKGROUND_PARAM`] and [`GROUP_PARAM`]:
    /// starting detached at once, alone or as a merged cohort. Only work that
    /// can be handed over before it has produced anything qualifies.
    pub start_detached: bool,
}

impl BackgroundSupport {
    fn background_option_names(&self) -> &'static [&'static str] {
        if self.start_detached {
            &[ON_TIMEOUT_PARAM, BACKGROUND_PARAM, GROUP_PARAM]
        } else {
            &[ON_TIMEOUT_PARAM]
        }
    }
}

/// `schema` plus `timeout_ms` and the background options `support` accepts.
/// The tool's declared surface — what the manifest records and the executor
/// validates against — always carries them; [`withhold_options`] narrows only
/// what a session is offered.
pub fn declare_options(mut schema: Value, support: &BackgroundSupport) -> Value {
    let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) else {
        return schema;
    };
    insert_options(props, support);
    schema
}

fn timeout_option(description: String) -> Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "maximum": MAX_RUN_TIME_MS,
        "description": description,
    })
}

fn run_time_bounds() -> String {
    format!(
        "Default (and `0`): {FOREGROUND_WINDOW_MS} (10 minutes); at most {MAX_RUN_TIME_MS} (1 hour)."
    )
}

fn insert_options(props: &mut Map<String, Value>, support: &BackgroundSupport) {
    props.insert(
        TIMEOUT_PARAM.to_string(),
        timeout_option(format!(
            "Milliseconds the call may run. {} With on_timeout='background' it moves to the background once this elapses, and no later than 10 minutes in.",
            run_time_bounds()
        )),
    );
    let ignored_when_detached = if support.start_detached {
        " Ignored when background=true."
    } else {
        ""
    };
    props.insert(
        ON_TIMEOUT_PARAM.to_string(),
        json!({
            "type": "string",
            "enum": [ON_TIMEOUT_BACKGROUND, ON_TIMEOUT_KILL],
            "default": ON_TIMEOUT_BACKGROUND,
            "description": format!(
                "For a call still running when `{TIMEOUT_PARAM}` elapses: 'background' (default) detaches it — handle now, result in a later turn — and 'kill' terminates it.{ignored_when_detached}"
            ),
        }),
    );
    if support.start_detached {
        props.insert(
            BACKGROUND_PARAM.to_string(),
            json!({
                "type": "boolean",
                "description": "Return a handle immediately instead of blocking; the result arrives in a later turn.",
            }),
        );
        props.insert(
            GROUP_PARAM.to_string(),
            json!({
                "type": "string",
                "description": "Barrier cohort name. Calls sharing a group, made in one turn, run in parallel and deliver ONE merged notification once they ALL finish, instead of one each. A group always runs in the background.",
            }),
        );
    }
}

/// Narrow the options `support` declared for a session that cannot run
/// background work: the background ones go — each only chooses *how* to
/// background, a choice the runtime has already made — and `timeout_ms`
/// says the call is terminated when it elapses.
pub(crate) fn withhold_options(schema: &mut Value, support: &BackgroundSupport) {
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        for name in support.background_option_names() {
            props.remove(*name);
        }
        props.insert(
            TIMEOUT_PARAM.to_string(),
            timeout_option(format!(
                "Milliseconds the call may run before it is terminated. {}",
                run_time_bounds()
            )),
        );
    }
}

/// How long the call in `params` may run in the foreground: its `timeout_ms`,
/// [`FOREGROUND_WINDOW`] when it names none (or `0`), never more than
/// [`MAX_RUN_TIME`] — nor than the context's own budget
/// ([`ToolContext::timeout`]), which is what keeps a test context fast.
pub fn requested_run_time(params: &Value, ctx: &ToolContext) -> Result<Duration, String> {
    let requested = match params.get(TIMEOUT_PARAM) {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_u64() {
            Some(0) => None,
            Some(ms) => Some(Duration::from_millis(ms)),
            None => {
                return Err(format!(
                    "{TIMEOUT_PARAM} must be a non-negative integer, got {value}"
                ));
            }
        },
    };
    Ok(requested
        .unwrap_or(FOREGROUND_WINDOW)
        .min(MAX_RUN_TIME)
        .min(ctx.timeout))
}

/// What one call asked for, as the tool's caller sent it.
///
/// Parsed even where the session cannot run background work — a withheld
/// option sent anyway still parses — and resolved against eligibility by
/// [`run`]. A tool may skip `run` for a cheaper blocking path only where `run`
/// could not hand the work over anyway: when
/// [`ToolContext::can_hand_over_background`] is false, or the call asked for
/// `on_timeout = kill`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundRequest {
    /// Wait out the foreground window, then do what `on_timeout` says.
    Wait(OnTimeout),
    /// Hand the work over at once (`background: true`, or a `group`, which
    /// implies it). `group` is the barrier cohort, trimmed; blank is none.
    Detach { group: Option<BackgroundGroupName> },
}

impl Default for BackgroundRequest {
    fn default() -> Self {
        Self::Wait(OnTimeout::Background)
    }
}

impl BackgroundRequest {
    /// Read the background options `support` declares out of a call's
    /// parameters. Options the tool does not declare are not its business and
    /// are left unread, whatever their shape.
    ///
    /// Strict: an `on_timeout` other than `background` / `kill` (trimmed, any
    /// case, empty meaning the default) is an error, not a silent default — a
    /// misspelt `kill` must not detach a command the caller wanted dead.
    pub fn parse(params: &Value, support: &BackgroundSupport) -> Result<Self, String> {
        let on_timeout = match params.get(ON_TIMEOUT_PARAM) {
            None | Some(Value::Null) => OnTimeout::Background,
            Some(Value::String(raw)) => match raw.trim() {
                "" => OnTimeout::Background,
                value if value.eq_ignore_ascii_case(ON_TIMEOUT_BACKGROUND) => OnTimeout::Background,
                value if value.eq_ignore_ascii_case(ON_TIMEOUT_KILL) => OnTimeout::Kill,
                other => {
                    return Err(format!(
                        "unknown {ON_TIMEOUT_PARAM} {other:?}; expected {ON_TIMEOUT_BACKGROUND}|{ON_TIMEOUT_KILL}"
                    ));
                }
            },
            Some(other) => return Err(format!("{ON_TIMEOUT_PARAM} must be a string, got {other}")),
        };
        if !support.start_detached {
            return Ok(Self::Wait(on_timeout));
        }
        let group = match params.get(GROUP_PARAM) {
            None | Some(Value::Null) => None,
            Some(Value::String(raw)) => BackgroundGroupName::parse(raw),
            Some(other) => return Err(format!("{GROUP_PARAM} must be a string, got {other}")),
        };
        let background = match params.get(BACKGROUND_PARAM) {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(other) => {
                return Err(format!("{BACKGROUND_PARAM} must be a boolean, got {other}"));
            }
        };
        Ok(if background || group.is_some() {
            Self::Detach { group }
        } else {
            Self::Wait(on_timeout)
        })
    }
}

/// Work a tool hands to [`run`]: something that keeps going until it is done
/// or `cancel` fires, whether or not the turn that started it is still there.
pub struct Job<T> {
    /// The id the model, `JobList` / `JobStop` and the eventual notification
    /// share. Minted by the tool with [`BackgroundHandle::new`],
    /// because a tool may need it before the work starts (Bash names its
    /// output files after it).
    pub handle: BackgroundHandle,
    /// What kind of job this is, as `JobList` and `/stop` name it: a subagent
    /// profile, or `"command"`.
    pub kind: String,
    /// What it is doing. May be a raw command line, so it is shown to the user
    /// but never logged.
    pub summary: String,
    /// The session the work itself runs in, when it has one (a subagent's
    /// child), so `/stop` can stamp that session's turn as user-stopped.
    pub session: Option<SessionId>,
    /// Tripping it must make `terminal` resolve promptly.
    pub cancel: CancellationToken,
    /// The work, run to completion. Must not borrow from or be cancelled by
    /// the turn: [`run`] stops it through `cancel` when it has to.
    pub terminal: BoxFuture<'static, T>,
    /// The notification-ready result, for a job that finished in the
    /// background.
    pub into_result: Box<dyn FnOnce(T) -> BoxFuture<'static, PendingBackgroundResult> + Send>,
}

/// How a [`Job`] run through [`run`] ended, from the tool call's side.
pub enum Ran<T> {
    /// It finished in the foreground, within its run time.
    Finished(T),
    /// The turn was stopped while it ran in the foreground; it was cancelled
    /// and drained.
    Cancelled(T),
    /// It outlasted its run time where it could not move to the background
    /// (`on_timeout = kill`, a session or turn that cannot take background
    /// work); it was cancelled and drained.
    TimedOut(T),
    /// It runs on in the background; tell the model with [`ack`].
    Backgrounded(Backgrounded),
}

/// A job that now runs in the background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backgrounded {
    pub handle: BackgroundHandle,
    pub how: BackgroundedHow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundedHow {
    /// Started detached, because the call asked for `background` or a group.
    Dispatched,
    /// Still running when its foreground window of this length elapsed.
    Converted(Duration),
}

/// A job handed to the runtime to escort past its turn.
pub struct DetachedJob {
    pub handle: BackgroundHandle,
    /// The session whose actor receives the result.
    pub parent: SessionId,
    pub kind: String,
    pub summary: String,
    pub session: Option<SessionId>,
    /// The cohort the result is held under, for a job started as part of a
    /// group.
    pub cohort: Option<CohortKey>,
    pub cancel: CancellationToken,
    pub result: BoxFuture<'static, PendingBackgroundResult>,
}

/// Takes a [`DetachedJob`] off a tool call: registers it in the session's
/// in-flight set (so `JobList`, `JobStop` and `/stop` reach it and the idle
/// reaper leaves the session alone), escorts it to its end, and delivers its
/// result unless it was stopped. Implemented by the agent layer and injected
/// through [`ToolContext::background_jobs`] — a capability handle, never the
/// policy: whether a call may use it is [`ToolContext::background_eligible`].
#[async_trait]
pub trait BackgroundJobSink: Send + Sync {
    /// Register `job` and start escorting it. Returns once it is registered,
    /// without waiting for the work. Must not await between registering the
    /// job and taking ownership of it: the caller may be dropped at any await.
    async fn adopt(&self, job: DetachedJob);
}

/// A job a call handed to the background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandedOver {
    pub handle: BackgroundHandle,
    /// The group it was started under, if it was part of one.
    pub group: Option<BackgroundGroupName>,
}

/// What the executor learns about the job a call handed to the background:
/// the agent loop counts a grouped one into its cohort, and records every one
/// as started by its turn. Filled by [`run`], read once the call returns.
#[derive(Debug, Default)]
pub struct DispatchRecord(parking_lot::Mutex<Option<HandedOver>>);

impl DispatchRecord {
    /// The job this call handed to the background, if it handed one over.
    pub fn take(&self) -> Option<HandedOver> {
        self.0.lock().take()
    }

    fn record(&self, handed_over: HandedOver) {
        *self.0.lock() = Some(handed_over);
    }
}

/// Run `job` under the call's background options.
///
/// - The call asked to start detached, and the session can take background
///   work: hand it over at once.
/// - Otherwise run it in the foreground for up to `run_time` (from
///   [`requested_run_time`]). Where [`ToolContext::overrun_moves_to_background`]
///   says a job still running then moves to the background, the wait is also
///   capped at [`FOREGROUND_WINDOW`] and the job is handed over; otherwise it is
///   cancelled once `run_time` elapses.
///
/// The turn's own cancellation reaches the job only while it runs in the
/// foreground — once handed over it is stopped through `JobStop` / `/stop`.
///
/// The work runs on its own task from the start, so it always reaches its
/// end — its cleanup included — even when the caller stops polling: the agent
/// loop drops an unfinished tool call when its turn is stopped. Until the job
/// is handed over, dropping this future cancels it.
pub async fn run<T: Send + 'static>(
    ctx: &ToolContext,
    request: &BackgroundRequest,
    run_time: Duration,
    job: Job<T>,
) -> Ran<T> {
    let Job {
        handle,
        kind,
        summary,
        session,
        cancel,
        terminal,
        into_result,
    } = job;
    let stop_if_abandoned = cancel.clone().drop_guard();
    let mut work: BoxFuture<'static, T> = tokio::spawn(terminal).map(joined).boxed();
    let detached = |work: BoxFuture<'static, T>, cohort| DetachedJob {
        handle: handle.clone(),
        parent: ctx.session_id.clone(),
        kind,
        summary,
        session,
        cohort,
        cancel: cancel.clone(),
        result: work.then(into_result).boxed(),
    };

    let sink = ctx
        .background_jobs
        .as_ref()
        .filter(|_| ctx.can_hand_over_background());
    if let (Some(sink), BackgroundRequest::Detach { group }) = (sink, request) {
        let cohort = group.as_ref().map(|g| CohortKey::new(ctx.turn_id, g));
        sink.adopt(detached(work, cohort)).await;
        stop_if_abandoned.disarm();
        ctx.background_dispatch.record(HandedOver {
            handle: handle.clone(),
            group: group.clone(),
        });
        return Ran::Backgrounded(Backgrounded {
            handle,
            how: BackgroundedHow::Dispatched,
        });
    }
    let overrun_sink = sink.filter(|_| ctx.overrun_moves_to_background(request));
    let window = if overrun_sink.is_some() {
        run_time.min(FOREGROUND_WINDOW)
    } else {
        run_time
    };

    let ran = tokio::select! {
        biased;
        _ = ctx.cancellation_token.cancelled() => {
            cancel.cancel();
            Ran::Cancelled(work.await)
        }
        done = &mut work => Ran::Finished(done),
        _ = tokio::time::sleep(window) => match overrun_sink {
            Some(sink) => {
                let handle = handle.clone();
                sink.adopt(detached(work, None)).await;
                ctx.background_dispatch.record(HandedOver { handle: handle.clone(), group: None });
                Ran::Backgrounded(Backgrounded { handle, how: BackgroundedHow::Converted(window) })
            }
            None => {
                cancel.cancel();
                Ran::TimedOut(work.await)
            }
        }
    };
    stop_if_abandoned.disarm();
    ran
}

/// Nothing aborts a job's task, so joining it fails only on a panic in the
/// job, which belongs to whoever awaits it — or when the runtime itself shuts
/// down and cancels the task, where unwinding is all that is left to do.
fn joined<T>(joined: Result<T, tokio::task::JoinError>) -> T {
    match joined {
        Ok(value) => value,
        Err(error) => std::panic::resume_unwind(error.into_panic()),
    }
}

/// What the model is told when its call now runs in the background: how it
/// got there, its handle, the tool's own `details` (one line each, e.g. where
/// its output streams), and the rule for what to do next.
pub fn ack(backgrounded: &Backgrounded, details: &[String]) -> String {
    let header = match backgrounded.how {
        BackgroundedHow::Dispatched => DISPATCHED_HEADER.to_string(),
        BackgroundedHow::Converted(window) => {
            CONVERTED_HEADER.replace("{{window}}", &format!("{window:?}"))
        }
    };
    let mut out = format!("{header}\n- handle: {}", backgrounded.handle);
    for line in details {
        out.push_str("\n- ");
        out.push_str(line);
    }
    out.push_str("\n\n");
    out.push_str(BACKGROUND_DISPATCH_YIELD_GUIDANCE);
    out
}

const DISPATCHED_HEADER: &str = "[started in the background]";
const CONVERTED_HEADER: &str = "[still running after {{window}} — moved to the background]";

#[cfg(test)]
mod tests {
    use super::*;

    const COMMAND_LIKE: BackgroundSupport = BackgroundSupport {
        start_detached: false,
    };
    const SPAWN_LIKE: BackgroundSupport = BackgroundSupport {
        start_detached: true,
    };

    fn props(schema: &Value) -> Vec<String> {
        let mut names: Vec<String> = schema["properties"]
            .as_object()
            .expect("object schema")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    #[test]
    fn each_tool_declares_only_the_options_it_supports() {
        let base = json!({"type": "object", "properties": {"command": {"type": "string"}}});
        assert_eq!(
            props(&declare_options(base.clone(), &COMMAND_LIKE)),
            ["command", "on_timeout", "timeout_ms"]
        );
        assert_eq!(
            props(&declare_options(base, &SPAWN_LIKE)),
            ["background", "command", "group", "on_timeout", "timeout_ms"]
        );
    }

    #[test]
    fn the_run_time_is_bounded_at_an_hour_in_the_schema() {
        let base = json!({"type": "object", "properties": {}});
        let spawn = declare_options(base, &SPAWN_LIKE);
        assert_eq!(spawn["properties"]["timeout_ms"]["maximum"], 3_600_000);
        let described = spawn["properties"]["on_timeout"]["description"]
            .as_str()
            .unwrap();
        assert!(described.starts_with("For a call still running when `timeout_ms` elapses:"));
        assert!(described.ends_with("Ignored when background=true."));
    }

    /// Where nothing can go to the background, the background options go and
    /// `timeout_ms` stays — as the call's hard limit.
    #[test]
    fn withholding_keeps_the_run_time_as_a_hard_limit() {
        let base = json!({"type": "object", "properties": {"command": {"type": "string"}}});
        let mut schema = declare_options(base, &SPAWN_LIKE);
        withhold_options(&mut schema, &SPAWN_LIKE);
        assert_eq!(props(&schema), ["command", "timeout_ms"]);
        let described = schema["properties"]["timeout_ms"]["description"]
            .as_str()
            .unwrap();
        assert!(described.contains("before it is terminated"), "{described}");
        assert!(!described.contains("background"), "{described}");
    }

    #[test]
    fn the_run_time_defaults_to_ten_minutes_and_caps_at_an_hour() {
        let mut ctx = ToolContext::for_test();
        ctx.timeout = Duration::from_secs(24 * 60 * 60);
        let run_time = |params: Value| requested_run_time(&params, &ctx);
        assert_eq!(run_time(json!({})), Ok(FOREGROUND_WINDOW));
        assert_eq!(run_time(json!({"timeout_ms": 0})), Ok(FOREGROUND_WINDOW));
        assert_eq!(
            run_time(json!({"timeout_ms": 1500})),
            Ok(Duration::from_millis(1500))
        );
        assert_eq!(run_time(json!({"timeout_ms": 7_200_000})), Ok(MAX_RUN_TIME));
        assert!(run_time(json!({"timeout_ms": "soon"})).is_err());
        ctx.timeout = Duration::from_secs(5);
        assert_eq!(
            requested_run_time(&json!({}), &ctx),
            Ok(Duration::from_secs(5)),
            "the context's own budget bounds it too"
        );
    }

    #[test]
    fn parsing_defaults_to_background_and_forces_a_group_detached() {
        assert_eq!(
            BackgroundRequest::parse(&json!({}), &SPAWN_LIKE).unwrap(),
            BackgroundRequest::default()
        );
        let killed =
            BackgroundRequest::parse(&json!({"on_timeout": " Kill "}), &SPAWN_LIKE).unwrap();
        assert_eq!(killed, BackgroundRequest::Wait(OnTimeout::Kill));
        let grouped = BackgroundRequest::parse(&json!({"group": " fan "}), &SPAWN_LIKE).unwrap();
        assert_eq!(
            grouped,
            BackgroundRequest::Detach {
                group: BackgroundGroupName::parse("fan")
            }
        );
        let blank = BackgroundRequest::parse(&json!({"group": "  "}), &SPAWN_LIKE).unwrap();
        assert_eq!(blank, BackgroundRequest::default());
    }

    /// The ack carries the yield rule verbatim. This is the only place it
    /// reaches a card's run: `PromptShape::Issue` drops the standing hint
    /// because every hand-over delivers it here instead, so a reworded copy
    /// would silently leave that run with no rule at all.
    #[test]
    fn the_ack_hands_back_the_one_yield_rule() {
        let text = ack(
            &Backgrounded {
                handle: "bg-7".into(),
                how: BackgroundedHow::Converted(Duration::from_secs(120)),
            },
            &["output: streaming to `/w/out.log`".to_string()],
        );
        assert!(text.contains(BACKGROUND_DISPATCH_YIELD_GUIDANCE), "{text}");
        assert!(text.starts_with("[still running after 120s"), "{text}");
        assert!(text.contains("- handle: bg-7"), "{text}");
        assert!(text.contains("/w/out.log"), "{text}");
    }

    #[test]
    fn an_unknown_on_timeout_is_rejected_not_defaulted() {
        let err =
            BackgroundRequest::parse(&json!({"on_timeout": "kil"}), &COMMAND_LIKE).unwrap_err();
        assert!(err.contains("expected background|kill"), "{err}");
    }

    /// A tool that does not offer `background` / `group` does not read them:
    /// a stray one, of any shape, neither fails its call nor detaches it.
    #[test]
    fn options_a_tool_does_not_declare_are_left_unread() {
        let stray = json!({"background": "yes", "group": 7});
        assert_eq!(
            BackgroundRequest::parse(&stray, &COMMAND_LIKE).unwrap(),
            BackgroundRequest::default()
        );
        assert!(BackgroundRequest::parse(&stray, &SPAWN_LIKE).is_err());
    }

    mod runner {
        //! The lifecycle every background-capable tool shares: the foreground
        //! window, conversion, kill, the turn's stop, and start-detached.
        use std::sync::Arc;

        use super::super::*;
        use baybo_model::SubagentExitStatus;
        use tokio::sync::oneshot;

        #[derive(Default)]
        struct Adopted(parking_lot::Mutex<Vec<DetachedJob>>);

        #[async_trait]
        impl BackgroundJobSink for Adopted {
            async fn adopt(&self, job: DetachedJob) {
                self.0.lock().push(job);
            }
        }

        const WINDOW: Duration = Duration::from_secs(120);
        const STARTS_DETACHED: BackgroundSupport = BackgroundSupport {
            start_detached: true,
        };

        fn ctx(eligible: bool) -> (ToolContext, Arc<Adopted>) {
            let sink = Arc::new(Adopted::default());
            let mut ctx = ToolContext::for_test();
            ctx.background_eligible = eligible;
            ctx.background_jobs = Some(Arc::clone(&sink) as Arc<dyn BackgroundJobSink>);
            (ctx, sink)
        }

        /// A job that finishes with `"done"` once `finish` fires, or with
        /// `"cancelled"` once its token does.
        fn job() -> (Job<&'static str>, oneshot::Sender<()>, CancellationToken) {
            let (finish, finished) = oneshot::channel::<()>();
            let cancel = CancellationToken::new();
            let terminal = {
                let cancel = cancel.clone();
                async move {
                    tokio::select! {
                        _ = cancel.cancelled() => "cancelled",
                        _ = finished => "done",
                    }
                }
                .boxed()
            };
            let job = Job {
                handle: "bg-1".into(),
                kind: "probe".into(),
                summary: "probe work".into(),
                session: None,
                cancel: cancel.clone(),
                terminal,
                into_result: Box::new(|outcome: &'static str| {
                    futures::future::ready(PendingBackgroundResult::command(
                        "bg-1",
                        "probe work",
                        0,
                        "/dev/null",
                        outcome,
                        SubagentExitStatus::Completed,
                    ))
                    .boxed()
                }),
            };
            (job, finish, cancel)
        }

        #[tokio::test(start_paused = true)]
        async fn work_done_within_the_window_finishes_in_the_foreground() {
            let (ctx, sink) = ctx(true);
            let (job, finish, _) = job();
            finish.send(()).unwrap();
            let ran = run(&ctx, &BackgroundRequest::default(), WINDOW, job).await;
            assert!(matches!(ran, Ran::Finished("done")));
            assert!(sink.0.lock().is_empty());
        }

        #[tokio::test(start_paused = true)]
        async fn an_overrun_moves_to_the_background_and_still_delivers() {
            let (ctx, sink) = ctx(true);
            let (job, finish, _) = job();
            let ran = run(&ctx, &BackgroundRequest::default(), WINDOW, job).await;
            let Ran::Backgrounded(backgrounded) = ran else {
                panic!("an overrun converts");
            };
            assert_eq!(backgrounded.how, BackgroundedHow::Converted(WINDOW));
            let adopted = sink.0.lock().pop().expect("handed to the sink");
            assert_eq!(adopted.handle, "bg-1");
            assert_eq!(adopted.parent, ctx.session_id);
            assert_eq!(adopted.cohort, None, "a converted job is never grouped");
            assert_eq!(
                ctx.background_dispatch.take(),
                Some(HandedOver {
                    handle: "bg-1".into(),
                    group: None
                }),
                "a conversion is recorded as a hand-over too"
            );
            finish.send(()).unwrap();
            assert_eq!(adopted.result.await.summary_text, "done");
        }

        #[tokio::test(start_paused = true)]
        async fn kill_cancels_the_work_at_the_window() {
            let (ctx, sink) = ctx(true);
            let (job, _finish, cancel) = job();
            let kill = BackgroundRequest::Wait(OnTimeout::Kill);
            let ran = run(&ctx, &kill, WINDOW, job).await;
            assert!(matches!(ran, Ran::TimedOut("cancelled")));
            assert!(cancel.is_cancelled());
            assert!(sink.0.lock().is_empty());
        }

        /// Where the session cannot run background work, the run time is a
        /// hard limit: the work is killed when it elapses, `background` or not.
        #[tokio::test(start_paused = true)]
        async fn an_ineligible_session_kills_at_the_run_time() {
            let (ctx, sink) = ctx(false);
            let (job, _finish, cancel) = job();
            let ran = run(&ctx, &BackgroundRequest::default(), MAX_RUN_TIME, job).await;
            assert!(matches!(ran, Ran::TimedOut("cancelled")));
            assert!(cancel.is_cancelled());
            assert!(sink.0.lock().is_empty());
        }

        /// A cron fire's own turn kills an overrun instead of handing it over,
        /// even where the session could take background work.
        #[tokio::test(start_paused = true)]
        async fn a_turn_that_keeps_overruns_kills_them() {
            let (mut ctx, sink) = ctx(true);
            ctx.overrun_to_background = false;
            let (job, _finish, cancel) = job();
            let ran = run(&ctx, &BackgroundRequest::default(), WINDOW, job).await;
            assert!(matches!(ran, Ran::TimedOut("cancelled")));
            assert!(cancel.is_cancelled());
            assert!(sink.0.lock().is_empty());
        }

        /// A long run time does not hold a call that may move to the
        /// background in the foreground past the window.
        #[tokio::test(start_paused = true)]
        async fn an_overrun_moves_to_the_background_no_later_than_the_window() {
            let (ctx, sink) = ctx(true);
            let (job, _finish, _) = job();
            let started = tokio::time::Instant::now();
            let ran = run(&ctx, &BackgroundRequest::default(), MAX_RUN_TIME, job).await;
            let Ran::Backgrounded(backgrounded) = ran else {
                panic!("the overrun moves to the background");
            };
            assert_eq!(started.elapsed(), FOREGROUND_WINDOW);
            assert_eq!(
                backgrounded.how,
                BackgroundedHow::Converted(FOREGROUND_WINDOW)
            );
            assert_eq!(sink.0.lock().len(), 1);
        }

        #[tokio::test(start_paused = true)]
        async fn stopping_the_turn_stops_foreground_work() {
            let (ctx, sink) = ctx(true);
            let (job, _finish, cancel) = job();
            ctx.cancellation_token.cancel();
            let ran = run(&ctx, &BackgroundRequest::default(), WINDOW, job).await;
            assert!(matches!(ran, Ran::Cancelled("cancelled")));
            assert!(cancel.is_cancelled());
            assert!(sink.0.lock().is_empty());
        }

        #[tokio::test(start_paused = true)]
        async fn a_grouped_call_starts_detached_under_a_turn_scoped_cohort() {
            let (ctx, sink) = ctx(true);
            let (job, _finish, _) = job();
            let grouped =
                BackgroundRequest::parse(&json!({"group": "g"}), &STARTS_DETACHED).unwrap();
            let ran = run(&ctx, &grouped, WINDOW, job).await;
            let Ran::Backgrounded(backgrounded) = ran else {
                panic!("a group starts detached");
            };
            assert_eq!(backgrounded.how, BackgroundedHow::Dispatched);
            assert_eq!(
                ctx.background_dispatch.take(),
                Some(HandedOver {
                    handle: "bg-1".into(),
                    group: BackgroundGroupName::parse("g")
                })
            );
            let adopted = sink.0.lock().pop().expect("handed to the sink");
            assert_eq!(
                adopted.cohort,
                BackgroundGroupName::parse("g").map(|g| CohortKey::new(ctx.turn_id, &g))
            );
        }

        /// The agent loop drops an unfinished tool call when its turn is
        /// stopped. The work must still be cancelled and see its own cleanup
        /// through (a subagent releases its fan-out slot there), rather than
        /// being dropped halfway.
        #[tokio::test]
        async fn an_abandoned_call_still_stops_its_work_and_finishes_cleanup() {
            let (ctx, _sink) = ctx(true);
            let cancel = CancellationToken::new();
            let (cleaned, cleaned_rx) = oneshot::channel::<()>();
            let terminal = {
                let cancel = cancel.clone();
                async move {
                    cancel.cancelled().await;
                    // Cleanup that needs more than one poll, like draining a
                    // subprocess or writing a turn row.
                    tokio::task::yield_now().await;
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    let _ = cleaned.send(());
                }
                .boxed()
            };
            let job = Job {
                handle: "bg-1".into(),
                kind: "probe".into(),
                summary: "probe work".into(),
                session: None,
                cancel: cancel.clone(),
                terminal,
                into_result: Box::new(|()| {
                    futures::future::ready(PendingBackgroundResult::command(
                        "bg-1",
                        "probe work",
                        0,
                        "/dev/null",
                        "",
                        SubagentExitStatus::Completed,
                    ))
                    .boxed()
                }),
            };
            let call = tokio::spawn(async move {
                run(&ctx, &BackgroundRequest::default(), WINDOW, job).await;
            });
            tokio::task::yield_now().await;
            call.abort();
            tokio::time::timeout(Duration::from_secs(2), cleaned_rx)
                .await
                .expect("the work ran to the end of its cleanup")
                .expect("cleanup completed");
            assert!(
                cancel.is_cancelled(),
                "abandoning the call stopped the work"
            );
        }
    }
}
