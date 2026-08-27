import {
  Fragment,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { useTranslation } from "react-i18next";
import { AttachmentBubble } from "../attachments";
import { bindNativeTarget, currentConnEpoch } from "../bridge";
import { MarkdownBody } from "../Markdown";
import {
  openIssue,
  openRun,
  pickField,
  postActivityAtBottom,
  postGeneratedFace,
  postIssueRendered,
  postIssueState,
  provideIssueState,
  revealIssueTarget,
  retryComment,
  subscribeIssue,
  type IssuePayload,
  type IssueViewState,
  type Person,
} from "./bridge";
import { avatarUrl } from "./avatars";
import { botttsPng } from "./generatedFace";
import { fold, foldHead, type Fold } from "./timeline";
import {
  isLiveRun,
  toWireAttachment,
  type Actor,
  type ChildIssue,
  type IssueDetail,
  type IssueEvent,
  type IssueRun,
} from "./types";

/// A project card, rendered whole in a webview under a native header and dock.
///
/// The whole BODY is here rather than only the description, and that is the
/// decision the layout turns on: a webview nested in a native scroll view is
/// two scrollers plus a height round-trip on every reflow, while a full-page
/// webview is exactly `ChatScreen`'s existing layering — header, webview, dock,
/// streamed bottom inset. Comment markdown, KaTeX, `#N` links and the
/// attachment cards all come along for free.
type IssuePageProps = {
  targetId?: string;
  initialBottomInset?: number;
  initialState?: IssueViewState;
  initialPayload?: IssuePayload;
};

export function IssuePage({
  targetId = "test",
  initialBottomInset = 0,
  initialState,
  initialPayload,
}: IssuePageProps = {}) {
  const { t } = useTranslation();
  const [payload, setPayload] = useState<IssuePayload | null>(initialPayload ?? null);
  const [bottomInset, setBottomInset] = useState(initialBottomInset);
  const [folds, setFolds] = useState<Record<string, boolean>>(() => ({
    ...(initialState?.folds ?? {}),
  }));
  const foldsRef = useRef(folds);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const renderedFor = useRef<string | null>(null);
  const restoredScroll = useRef(false);
  const stateTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  /// Where the operator stopped reading, FROZEN for the life of this page.
  ///
  /// It has to be: painting the card stamps it read (`postIssueRendered`), the
  /// stamp invalidates the timeline, and the refetch that follows a second
  /// later answers `first_unread: null` — so a rule that tracked the payload
  /// would appear and then vanish under the reader, taking the one landmark
  /// telling them where the new part starts.
  const [landing, setLanding] = useState<string | null>(null);
  /// The landing already scrolled to. One scroll per boundary, not per
  /// delivery — a card refetches on every frame its board sends.
  const landedOn = useRef<string | null>(null);
  /// The reader has put a finger on the page. From then on the scroll is
  /// theirs: a live payload arriving mid-read must not yank them anywhere.
  const grabbed = useRef(false);
  const followedLocalComment = useRef<string | null>(null);

  useLayoutEffect(() => {
    const unbind = bindNativeTarget(targetId);
    revealIssueTarget(targetId);
    return unbind;
  }, [targetId]);

  useEffect(
    () =>
      subscribeIssue({
        deliver: (p) => setPayload(p),
        bottomInset: (px) => setBottomInset(px),
        jumpToLatest: () => {
          const el = scrollRef.current;
          if (el) el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
        },
      }),
    [],
  );

  /// Read is stamped only once the card has actually painted, and once per
  /// card: a card whose timeline threw has not been read, and re-stamping on
  /// every delivery would spend a round trip per comment.
  useEffect(() => {
    if (payload?.timelineLive !== true) return;
    const key = `${payload.issue.project_id}#${payload.issue.number}`;
    if (renderedFor.current === key) return;
    renderedFor.current = key;
    postIssueRendered(targetId);
  }, [payload, targetId]);

  // Latched on the first payload that names one and never cleared — see
  // `landing`. A second boundary never arrives while this page lives: the only
  // thing that moves the cursor is opening the card, which already happened.
  useEffect(() => {
    const anchor = payload?.firstUnread;
    if (anchor === undefined) return;
    setLanding((current) => current ?? anchor);
  }, [payload?.firstUnread]);

  /// Open the card where the reading stopped, once.
  ///
  /// Deliberately after the rule has rendered rather than on the payload: the
  /// row is found in the DOM, so a boundary naming an entry this page did not
  /// draw simply leaves the card at the top instead of scrolling into nothing.
  useEffect(() => {
    if (landing === null || landedOn.current === landing) return;
    const el = scrollRef.current;
    const rule = el?.querySelector<HTMLElement>("[data-unread-rule]");
    if (!el || !rule) return;
    landedOn.current = landing;
    if (grabbed.current) return;
    rule.scrollIntoView({ block: "start" });
  }, [landing, payload]);

  /// Give a faceless teammate the generated face, once.
  ///
  /// An agent is created with no avatar — every path that makes one sets it
  /// to null — so without this the phone draws letters for a roster the web
  /// draws robots for. The generator is the library `app/web` already uses,
  /// which is the whole reason it runs HERE rather than in Rust: the gateway
  /// has no JS engine, and porting DiceBear would be a second implementation
  /// of somebody else's artwork, drifting on their next release.
  ///
  /// Once per agent per page, and only for agents this card actually names.
  /// Native answers by uploading and PUTting; the face arrives on the next
  /// delivery like any other change to the team.
  const drawnFor = useRef(new Set<string>());
  useEffect(() => {
    const people = payload?.people;
    if (people === undefined) return;
    for (const [id, person] of Object.entries(people)) {
      if (person.avatar !== undefined || drawnFor.current.has(id)) continue;
      drawnFor.current.add(id);
      void botttsPng(id)
        .then((png) => postGeneratedFace(targetId, id, png))
        .catch(() => {
          // A face that could not be drawn leaves the agent as it already
          // was — with a monogram — and does not retry this page's life.
        });
    }
  }, [payload?.people, targetId]);

  /// Tell native whether the newest activity is on screen — it draws the way
  /// back down, and only when there is one.
  const reportAtBottom = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    postActivityAtBottom(targetId, el.scrollHeight - el.scrollTop - el.clientHeight < 48);
  }, [targetId]);

  const readPageState = useCallback((): IssueViewState | null => {
    const el = scrollRef.current;
    if (!el) return null;
    return { scrollTop: Math.max(0, el.scrollTop), folds: { ...foldsRef.current } };
  }, []);

  useLayoutEffect(() => provideIssueState(readPageState), [readPageState]);

  const flushPageState = useCallback(() => {
    const state = readPageState();
    if (state !== null) postIssueState(targetId, state);
  }, [readPageState, targetId]);

  const schedulePageState = useCallback(() => {
    if (stateTimer.current !== null) return;
    stateTimer.current = setTimeout(() => {
      stateTimer.current = null;
      flushPageState();
    }, 80);
  }, [flushPageState]);

  useEffect(
    () => () => {
      if (stateTimer.current !== null) clearTimeout(stateTimer.current);
      flushPageState();
    },
    [flushPageState],
  );

  useEffect(() => {
    foldsRef.current = folds;
    schedulePageState();
  }, [folds, schedulePageState]);

  const changeFold = useCallback((key: string, open: boolean) => {
    setFolds((current) => {
      const next = { ...current, [key]: open };
      foldsRef.current = next;
      return next;
    });
  }, []);

  useLayoutEffect(() => {
    if (payload === null || restoredScroll.current) return;
    const el = scrollRef.current;
    if (!el) return;
    restoredScroll.current = true;
    if (initialState === undefined) return;
    grabbed.current = true;
    el.scrollTop = Math.max(0, initialState.scrollTop);
    reportAtBottom();
    schedulePageState();
  }, [initialState, payload, reportAtBottom, schedulePageState]);

  /// On every delivery as well as on every scroll. A card that arrives taller
  /// than its screen fires no scroll event at all, so a page reported only
  /// from `onScroll` opens at the top saying it is at the bottom — which is
  /// exactly the card that needs the disc most.
  useEffect(reportAtBottom, [reportAtBottom, payload, landing]);

  const pending = payload?.pendingComments ?? [];
  const latestLocalComment = pending[pending.length - 1]?.client_msg_id;
  useLayoutEffect(() => {
    if (
      latestLocalComment === undefined ||
      followedLocalComment.current === latestLocalComment
    ) {
      return;
    }
    followedLocalComment.current = latestLocalComment;
    const el = scrollRef.current;
    if (el) {
      el.scrollTop = el.scrollHeight;
      schedulePageState();
    }
  }, [latestLocalComment, schedulePageState]);

  if (!payload) {
    return <div className="issue-loading">{t("issue.loading")}</div>;
  }

  const { issue, events, runs, people } = payload;
  const confirmedClientMsgIds = new Set(
    events.flatMap((event) =>
      event.client_msg_id === undefined ? [] : [event.client_msg_id],
    ),
  );
  const activityEvents = [
    ...events,
    ...(payload.pendingComments ?? []).filter(
      (event) =>
        event.client_msg_id === undefined || !confirmedClientMsgIds.has(event.client_msg_id),
    ),
  ];
  // Ids in, people out. The DTOs carry profile ids and only the board knows
  // the team, so the map arrives on the payload and every name and face on
  // this page goes through it — an id that resolves to nothing prints as
  // itself, which is what the gateway does too, and wears a blank disc.
  const who = (id: string): Person => people[id] ?? { handle: id, monogram: "" };
  const handle = (id: string): string => who(id).handle;
  const liveRun = runs.find(isLiveRun) ?? null;
  // Who opened the card: the description reads as their post, the way the
  // first box on a GitHub issue does. Off the timeline's `opened` entry —
  // the card DTO records only whether an AGENT opened it, not which one.
  const opened = activityEvents.find((e) => e.body.kind === "opened") ?? null;
  // And the entry is then HOISTED rather than repeated: the description's own
  // head already says "@who opened this card", so leaving the line in the
  // Activity would print the same fact twice, a screen apart.
  const timeline =
    opened === null ? activityEvents : activityEvents.filter((e) => e.id !== opened.id);

  return (
    <div
      className="issue-page"
      ref={scrollRef}
      onScroll={() => {
        reportAtBottom();
        schedulePageState();
      }}
      // `pointerdown`, not `scroll`: the auto-scroll below fires a scroll of
      // its own, and a guard that could not tell the two apart would disarm
      // itself on the very first thing it does.
      onPointerDown={() => (grabbed.current = true)}
      style={{ paddingBottom: `${String(bottomInset + 24)}px` }}
    >
      <IssueHead issue={issue} opened={opened} who={who} />
      <Description issue={issue} />
      <StateBand issue={issue} handle={handle} liveRun={liveRun} />
      {issue.attachments !== undefined && issue.attachments.length > 0 && (
        <section className="issue-section">
          <h2>{t("issue.attachments")}</h2>
          <div className="issue-attachments">
            {issue.attachments.map((a) => (
              <AttachmentBubble
                key={a.blob_id}
                attachment={toWireAttachment(a)}
                connEpoch={currentConnEpoch()}
              />
            ))}
          </div>
        </section>
      )}
      <SubIssues issue={issue} children={payload.children ?? []} />
      <Activity
        events={timeline}
        landing={landing}
        who={who}
        folds={folds}
        onFold={changeFold}
      />
    </div>
  );
}

