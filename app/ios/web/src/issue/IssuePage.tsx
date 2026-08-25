import {
  Fragment,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
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
  type Person,
} from "./bridge";
import { avatarUrl } from "./avatars";
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
export function IssuePage() {
  const { t } = useTranslation();
  const [payload, setPayload] = useState<IssuePayload | null>(null);
  const [bottomInset, setBottomInset] = useState(0);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const renderedFor = useRef<string | null>(null);
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

  const onScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    postActivityAtBottom(el.scrollHeight - el.scrollTop - el.clientHeight < 48);
  }, []);

  if (!payload) {
    return <div className="issue-loading">{t("issue.loading")}</div>;
  }

  const { issue, events, runs, people } = payload;
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
  const opened = events.find((e) => e.body.kind === "opened") ?? null;
  // And the entry is then HOISTED rather than repeated: the description's own
  // head already says "@who opened this card", so leaving the line in the
  // Activity would print the same fact twice, a screen apart.
  const timeline = opened === null ? events : events.filter((e) => e.id !== opened.id);

  return (
    <div
      className="issue-page"
      ref={scrollRef}
      onScroll={onScroll}
      // `pointerdown`, not `scroll`: the auto-scroll below fires a scroll of
      // its own, and a guard that could not tell the two apart would disarm
      // itself on the very first thing it does.
      onPointerDown={() => (grabbed.current = true)}
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
        author={opened?.actor ?? null}
        who={who}
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
      <Activity events={timeline} landing={landing} who={who} />
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
  author,
  who,
  editing,
  draft,
  onDraft,
  onDone,
}: {
  issue: IssueDetail;
  author: Actor | null;
  who: (id: string) => Person;
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

  return (
    <section className="issue-section">
      <Post
        actor={author}
        who={who}
        title={author !== null ? speaker(author, who, t) : t("issue.description")}
        at={issue.created_at_ms}
        said={author !== null ? t("issue.eventOpened") : undefined}
      >
        {editing ? (
          <textarea
            className="issue-editor"
            value={draft}
            onChange={(e) => onDraft(e.target.value)}
            onBlur={() => onDone(draft)}
            autoFocus
            spellCheck={false}
          />
        ) : issue.description === "" ? (
          <p className="issue-empty">{t("issue.noDescription")}</p>
        ) : (
          <MarkdownBody text={issue.description} />
        )}
      </Post>
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
///
/// `landing` is where the operator stopped reading, and the rule drawn above
/// that row is the only thing on this page that says so. It is not a filter:
/// everything above it is still there, because a card is a record and the
/// point of arriving mid-thread is to be able to scroll back up out of it.
function Activity({
  events,
  landing,
  who,
}: {
  events: IssueEvent[];
  landing: string | null;
  who: (id: string) => Person;
}) {
  const { t } = useTranslation();
  const folded = useMemo(() => fold(events, landing ?? undefined), [events, landing]);
  if (folded.length === 0) return null;
  return (
    <section className="issue-section">
      <h2>{t("issue.activity")}</h2>
      <ol className="issue-activity">
        {folded.map((item, i) => (
          <Fragment key={rowKey(item, i)}>
            {foldHead(item)?.id === landing && (
              <li className="issue-unread-rule" data-unread-rule>
                {t("issue.unreadFrom")}
              </li>
            )}
            <FoldRow item={item} landed={foldHead(item)?.id === landing} who={who} />
          </Fragment>
        ))}
      </ol>
    </section>
  );
}

function rowKey(item: Fold, index: number): string {
  return item.kind === "entry" ? item.event.id : `sys-${item.events[0]?.id ?? String(index)}`;
}

/// One folded run of machinery, closed until asked.
///
/// **Every run, including one entry long.** A lone `moved` used to render in
/// full on the argument that "1 event ›" saves no space — but space was never
/// the point: a card's Activity is mostly machinery, and what buries the two
/// things a person said is a wall of rows that all look like rows. One
/// uniform closed line per run is what makes the comments findable, and it is
/// also what keeps the shape stable — splitting a run at the unread boundary
/// would otherwise turn a tidy `3 events ›` into a raw row plus `2 events ›`.
///
/// `landed` is the exception, and it has to be: the run carrying the unread
/// boundary is what the page just scrolled to, and landing a reader on a
/// closed line is landing them on nothing.
function FoldRow({
  item,
  landed,
  who,
}: {
  item: Fold;
  landed: boolean;
  who: (id: string) => Person;
}) {
  const { t } = useTranslation();
  /// The reader's own choice, once they make one — `null` means "follow the
  /// landing". Not seeded with `useState(landed)`: the boundary arrives with
  /// the live payload, a beat after the mirror has already mounted this row,
  /// and an initial value would be read before it exists.
  const [toggled, setToggled] = useState<boolean | null>(null);

  if (item.kind === "entry") return <EntryRow event={item.event} who={who} />;

  const open = toggled ?? landed;
  return (
    <>
      <li className="issue-fold">
        <button type="button" onClick={() => setToggled(!open)} aria-expanded={open}>
          {t("issue.nEvents", { count: item.events.length })} {open ? "⌄" : "›"}
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
    <li className="issue-entry comment">
      <Post
        actor={event.actor}
        who={who}
        title={speaker(event.actor, who, t)}
        at={event.created_at_ms}
      >
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
      </Post>
    </li>
  );
}

/// A face beside a bordered box: the shape a threaded issue has had since
/// before GitHub, and the reason a long card stays readable — the eye finds
/// the next thing somebody said by looking down one column, not by reading.
function Post({
  actor,
  who,
  title,
  at,
  said,
  children,
}: {
  /// Whose face goes beside the box. `null` draws the board's blank disc —
  /// the description of a card whose opening nothing recorded.
  actor: Actor | null;
  who: (id: string) => Person;
  /// The head's first word. Passed in rather than derived from `actor`
  /// because the description's box is titled by what it IS on a card nobody
  /// can be named the author of.
  title: string;
  at: number;
  /// What this box is, when that is not simply "they wrote this" — the
  /// description's box says "opened this card".
  said?: string;
  children: ReactNode;
}) {
  return (
    <div className="issue-post">
      <Avatar actor={actor} who={who} />
      <div className="issue-box">
        <div className="issue-box-head">
          <span className="issue-box-who">{title}</span>
          {said !== undefined && <span className="issue-box-said">{said}</span>}
          <span className="issue-box-when">{shortTime(at)}</span>
        </div>
        <div className="issue-box-body">{children}</div>
      </div>
    </div>
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
