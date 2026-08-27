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

  useEffect(() => {
    if (payload?.timelineLive !== true) return;
    const key = `${payload.issue.project_id}#${payload.issue.number}`;
    if (renderedFor.current === key) return;
    renderedFor.current = key;
    postIssueRendered(targetId);
  }, [payload, targetId]);

  useEffect(() => {
    const anchor = payload?.firstUnread;
    if (anchor === undefined) return;
    setLanding((current) => current ?? anchor);
  }, [payload?.firstUnread]);

  useEffect(() => {
    if (landing === null || landedOn.current === landing) return;
    const el = scrollRef.current;
    const rule = el?.querySelector<HTMLElement>("[data-unread-rule]");
    if (!el || !rule) return;
    landedOn.current = landing;
    if (grabbed.current) return;
    rule.scrollIntoView({ block: "start" });
  }, [landing, payload]);

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
  const who = (id: string): Person => people[id] ?? { handle: id, monogram: "" };
  const handle = (id: string): string => who(id).handle;
  const liveRun = runs.find(isLiveRun) ?? null;
  const opened = activityEvents.find((e) => e.body.kind === "opened") ?? null;
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
  if (actor.kind === "user") return t("issue.you");
  if (actor.kind === "system") return t("issue.system");
  return `@${who(actor.id).handle}`;
}

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