/// The title, and one thin line of provenance under it.
///
/// **Nothing here is a control.** The three pickers used to sit between the
/// title and the card's first sentence, which put a row of pills in front of
/// the two things a reader opens a card to find out: what it is called and
/// what it says. They are state, they are read second, and they now sit under
/// the description in the `StateBand` — the head is the title, then who opened
/// it, then the text.
///
/// The meta line also absorbed the description's old header (`@who opened
/// this card · time`): that was a bordered bar with an avatar beside it,
/// carrying two facts, directly under a title that had just said what the
/// card is.
function IssueHead({
  issue,
  opened,
  who,
}: {
  issue: IssueDetail;
  opened: IssueEvent | null;
  who: (id: string) => Person;
}) {
  const { t } = useTranslation();
  const cancelled = issue.cancelled_at_ms !== undefined;
  const meta: ReactNode[] = [];
  meta.push(
    <span key="opened">
      {t("issue.openedBy", {
        who: opened === null ? t("issue.system") : speaker(opened.actor, who, t),
      })}{" "}
      {shortTime(opened?.created_at_ms ?? issue.created_at_ms)}
    </span>,
  );
  if (issue.branch !== undefined && issue.branch !== "") {
    meta.push(<span key="branch">⑂ {issue.branch}</span>);
  }
  if (issue.parent !== undefined) {
    meta.push(
      <button type="button" key="parent" onClick={() => openIssue(issue.parent ?? 0)}>
        ↳ #{issue.parent}
      </button>,
    );
  }

  return (
    <header className="issue-head">
      <h1 className={cancelled ? "cancelled" : undefined}>{issue.title}</h1>
      <div className="issue-meta">
        {meta.map((item, i) => (
          <Fragment key={i}>
            {i > 0 && <span className="issue-meta-dot">·</span>}
            {item}
          </Fragment>
        ))}
      </div>
    </header>
  );
}

