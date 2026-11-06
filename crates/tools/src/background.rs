//! Background jobs as a capability a tool composes in, instead of
//! re-implementing.
//!
//! A tool that can hand slow work to the background declares a
//! [`BackgroundSupport`] from [`crate::Tool::background_support`]. Everything
//! that is the same for every such tool lives here, once:
//!
//! - **The options.** [`declare_options`] adds `on_timeout` (and, where the
//!   tool can start work detached, `background` and `group`) to the tool's
//!   schema; the registry strips them again wherever the session cannot run
//!   background work ([`withhold_options`]), so the model is never offered a
//!   choice the runtime would refuse.
//! - **Their parsing.** [`BackgroundRequest::parse`] is the one reading of
//!   those options, strict for every tool.
//! - **The lifecycle.** [`run`] owns the foreground window: it waits on the
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
    BACKGROUND_DISPATCH_YIELD_GUIDANCE, BackgroundNotificationGroup, OnTimeout,
    PendingBackgroundResult, SessionId,
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

/// How long a call runs in the foreground before `on_timeout` decides its
/// fate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForegroundWindow {
    /// The call's own deadline parameter (e.g. Bash's `timeout_ms`).
    Param(&'static str),
    /// A fixed wait the runtime applies to every call.
    Fixed(Duration),
}

/// A tool's declaration that it can run work past the turn that started it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackgroundSupport {
    pub window: ForegroundWindow,
    /// Whether the tool accepts [`BACKGROUND_PARAM`] and [`GROUP_PARAM`]:
    /// starting detached at once, alone or as a merged cohort. Only work that
    /// can be handed over before it has produced anything qualifies.
    pub start_detached: bool,
    /// Tool-specific advice appended to the `background` option's
    /// description (e.g. which runs it suits best).
    pub background_hint: &'static str,
}

impl BackgroundSupport {
    fn option_names(&self) -> &'static [&'static str] {
        if self.start_detached {
            &[ON_TIMEOUT_PARAM, BACKGROUND_PARAM, GROUP_PARAM]
        } else {
            &[ON_TIMEOUT_PARAM]
        }
    }

    fn window_phrase(&self) -> String {
        match self.window {
            ForegroundWindow::Param(param) => format!("When `{param}` elapses"),
            ForegroundWindow::Fixed(wait) => {
                let secs = wait.as_secs();
                if secs >= 60 && secs.is_multiple_of(60) {
                    format!("After a {}-minute foreground wait", secs / 60)
                } else {
                    format!("After a {secs}-second foreground wait")
                }
            }
        }
    }
}

/// `schema` plus the background options `support` accepts. The tool's
/// declared surface — what the manifest records and the executor validates
/// against — always carries them; [`withhold_options`] narrows only what a
/// session is offered.
pub fn declare_options(mut schema: Value, support: &BackgroundSupport) -> Value {
    let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) else {
        return schema;
    };
    insert_options(props, support);
    schema
}

fn insert_options(props: &mut Map<String, Value>, support: &BackgroundSupport) {
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
                "{}: 'background' (default) detaches it — handle now, result in a later turn — and 'kill' terminates it.{ignored_when_detached}",
                support.window_phrase()
            ),
        }),
    );
    if support.start_detached {
        let hint = if support.background_hint.is_empty() {
            String::new()
        } else {
            format!(" {}", support.background_hint)
        };
        props.insert(
            BACKGROUND_PARAM.to_string(),
            json!({
                "type": "boolean",
                "description": format!("Return a handle immediately instead of blocking; the result arrives in a later turn.{hint}"),
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

/// Remove the options `support` declared, for a session that cannot run
/// background work. Every one of them only chooses *how* to background, so
/// offering them there is offering a choice the runtime has already made.
pub(crate) fn withhold_options(schema: &mut Value, support: &BackgroundSupport) {
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        for name in support.option_names() {
            props.remove(*name);
        }
    }
}

/// The background options of one call, as the tool's caller sent them.
///
/// Parsed even where the session cannot run background work — a withheld
/// option sent anyway still parses — and resolved against eligibility by
/// [`run`]. A tool may skip `run` for a cheaper blocking path only where `run`
/// could not hand the work over anyway: when
/// [`ToolContext::can_hand_over_background`] is false, or the call asked for
/// `on_timeout = kill`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BackgroundRequest {
    pub on_timeout: OnTimeout,
    /// Start detached at once. Implied by a group.
    pub start_detached: bool,
    /// The barrier cohort, trimmed; a blank name is no group.
    pub group: Option<String>,
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
            return Ok(Self {
                on_timeout,
                ..Self::default()
            });
        }
        let group = match params.get(GROUP_PARAM) {
            None | Some(Value::Null) => None,
            Some(Value::String(raw)) => Some(raw.trim())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            Some(other) => return Err(format!("{GROUP_PARAM} must be a string, got {other}")),
        };
        let background = match params.get(BACKGROUND_PARAM) {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(other) => {
                return Err(format!("{BACKGROUND_PARAM} must be a boolean, got {other}"));
            }
        };
        Ok(Self {
            on_timeout,
            start_detached: background || group.is_some(),
            group,
        })
    }
}

