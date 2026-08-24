import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { AttachmentBubble } from "../attachments";
import { currentConnEpoch } from "../bridge";
import { MarkdownBody } from "../Markdown";
import {
  openIssue,
  openRun,
  pickField,
  postActivityAtBottom,
  postDescription,
  postIssueRendered,
  subscribeIssue,
  type IssuePayload,
} from "./bridge";
import { fold, type Fold } from "./timeline";
import {
  actorHandle,
  isLiveRun,
  toWireAttachment,
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
export function IssuePage() {
  const { t } = useTranslation();
  const [payload, setPayload] = useState<IssuePayload | null>(null);
  const [bottomInset, setBottomInset] = useState(0);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const renderedFor = useRef<string | null>(null);

  useEffect(
    () =>
      subscribeIssue({
        init: (p) => setBottomInset(p.bottomInset),
        deliver: (p) => setPayload(p),
        bottomInset: (px) => setBottomInset(px),
        language: () => {
          /* i18n is switched by main.tsx, which owns the instance */
        },
        setEditing: (active) => setEditing(active),
        jumpToLatest: () => {
          const el = scrollRef.current;
          if (el) el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
        },
      }),
    [],
  );

  // The editor opens on whatever the card currently says, and a card that
  // changes underneath an open editor must not yank the text out from under
  // the typing hand — so the draft is seeded once per opening, not per render.
  useEffect(() => {
    if (editing) setDraft(payload?.issue.description ?? "");
  }, [editing, payload?.issue.description]);

  /// Read is stamped only once the card has actually painted, and once per
  /// card: a card whose timeline threw has not been read, and re-stamping on
  /// every delivery would spend a round trip per comment.
  useEffect(() => {
    if (!payload) return;
    const key = `${payload.issue.project_id}#${payload.issue.number}`;
    if (renderedFor.current === key) return;
    renderedFor.current = key;
    postIssueRendered();
  }, [payload]);

  const onScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    postActivityAtBottom(el.scrollHeight - el.scrollTop - el.clientHeight < 48);
  }, []);

  if (!payload) {
    return <div className="issue-loading">{t("issue.loading")}</div>;
  }

  const { issue, events, runs, handles } = payload;
  // Ids in, handles out. The DTOs carry profile ids and only the board
  // knows the team, so the map arrives on the payload and every name on
  // this page goes through it — an id that resolves to nothing prints as
  // itself, which is what the gateway does too.
  const handle = (id: string): string => handles[id] ?? id;
  const liveRun = runs.find(isLiveRun) ?? null;

  return (
    <div
      className="issue-page"
      ref={scrollRef}
      onScroll={onScroll}
      style={{ paddingBottom: `${String(bottomInset + 24)}px` }}
    >
      <IssueHead issue={issue} assigneeHandle={issue.assignee !== undefined ? handle(issue.assignee) : null} />
      {issue.blocked_reason !== undefined && issue.blocked_reason !== "" && (
        <div className="issue-blocked" role="note">
          <span className="issue-blocked-label">{t("issue.blocked")}</span>
          <span>{issue.blocked_reason}</span>
        </div>
      )}
      {liveRun !== null && <RunRow run={liveRun} who={handle(liveRun.agent_id)} />}
      <Description
        issue={issue}
        editing={editing}
        draft={draft}
        onDraft={setDraft}
        onDone={(text) => postDescription(text)}
      />
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
      <Runs runs={runs} handle={handle} />
      <Activity events={events} />
    </div>
  );
}

function IssueHead({
  issue,
  assigneeHandle,
}: {
  issue: IssueDetail;
  assigneeHandle: string | null;
}) {
  const { t } = useTranslation();
  const cancelled = issue.cancelled_at_ms !== undefined;
  return (
    <header className="issue-head">
      <h1 className={cancelled ? "cancelled" : undefined}>{issue.title}</h1>
      <div className="issue-chips">
        <button type="button" className="issue-chip" onClick={() => pickField("status")}>
          {t(`issue.status.${issue.status}`)}
        </button>
        <button type="button" className="issue-chip" onClick={() => pickField("priority")}>
          {t(`issue.priority.${issue.priority}`)}
        </button>
        <button type="button" className="issue-chip" onClick={() => pickField("assignee")}>
          {assigneeHandle !== null ? `@${assigneeHandle}` : t("issue.unassigned")}
        </button>
        {issue.branch !== undefined && issue.branch !== "" && (
          <span className="issue-chip static">⑂ {issue.branch}</span>
        )}
        {issue.parent !== undefined && (
          <button type="button" className="issue-chip" onClick={() => openIssue(issue.parent ?? 0)}>
            ↳ #{issue.parent}
          </button>
        )}
      </div>
    </header>
  );
}