/// What state the card is in, under the text it is about.
///
/// The three pickers, the run that holds the card, and the block that stops
/// it — one band, read after the description rather than in front of it.
///
/// The chips post `pick`; native owns the pickers and the writes, because a
/// move sends the destination column's whole order and a move into In Progress
/// starts a run — rules that live in `ProjectsStore` and may not have a second
/// implementation on this page.
///
/// **The chips carry a hue**, which is this app's one departure from
/// ink-on-paper (see `docs/design-system.md`): a status and a priority are
/// the two facts a board is scanned for, they are read in a glance rather
/// than a sentence, and the word alone in ink-soft made a card in Review
/// indistinguishable from one in Backlog until you read it. The hue is a
/// property of the VALUE — the same table tints the sub-issue dots — so the
/// page cannot end up saying `done` in two colours.
function StateBand({
  issue,
  handle,
  liveRun,
}: {
  issue: IssueDetail;
  handle: (id: string) => string;
  liveRun: IssueRun | null;
}) {
  const { t } = useTranslation();
  const assignee = issue.assignee;
  return (
    <div className="issue-state">
      <div className="issue-chips">
        <button
          type="button"
          className="issue-chip"
          data-status={issue.status}
          onClick={() => pickField("status")}
        >
          {t(`issue.status.${issue.status}`)}
        </button>
        <button
          type="button"
          className="issue-chip"
          data-priority={issue.priority}
          onClick={() => pickField("priority")}
        >
          {t(`issue.priority.${issue.priority}`)}
        </button>
        <button type="button" className="issue-chip" onClick={() => pickField("assignee")}>
          {assignee !== undefined ? `@${handle(assignee)}` : t("issue.unassigned")}
        </button>
      </div>
      {liveRun !== null && <RunRow run={liveRun} who={handle(liveRun.agent_id)} />}
      {issue.blocked_reason !== undefined && issue.blocked_reason !== "" && (
        <div className="issue-blocked" role="note">
          <span className="issue-blocked-label">{t("issue.blocked")}</span>
          <span>{issue.blocked_reason}</span>
        </div>
      )}
    </div>
  );
}