/// Work a tool hands to [`run`]: something that keeps going until it is done
/// or `cancel` fires, whether or not the turn that started it is still there.
pub struct Job<T> {
    /// The id the model, `JobList` / `JobStop` and the eventual notification
    /// share. Minted by the tool with [`baybo_model::new_background_handle`],
    /// because a tool may need it before the work starts (Bash names its
    /// output files after it).
    pub handle: String,
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
    /// It finished in the foreground: within its window, or with no window at
    /// all where the session cannot run background work.
    Finished(T),
    /// The turn was stopped while it ran in the foreground; it was cancelled
    /// and drained.
    Cancelled(T),
    /// It outlasted its window under `on_timeout = kill`; it was cancelled and
    /// drained.
    TimedOut(T),
    /// It runs on in the background; tell the model with [`ack`].
    Backgrounded(Backgrounded),
}

/// A job that now runs in the background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backgrounded {
    pub handle: String,
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
    pub handle: String,
    /// The session whose actor receives the result.
    pub parent: SessionId,
    pub kind: String,
    pub summary: String,
    pub session: Option<SessionId>,
    /// The turn-namespaced cohort key the result is held under, for a job
    /// started as part of a group.
    pub group: Option<String>,
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

/// What the executor learns about the job a call started detached, so the
/// agent loop can count a grouped call into its cohort. Filled by [`run`],
/// read once the call returns.
#[derive(Debug, Default)]
pub struct DispatchRecord(parking_lot::Mutex<Option<String>>);

impl DispatchRecord {
    /// The raw group of the job this call started detached, if it had one.
    pub fn take_group(&self) -> Option<String> {
        self.0.lock().take()
    }

    fn record_group(&self, group: String) {
        *self.0.lock() = Some(group);
    }
}