/// The live run's own row: what is happening and how long it has been.
function RunRow({ run, who }: { run: IssueRun; who: string }) {
  const { t } = useTranslation();
  return (
    <div className={`issue-run ${run.status}`}>
      <span className="issue-run-word">{t(`issue.run.${run.status}`)}</span>
      <span className="issue-run-who">@{who}</span>
      <button type="button" className="issue-run-open" onClick={() => openRun(run.attempt)}>
        {t("issue.openRun")}
      </button>
    </div>
  );
}

/// The description, and the editor that replaces it.
///
/// A plain `<textarea>` over the raw markdown, deliberately not a
/// contenteditable: the source is what gets PATCHed, and a rich editor would
/// have to round-trip markdown through a DOM and back — which is where the
/// list that silently loses its nesting comes from.
function Description({
  issue,
  editing,
  draft,
  onDraft,
  onDone,
}: {
  issue: IssueDetail;
  editing: boolean;
  draft: string;
  onDraft: (text: string) => void;
  onDone: (text: string) => void;
}) {
  const { t } = useTranslation();
  const sent = useRef(false);

  // Native's dock owns Done; it flips `editing` back off, and that edge is what
  // sends. Guarded so a re-render during the transition cannot send twice.
  useEffect(() => {
    if (editing) {
      sent.current = false;
      return;
    }
    if (sent.current) return;
    sent.current = true;
  }, [editing]);

  if (editing) {
    return (
      <section className="issue-section">
        <h2>{t("issue.description")}</h2>
        <textarea
          className="issue-editor"
          value={draft}
          onChange={(e) => onDraft(e.target.value)}
          onBlur={() => onDone(draft)}
          autoFocus
          spellCheck={false}
        />
      </section>
    );
  }

  return (
    <section className="issue-section">
      <h2>{t("issue.description")}</h2>
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
              <span className={`issue-sub-dot ${sub.status}`} aria-hidden="true" />
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

function Runs({ runs, handle }: { runs: IssueRun[]; handle: (id: string) => string }) {
  const { t } = useTranslation();
  if (runs.length === 0) return null;
  return (
    <section className="issue-section">
      <h2>{t("issue.runs")}</h2>
      <ul className="issue-runs">
        {runs.map((run) => (
          <li key={`${String(run.attempt)}-${String(run.created_at_ms)}`}>
            <button type="button" onClick={() => openRun(run.attempt)}>
              <span className="issue-run-attempt">#{run.attempt}</span>
              <span className={`issue-run-status ${run.status}`}>
                {t(`issue.run.${run.status}`)}
              </span>
              <span className="issue-run-who">@{handle(run.agent_id)}</span>
              {run.error !== undefined && run.error !== "" && (
                <span className="issue-run-error">{run.error}</span>
              )}
            </button>
          </li>
        ))}
      </ul>
    </section>
  );
}

/// The card's Activity: what people said, with the machinery folded away.
function Activity({ events }: { events: IssueEvent[] }) {
  const { t } = useTranslation();
  const folded = useMemo(() => fold(events), [events]);
  if (folded.length === 0) return null;
  return (
    <section className="issue-section">
      <h2>{t("issue.activity")}</h2>
      <ol className="issue-activity">
        {folded.map((item, i) => (
          <FoldRow key={rowKey(item, i)} item={item} />
        ))}
      </ol>
    </section>
  );
}

function rowKey(item: Fold, index: number): string {
  return item.kind === "entry" ? item.event.id : `sys-${item.events[0]?.id ?? String(index)}`;
}

function FoldRow({ item }: { item: Fold }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);

  if (item.kind === "entry") return <EntryRow event={item.event} />;

  if (item.events.length === 1 || open) {
    return (
      <>
        {item.events.map((event) => (
          <EntryRow key={event.id} event={event} />
        ))}
      </>
    );
  }
  return (
    <li className="issue-fold">
      <button type="button" onClick={() => setOpen(true)}>
        {t("issue.nEvents", { count: item.events.length })} ›
      </button>
    </li>
  );
}

function EntryRow({ event }: { event: IssueEvent }) {
  const { t } = useTranslation();
  const who = actorHandle(event.actor);
  const isComment = event.body.kind === "comment";
  const text = typeof event.body.text === "string" ? event.body.text : "";
  const attachments = Array.isArray(event.body.attachments) ? event.body.attachments : [];

  return (
    <li className={`issue-entry ${isComment ? "comment" : "system"}`}>
      <div className="issue-entry-head">
        <span className="issue-entry-who">{who !== null ? `@${who}` : t("issue.system")}</span>
        <span className="issue-entry-time">{shortTime(event.created_at_ms)}</span>
      </div>
      {isComment ? (
        <>
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
        </>
      ) : (
        // An unrecognised kind renders as its own name rather than throwing:
        // the gateway adds kinds on its own schedule, and a card whose Activity
        // died because of one would take the comments with it.
        <span className="issue-entry-line">{describe(event, t)}</span>
      )}
    </li>
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