/// The live run, as ONE LINE — what is happening, who is doing it, and the way
/// into its transcript.
///
/// It was a bordered box, which put a second rectangle between the title and
/// the card's first sentence. A run is a state, not an object: the line reads
/// as one — and it is the only run left on this page. The card's whole run
/// LIST moved into the native ⋯, where the rest of the things you can do to a
/// card already are; a settled attempt is history, and history does not belong
/// between a card's state and its comments.
function RunRow({ run, who }: { run: IssueRun; who: string }) {
  const { t } = useTranslation();
  return (
    <div className="issue-run" data-run={run.status}>
      <span className="issue-run-word">{t(`issue.run.${run.status}`)}</span>
      <span className="issue-run-who">@{who}</span>
      <button type="button" className="issue-run-open" onClick={() => openRun(run.attempt)}>
        {t("issue.openRun")} ›
      </button>
    </div>
  );
}

/// What the card says, read-only.
///
/// It had an editor — a plain `<textarea>` over the raw markdown, opened from
/// the card's ⋯ — and the whole of it came out on 2026-08-26. A card's text is
/// written by whoever files it and by the agent working it; the phone reads
/// it, comments on it, and does not rewrite it.
function Description({ issue }: { issue: IssueDetail }) {
  const { t } = useTranslation();
  return (
    <section className="issue-section issue-body">
      {issue.description === "" ? (
        <p className="issue-empty">{t("issue.noDescription")}</p>
      ) : (
        <MarkdownBody text={issue.description} />
      )}
    </section>
  );
}