/// Run `job` under the call's background options.
///
/// - The session cannot run background work (or no sink is wired): wait for
///   the job with no window at all, stopping it with the turn.
/// - The call asked to start detached and the tool supports that: hand it
///   over at once.
/// - Otherwise wait up to `window`; then `on_timeout` either hands it over or
///   cancels it.
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
    support: &BackgroundSupport,
    request: &BackgroundRequest,
    window: Duration,
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
    let detached = |work: BoxFuture<'static, T>, group| DetachedJob {
        handle: handle.clone(),
        parent: ctx.session_id.clone(),
        kind,
        summary,
        session,
        group,
        cancel: cancel.clone(),
        result: work.then(into_result).boxed(),
    };

    let sink = ctx
        .background_jobs
        .as_ref()
        .filter(|_| ctx.can_hand_over_background());
    let Some(sink) = sink else {
        let ran = tokio::select! {
            biased;
            _ = ctx.cancellation_token.cancelled() => {
                cancel.cancel();
                Ran::Cancelled(work.await)
            }
            done = &mut work => Ran::Finished(done),
        };
        stop_if_abandoned.disarm();
        return ran;
    };
    if support.start_detached && request.start_detached {
        let group = request.group.clone();
        if let Some(group) = &group {
            ctx.background_dispatch.record_group(group.clone());
        }
        let cohort = group.map(|g| BackgroundNotificationGroup::cohort_key(ctx.turn_id, &g));
        sink.adopt(detached(work, cohort)).await;
        stop_if_abandoned.disarm();
        return Ran::Backgrounded(Backgrounded {
            handle,
            how: BackgroundedHow::Dispatched,
        });
    }

    let ran = tokio::select! {
        biased;
        _ = ctx.cancellation_token.cancelled() => {
            cancel.cancel();
            Ran::Cancelled(work.await)
        }
        done = &mut work => Ran::Finished(done),
        _ = tokio::time::sleep(window) => match request.on_timeout {
            OnTimeout::Kill => {
                cancel.cancel();
                Ran::TimedOut(work.await)
            }
            OnTimeout::Background => {
                let handle = handle.clone();
                sink.adopt(detached(work, None)).await;
                Ran::Backgrounded(Backgrounded { handle, how: BackgroundedHow::Converted(window) })
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
        window: ForegroundWindow::Param("timeout_ms"),
        start_detached: false,
        background_hint: "",
    };
    const SPAWN_LIKE: BackgroundSupport = BackgroundSupport {
        window: ForegroundWindow::Fixed(Duration::from_secs(120)),
        start_detached: true,
        background_hint: "Best for long runs.",
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
            ["command", "on_timeout"]
        );
        assert_eq!(
            props(&declare_options(base, &SPAWN_LIKE)),
            ["background", "command", "group", "on_timeout"]
        );
    }

    #[test]
    fn the_window_is_named_in_the_on_timeout_description() {
        let base = json!({"type": "object", "properties": {}});
        let command = declare_options(base.clone(), &COMMAND_LIKE);
        assert!(
            command["properties"]["on_timeout"]["description"]
                .as_str()
                .unwrap()
                .starts_with("When `timeout_ms` elapses:")
        );
        let spawn = declare_options(base, &SPAWN_LIKE);
        let described = spawn["properties"]["on_timeout"]["description"]
            .as_str()
            .unwrap();
        assert!(described.starts_with("After a 2-minute foreground wait:"));
        assert!(described.ends_with("Ignored when background=true."));
        assert!(
            spawn["properties"]["background"]["description"]
                .as_str()
                .unwrap()
                .ends_with("Best for long runs.")
        );
    }

    #[test]
    fn withholding_removes_exactly_the_declared_options() {
        let base = json!({"type": "object", "properties": {"command": {"type": "string"}}});
        let mut schema = declare_options(base, &SPAWN_LIKE);
        withhold_options(&mut schema, &SPAWN_LIKE);
        assert_eq!(props(&schema), ["command"]);
    }

    #[test]
    fn parsing_defaults_to_background_and_forces_a_group_detached() {
        assert_eq!(
            BackgroundRequest::parse(&json!({}), &SPAWN_LIKE).unwrap(),
            BackgroundRequest::default()
        );
        let killed =
            BackgroundRequest::parse(&json!({"on_timeout": " Kill "}), &SPAWN_LIKE).unwrap();
        assert_eq!(killed.on_timeout, OnTimeout::Kill);
        let grouped = BackgroundRequest::parse(&json!({"group": " fan "}), &SPAWN_LIKE).unwrap();
        assert!(grouped.start_detached);
        assert_eq!(grouped.group.as_deref(), Some("fan"));
        let blank = BackgroundRequest::parse(&json!({"group": "  "}), &SPAWN_LIKE).unwrap();
        assert!(!blank.start_detached);
        assert_eq!(blank.group, None);
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
        const CONVERTS: BackgroundSupport = BackgroundSupport {
            window: ForegroundWindow::Fixed(WINDOW),
            start_detached: false,
            background_hint: "",
        };
        const STARTS_DETACHED: BackgroundSupport = BackgroundSupport {
            start_detached: true,
            ..CONVERTS
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
            let ran = run(&ctx, &CONVERTS, &BackgroundRequest::default(), WINDOW, job).await;
            assert!(matches!(ran, Ran::Finished("done")));
            assert!(sink.0.lock().is_empty());
        }

        #[tokio::test(start_paused = true)]
        async fn an_overrun_moves_to_the_background_and_still_delivers() {
            let (ctx, sink) = ctx(true);
            let (job, finish, _) = job();
            let ran = run(&ctx, &CONVERTS, &BackgroundRequest::default(), WINDOW, job).await;
            let Ran::Backgrounded(backgrounded) = ran else {
                panic!("an overrun converts");
            };
            assert_eq!(backgrounded.how, BackgroundedHow::Converted(WINDOW));
            let adopted = sink.0.lock().pop().expect("handed to the sink");
            assert_eq!(adopted.handle, "bg-1");
            assert_eq!(adopted.parent, ctx.session_id);
            assert_eq!(adopted.group, None, "a converted job is never grouped");
            finish.send(()).unwrap();
            assert_eq!(adopted.result.await.summary_text, "done");
        }

        #[tokio::test(start_paused = true)]
        async fn kill_cancels_the_work_at_the_window() {
            let (ctx, sink) = ctx(true);
            let (job, _finish, cancel) = job();
            let kill = BackgroundRequest {
                on_timeout: OnTimeout::Kill,
                ..Default::default()
            };
            let ran = run(&ctx, &CONVERTS, &kill, WINDOW, job).await;
            assert!(matches!(ran, Ran::TimedOut("cancelled")));
            assert!(cancel.is_cancelled());
            assert!(sink.0.lock().is_empty());
        }

        /// Where the session cannot run background work there is no window at
        /// all: the call waits however long the work takes.
        #[tokio::test(start_paused = true)]
        async fn an_ineligible_session_waits_past_the_window() {
            let (ctx, sink) = ctx(false);
            let (job, finish, _) = job();
            let waiting = tokio::spawn(async move {
                run(
                    &ctx,
                    &STARTS_DETACHED,
                    &BackgroundRequest::default(),
                    WINDOW,
                    job,
                )
                .await
            });
            tokio::time::advance(WINDOW * 3).await;
            assert!(
                !waiting.is_finished(),
                "no timer where nothing can background"
            );
            finish.send(()).unwrap();
            assert!(matches!(waiting.await.unwrap(), Ran::Finished("done")));
            assert!(sink.0.lock().is_empty());
        }

        #[tokio::test(start_paused = true)]
        async fn stopping_the_turn_stops_foreground_work() {
            let (ctx, sink) = ctx(true);
            let (job, _finish, cancel) = job();
            ctx.cancellation_token.cancel();
            let ran = run(&ctx, &CONVERTS, &BackgroundRequest::default(), WINDOW, job).await;
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
            let ran = run(&ctx, &STARTS_DETACHED, &grouped, WINDOW, job).await;
            let Ran::Backgrounded(backgrounded) = ran else {
                panic!("a group starts detached");
            };
            assert_eq!(backgrounded.how, BackgroundedHow::Dispatched);
            assert_eq!(ctx.background_dispatch.take_group().as_deref(), Some("g"));
            let adopted = sink.0.lock().pop().expect("handed to the sink");
            assert_eq!(
                adopted.group,
                Some(BackgroundNotificationGroup::cohort_key(ctx.turn_id, "g"))
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
                run(&ctx, &CONVERTS, &BackgroundRequest::default(), WINDOW, job).await;
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

        /// A tool that cannot start detached ignores a `background` / `group`
        /// sent anyway, rather than handing over work that has not started.
        #[tokio::test(start_paused = true)]
        async fn start_detached_needs_the_tool_to_support_it() {
            let (ctx, sink) = ctx(true);
            let (job, finish, _) = job();
            finish.send(()).unwrap();
            let grouped = BackgroundRequest {
                start_detached: true,
                ..Default::default()
            };
            let ran = run(&ctx, &CONVERTS, &grouped, WINDOW, job).await;
            assert!(matches!(ran, Ran::Finished("done")));
            assert!(sink.0.lock().is_empty());
            assert_eq!(ctx.background_dispatch.take_group(), None);
        }
    }
}
