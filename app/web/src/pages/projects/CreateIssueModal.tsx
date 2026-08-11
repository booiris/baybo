import { useEffect, useState } from 'react';

import { Button } from '../../components/Button';
import { useAdminClient, useAuth } from '../../api/auth';
import { createIssue } from './api';
import { Avatar } from './Avatar';
import { Picker } from './Picker';
import {
  COLUMN_LABEL,
  STATUS_PILL,
  type Agent,
  type IssuePriority,
  type IssueStatus,
} from './boardModel';
import {
  assigneeOptions,
  priorityOptions,
  statusOptions,
  PRIORITY_LABEL,
  PRIORITY_TONE,
} from './issueFields';
import type { Portrait } from './portrait';

/// A chip in the property row. Pressable, and looking it — the row is where
/// everything about the card that is not prose gets set, and until these were
/// buttons they were native `<select>`s drawing an operating-system menu in
/// the middle of a hand-drawn modal.
const pill =
  'inline-flex items-center gap-1.5 border-2 border-black rounded-full px-2.5 py-1 font-mono text-[0.66rem] font-bold bg-surface hover:bg-brand/25';

export function CreateIssueModal({
  projectId,
  status: initialStatus,
  agents,
  portrait,
  parents,
  onClose,
  onCreated,
}: {
  projectId: string;
  /// Pre-filled from the column whose `+` was pressed — a starting point,
  /// not a lock: the mockup's chips all open their picker.
  status: IssueStatus;
  agents: Agent[];
  /// Resolved faces, from the board's one lookup — the assignee chip shows
  /// who, not a handle to read.
  portrait: Portrait;
  /// Cards this one could be a step of. One level only, so a card that is
  /// already a step is not offered.
  parents: { number: number; title: string }[];
  onClose: () => void;
  onCreated: () => void;
}) {
  const client = useAdminClient();
  const { logout } = useAuth();
  const [title, setTitle] = useState('');
  const [description, setDescription] = useState('');
  const [priority, setPriority] = useState<IssuePriority>('none');
  const [assignee, setAssignee] = useState('');
  const [keepOpen, setKeepOpen] = useState(false);
  const [status, setStatus] = useState<IssueStatus>(initialStatus);
  const [parent, setParent] = useState<number | null>(null);
  const [stage, setStage] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  useEffect(() => {
    function onKeyDown(event: KeyboardEvent) {
      if (event.key === 'Escape') onClose();
    }
    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('keydown', onKeyDown);
    };
  }, [onClose]);

  const picked = agents.find((agent) => agent.id === assignee);
  const needsAssignee = status === 'in_progress' && assignee.length === 0;
  const willEnqueue = status === 'in_progress' && assignee.length > 0;

  async function submit() {
    if (submitting || title.trim().length === 0 || needsAssignee) return;
    setSubmitting(true);
    setError(null);
    const outcome = await createIssue(client, projectId, {
      title,
      description,
      status,
      priority,
      ...(assignee.length > 0 ? { assignee } : {}),
      ...(parent == null ? {} : { parent, stage }),
    });
    setSubmitting(false);
    if (outcome.kind === 'unauthorized') {
      logout();
      return;
    }
    if (outcome.kind === 'failed') {
      setError(outcome.message);
      return;
    }
    if (keepOpen) {
      setTitle('');
      setDescription('');
      return;
    }
    onCreated();
  }

  return (
    <div
      className="fixed inset-0 z-40 bg-black/40 flex items-start justify-center p-10 overflow-y-auto"
      onClick={onClose}
    >
      <div
        className="w-full max-w-xl bg-surface border-[3px] border-black rounded-md shadow-brutal"
        onClick={(event) => {
          event.stopPropagation();
        }}
        onKeyDown={(event) => {
          if (event.key === 'Enter' && (event.metaKey || event.ctrlKey)) {
            void submit();
          }
        }}
      >
        <div className="flex items-center gap-2 px-4 pt-3 pb-1 font-mono text-[0.66rem] text-ink-soft">
          <span>New issue</span>
          <span>›</span>
          <span className="font-bold text-ink">{COLUMN_LABEL[status]}</span>
          <button
            type="button"
            className="ml-auto cursor-pointer px-1"
            onClick={onClose}
            aria-label="Close"
          >
            ✕
          </button>
        </div>

        <div className="px-4 pb-4 flex flex-col gap-3">
          <input
            className="w-full bg-transparent font-mono text-[1.05rem] font-bold outline-none placeholder:text-ink-soft/60"
            placeholder="Issue title"
            value={title}
            autoFocus
            onChange={(event) => {
              setTitle(event.target.value);
            }}
          />
          <textarea
            className="w-full min-h-[110px] bg-transparent font-sans text-[0.85rem] outline-none resize-y placeholder:text-ink-soft/60"
            placeholder="Add description…"
            value={description}
            onChange={(event) => {
              setDescription(event.target.value);
            }}
          />
          {/* Status, priority, assignee, parent — the design's order, and the
              board's own vocabulary in each: the status pill's column colour,
              the priority's tone, the assignee's face. */}
          <div className="flex flex-wrap gap-2">
            <Picker
              label="Status"
              value={status}
              disabled={submitting}
              options={statusOptions}
              onPick={(next) => {
                setStatus(next as IssueStatus);
              }}
              triggerClassName={`${pill} !border-black ${STATUS_PILL[status]}`}
            >
              {COLUMN_LABEL[status]}
            </Picker>
            <Picker
              label="Priority"
              value={priority}
              disabled={submitting}
              options={priorityOptions}
              onPick={(next) => {
                setPriority(next as IssuePriority);
              }}
              triggerClassName={`${pill} ${PRIORITY_TONE[priority]}`}
            >
              {PRIORITY_LABEL[priority]}
            </Picker>
            <Picker
              label="Assignee"
              value={assignee}
              disabled={submitting}
              options={assigneeOptions(agents, portrait)}
              onPick={setAssignee}
              triggerClassName={pill}
            >
              {picked === undefined ? (
                <span className="text-ink-soft">Unassigned</span>
              ) : (
                <span className="inline-flex min-w-0 items-center gap-1.5">
                  <Avatar handle={picked.handle} src={portrait(picked.id)} size="sm" />
                  <span className="truncate">@{picked.handle}</span>
                </span>
              )}
            </Picker>
            {/* One chip, as the design draws it: a step's stage is meaningless
                without the parent it is a step of, so the two never appear
                apart. */}
            <span className={`${pill} hover:bg-surface`}>
              <span className="text-ink-soft">⌗</span>
              <Picker
                label="Parent"
                value={parent == null ? '' : String(parent)}
                disabled={submitting}
                options={[
                  { value: '', label: 'No parent' },
                  ...parents.map((candidate) => ({
                    value: String(candidate.number),
                    label: `#${candidate.number} ${candidate.title}`,
                  })),
                ]}
                onPick={(next) => {
                  setParent(next === '' ? null : Number(next));
                }}
                triggerClassName="max-w-[10rem] hover:text-brand-hover"
              >
                <span className="truncate">{parent == null ? 'Parent' : `#${parent}`}</span>
              </Picker>
              {parent == null ? null : (
                <label className="inline-flex items-center gap-1 border-l border-black/25 pl-1.5">
                  <span className="text-ink-soft">stage</span>
                  <input
                    type="number"
                    min={0}
                    value={stage}
                    onChange={(event) => {
                      setStage(Math.max(0, Number(event.target.value)));
                    }}
                    className="w-9 bg-transparent outline-none font-bold"
                  />
                </label>
              )}
            </span>
          </div>
          {needsAssignee ? (
            <p className="font-mono text-[0.62rem] font-bold text-warn">
              In Progress needs an assignee — pick who is on it.
            </p>
          ) : willEnqueue ? (
            // Creating into In Progress with somebody on it starts an agent
            // and spends money. The modal says so before the button is
            // pressed rather than reporting it afterwards in a toast.
            <p className="border-2 border-black bg-brand/25 rounded-md px-2 py-1 font-mono text-[0.62rem] font-bold">
              ⚡ In Progress + @{picked?.handle ?? '?'} — creating this starts a run.
            </p>
          ) : null}
          {error != null ? (
            <div className="bg-white border-2 border-err text-err rounded-md px-3 py-2 font-mono text-[0.75rem] break-words">
              {error}
            </div>
          ) : null}
        </div>

        <div className="flex items-center gap-3 px-4 py-3 border-t-2 border-black bg-canvas">
          <label className="flex items-center gap-1.5 font-mono text-[0.66rem] text-ink-soft cursor-pointer">
            <input
              type="checkbox"
              checked={keepOpen}
              onChange={(event) => {
                setKeepOpen(event.target.checked);
              }}
            />
            Continue create
          </label>
          <Button
            className="ml-auto !px-4 !py-1.5 !text-[0.78rem]"
            variant="primary"
            disabled={submitting || title.trim().length === 0 || needsAssignee}
            onClick={() => {
              void submit();
            }}
          >
            {submitting ? 'Creating…' : 'Create issue ⌘↵'}
          </Button>
        </div>
      </div>
    </div>
  );
}