/// A card's children.
///
/// The progress counts come from the card's own DTO (`sub_issues`) and the LIST
/// comes from the board — the card carries only a done/total. When the two
/// disagree the DTO wins the header, because it is the number the board's own
/// column count agrees with.
function SubIssues({ issue, children }: { issue: IssueDetail; children: ChildIssue[] }) {
  const { t } = useTranslation();
  const progress = issue.sub_issues;
  if (progress === undefined || progress.total === 0) return null;
  return (
    <section className="issue-section">
      <h2>
        {t("issue.subIssues")}{" "}
        <span className="issue-count">
          {`${String(progress.done)}/${String(progress.total)}`}
        </span>
      </h2>
      <ul className="issue-subs">
        {children.map((sub) => (
          <li key={sub.number}>
            <button type="button" onClick={() => openIssue(sub.number)}>
              <span className="issue-sub-dot" data-status={sub.status} aria-hidden="true" />
              <span className={sub.cancelled_at_ms !== undefined ? "cancelled" : undefined}>
                #{sub.number} {sub.title}
              </span>
            </button>
          </li>
        ))}
      </ul>
    </section>
  );
}

/// The card's Activity: what people said, with the machinery folded away.
///
/// `landing` is where the operator stopped reading, and the rule drawn above
/// that row is the only thing on this page that says so. It is not a filter:
/// everything above it is still there, because a card is a record and the
/// point of arriving mid-thread is to be able to scroll back up out of it.
function Activity({
  events,
  landing,
  who,
  folds,
  onFold,
}: {
  events: IssueEvent[];
  landing: string | null;
  who: (id: string) => Person;
  folds: Record<string, boolean>;
  onFold: (key: string, open: boolean) => void;
}) {
  const { t } = useTranslation();
  const folded = useMemo(() => fold(events, landing ?? undefined), [events, landing]);
  if (folded.length === 0) return null;
  return (
    <section className="issue-section">
      <h2 className="issue-ruled">{t("issue.activity")}</h2>
      <ol className="issue-activity">
        {folded.map((item, i) => {
          const key = rowKey(item, i);
          return (
            <Fragment key={key}>
              {foldHead(item)?.id === landing && (
                <li className="issue-unread-rule" data-unread-rule>
                  {t("issue.unreadFrom")}
                </li>
              )}
              <FoldRow
                item={item}
                landed={foldHead(item)?.id === landing}
                who={who}
                toggled={folds[key]}
                onToggle={(open) => onFold(key, open)}
              />
            </Fragment>
          );
        })}
      </ol>
    </section>
  );
}

function rowKey(item: Fold, index: number): string {
  return item.kind === "entry" ? item.event.id : `sys-${item.events[0]?.id ?? String(index)}`;
}

