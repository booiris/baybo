import { useEffect, useState } from 'react';

import { Button } from '../../components/Button';
import { useAdminClient, useAuth } from '../../api/auth';
import { createIssue } from './api';
import {
  COLUMNS,
  COLUMN_LABEL,
  PRIORITIES,
  type Agent,
  type IssuePriority,
  type IssueStatus,
} from './boardModel';

const PRIORITY_LABEL: Record<IssuePriority, string> = {
  urgent: 'Urgent',
  high: 'High',
  medium: 'Medium',
  low: 'Low',
  none: 'No priority',
};

const pill =
  'inline-flex items-center gap-1.5 border-2 border-black rounded-full px-3 py-1 font-mono text-[0.66rem] font-bold bg-surface cursor-pointer';

/// `◐` is not in Space Mono, so the status chip rendered as a stray glyph.
/// A filled circle is in the face and reads the same.
const STATUS_GLYPH = '●';

export function CreateIssueModal({
  projectId,
  status: initialStatus,
  agents,
  parents,
  onClose,
  onCreated,
}: {
  projectId: string;
  /// Pre-filled from the column whose `+` was pressed — a starting point,
  /// not a lock: the mockup's chips all open their picker.
  status: IssueStatus;
  agents: Agent[];
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
          <div className="flex flex-wrap gap-2">
            <label className={`${pill} bg-brand/35`}>
              <span aria-hidden="true">{STATUS_GLYPH}</span>
              <select
                aria-label="Status"
                className="bg-transparent outline-none font-bold cursor-pointer"
                value={status}
                onChange={(event) => {
                  setStatus(event.target.value as IssueStatus);
                }}
              >
                {COLUMNS.map((value) => (
                  <option key={value} value={value}>
                    {COLUMN_LABEL[value]}
                  </option>
                ))}
              </select>
            </label>
            <label className={pill}>
              <span className="text-ink-soft">Assignee</span>
              <select
                className="bg-transparent outline-none font-bold cursor-pointer max-w-[9rem]"
                value={assignee}
                onChange={(event) => {
                  setAssignee(event.target.value);
                }}
              >
                <option value="">Unassigned</option>
                {agents.map((agent) => (
                  <option key={agent.id} value={agent.id}>
                    @{agent.handle} — {agent.name}
                  </option>
                ))}
              </select>
            </label>
            <label className={pill}>
              <span className="text-ink-soft">⌗ Parent</span>
              <select
                className="bg-transparent outline-none font-bold cursor-pointer max-w-[8rem]"
                value={parent == null ? '' : String(parent)}
                onChange={(event) => {
                  const picked = event.target.value;
                  setParent(picked === '' ? null : Number(picked));
                }}
              >
                <option value="">None</option>
                {parents.map((candidate) => (
                  <option key={candidate.number} value={candidate.number}>
                    #{candidate.number} {candidate.title}
                  </option>
                ))}
              </select>
            </label>
            {parent == null ? null : (
              <label className={pill}>
                <span className="text-ink-soft">stage</span>
                <input
                  type="number"
                  min={0}
                  value={stage}
                  onChange={(event) => {
                    setStage(Math.max(0, Number(event.target.value)));
                  }}
                  className="w-10 bg-transparent outline-none font-bold"
                />
              </label>
            )}
            <label className={pill}>
              <span className="text-ink-soft">Priority</span>
              <select
                className="bg-transparent outline-none font-bold cursor-pointer"
                value={priority}
                onChange={(event) => {
                  setPriority(event.target.value as IssuePriority);
                }}
              >
                {PRIORITIES.map((value) => (
                  <option key={value} value={value}>
                    {PRIORITY_LABEL[value]}
                  </option>
                ))}
              </select>
            </label>
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
              ⚡ In Progress + @{agents.find((a) => a.id === assignee)?.handle ?? '?'} — creating
              this starts a run.
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