/// One folded run of machinery, closed until asked.
///
/// **A run of ONE is not a run** (2026-08-26). It was uniform for a while —
/// every run collapsed, a lone `moved` included — on the argument that what
/// buries the two things a person said is a wall of rows that all look like
/// rows, so one closed line per run is what makes the comments findable. In
/// practice `1 event ›` is a control that hides exactly one line and spends
/// one saying so, and a card's Activity was full of them. Two or more still
/// collapse.
///
/// The grouping itself stays in `fold`: a split at the unread boundary really
/// can leave a group of one, and to the model that is still a group. This is
/// only about what a group of one is DRAWN as.
///
/// `landed` is the exception, and it has to be: the run carrying the unread
/// boundary is what the page just scrolled to, and landing a reader on a
/// closed line is landing them on nothing.
function FoldRow({
  item,
  landed,
  who,
  toggled,
  onToggle,
}: {
  item: Fold;
  landed: boolean;
  who: (id: string) => Person;
  toggled: boolean | undefined;
  onToggle: (open: boolean) => void;
}) {
  const { t } = useTranslation();

  if (item.kind === "entry") return <EntryRow event={item.event} who={who} />;
  if (item.events.length === 1) return <EntryRow event={item.events[0]} who={who} />;

  const open = toggled ?? landed;
  return (
    <>
      <li className="issue-fold">
        <button type="button" onClick={() => onToggle(!open)} aria-expanded={open}>
          {t("issue.nEvents", { count: item.events.length })}
          {/* ONE glyph, rotated — the transcript's `.work-chevron`. Two
              characters (`›` and `⌄`) have two sets of metrics, so the mark
              moved every time the fold opened. */}
          <span className={`issue-chevron${open ? " open" : ""}`} aria-hidden="true">
            ›
          </span>
        </button>
      </li>
      {open && item.events.map((event) => <EntryRow key={event.id} event={event} who={who} />)}
    </>
  );
}

/// One timeline entry: a POST if somebody wrote it, a LINE if the board did.
///
/// The split is the whole shape of the page. What a person (or an agent, which
/// on a board is the same kind of thing) said is a boxed post with a face
/// beside it — it has an author, a body, and it is what the card is about.
/// Machinery has no body worth a box: a `moved` is one sentence, and giving it
/// the same frame as a paragraph of reasoning makes a card read as a wall of
/// identical rectangles.
function EntryRow({ event, who }: { event: IssueEvent; who: (id: string) => Person }) {
  const { t } = useTranslation();
  const text = typeof event.body.text === "string" ? event.body.text : "";
  const attachments = Array.isArray(event.body.attachments) ? event.body.attachments : [];

  if (event.body.kind !== "comment") {
    return (
      <li className="issue-line">
        <span className="issue-line-dot" aria-hidden="true" />
        <span className="issue-line-text">
          <span className="issue-line-who">{speaker(event.actor, who, t)}</span>{" "}
          {/* An unrecognised kind renders as its own name rather than throwing:
              the gateway adds kinds on its own schedule, and a card whose
              Activity died because of one would take the comments with it. */}
          {describe(event, t)}
        </span>
        <span className="issue-line-when">{shortTime(event.created_at_ms)}</span>
      </li>
    );
  }

  return (
    <li className="issue-entry comment" data-actor={event.actor.kind}>
      <div className="issue-post">
        <div className="issue-said-head">
          <Avatar actor={event.actor} who={who} />
          <span className="issue-said-who">{speaker(event.actor, who, t)}</span>
          <span className="issue-said-when">{shortTime(event.created_at_ms)}</span>
        </div>
        <div className={`issue-box${event.send_state ? ` ${event.send_state}` : ""}`}>
          {event.send_state === "sending" && <span className="send-spinner" aria-hidden="true" />}
          {event.send_state === "failed" && event.client_msg_id !== undefined && (
            <button
              type="button"
              className="send-failed"
              onClick={() => retryComment(event.client_msg_id ?? "")}
              aria-label={t("chat.retrySend")}
            >
              !
            </button>
          )}
          <MarkdownBody text={text} />
          {attachments.length > 0 && (
            <div className="issue-attachments">
              {attachments.map((raw, i) => {
                const a = raw as { blob_id?: unknown };
                if (typeof a.blob_id !== "string") return null;
                return (
                  <AttachmentBubble
                    key={`${a.blob_id}-${String(i)}`}
                    attachment={toWireAttachment(raw as Parameters<typeof toWireAttachment>[0])}
                    connEpoch={currentConnEpoch()}
                  />
                );
              })}
            </div>
          )}
        </div>
      </div>
    </li>
  );
}

/// Whoever an actor is, in words.
function speaker(actor: Actor, who: (id: string) => Person, t: Translate): string {
  // The operator reads as themselves. This printed "board" until 2026-08-25,
  // because `actorHandle` answers `null` for both a user and the system and
  // the row printed the system's word for either.
  if (actor.kind === "user") return t("issue.you");
  if (actor.kind === "system") return t("issue.system");
  return `@${who(actor.id).handle}`;
}

/// An agent's picture, or the letters that stand in for it.
///
/// The bytes come over the bridge (`avatars.ts`) because this page's scheme
/// handler is static-only. A face that has not loaded — or an agent with no
/// avatar, which is most of them — is the monogram native computed for the
/// whole team; an actor who is not an agent gets a plain disc, filled for the
/// operator and hairline for the board.
function Avatar({ actor, who }: { actor: Actor | null; who: (id: string) => Person }) {
  const person = actor !== null && actor.kind === "agent" ? who(actor.id) : null;
  const blob = person?.avatar;
  const [url, setUrl] = useState<string | null>(null);

  useEffect(() => {
    if (blob === undefined) {
      setUrl(null);
      return;
    }
    let live = true;
    avatarUrl(blob)
      .then((next) => {
        if (live) setUrl(next);
      })
      .catch(() => {
        // A face that will not load is a monogram, not an error: the card is
        // about what was said, and nothing here is worth a broken-image icon.
      });
    return () => {
      live = false;
    };
  }, [blob]);

  if (url !== null) return <img className="issue-face" src={url} alt="" />;
  return (
    <span className={`issue-face ${actor?.kind ?? "system"}`} aria-hidden="true">
      {person?.monogram ?? ""}
    </span>
  );
}

type Translate = ReturnType<typeof useTranslation>["t"];

/// One machinery line, in words.
///
/// Every branch reads its fields defensively — this is raw gateway JSON, and a
/// kind that gained a field, lost one, or was added last week must still
/// produce a line. The fallback prints the kind itself, which is more useful
/// than a blank row and infinitely more useful than a thrown render.
function describe(event: IssueEvent, t: Translate): string {
  const body = event.body;
  const str = (key: string): string => (typeof body[key] === "string" ? body[key] : "");
  const num = (key: string): number => (typeof body[key] === "number" ? body[key] : 0);
  switch (body.kind) {
    case "opened":
      return t("issue.eventOpened");
    case "moved":
      return t("issue.eventMoved", {
        from: t(`issue.status.${str("from")}`, { defaultValue: str("from") }),
        to: t(`issue.status.${str("to")}`, { defaultValue: str("to") }),
      });
    case "run_started":
      return t("issue.eventRunStarted", { attempt: num("attempt") });
    case "run_settled":
      return t("issue.eventRunSettled", { attempt: num("attempt"), status: str("status") });
    case "cancelled":
      return t("issue.eventCancelled");
    case "branch_merged":
      return t("issue.eventMerged", { branch: str("branch"), into: str("into") });
    default:
      return body.kind.replace(/_/g, " ");
  }
}

/// `HH:MM`, or `MM-DD HH:MM` on an earlier day — the transcript's rule, so the
/// two surfaces read the same clock.
function shortTime(ms: number): string {
  const d = new Date(ms);
  const now = new Date();
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  const sameDay =
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate();
  if (sameDay) return `${hh}:${mm}`;
  const mo = String(d.getMonth() + 1).padStart(2, "0");
  const dd = String(d.getDate()).padStart(2, "0");
  return `${mo}-${dd} ${hh}:${mm}`;
}
