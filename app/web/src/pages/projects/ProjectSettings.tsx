import { useState } from 'react';

import { useAdminClient, useAuth } from '../../api/auth';
import { setProjectArchived, updateProject } from './api';
import type { Project } from './boardModel';
import {
  BUDGET_REFUSAL,
  TOKEN_BUDGET_REFUSAL,
  budgetHint,
  formatBudget,
  formatTokenBudget,
  parseBudget,
  parseTokenBudget,
  tokenBudgetHint,
} from './budgetModel';
import {
  agentsMayMergeHint,
  formatParallelIssueRuns,
  parallelIssueRunsHint,
  parseParallelIssueRuns,
} from './driverModel';
import { fieldLabel, textInput } from './CreateProjectForm';
import { MarkdownEditor } from './MarkdownEditor';
import { activityFor, useBoardActivity } from './useBoardActivity';

export function ProjectSettings({
  project,
  onClose,
  onSaved,
}: {
  project: Project;
  onClose: () => void;
  onSaved: (project: Project) => void;
}) {
  const client = useAdminClient();
  const { logout } = useAuth();
  const [name, setName] = useState(project.name);
  const [description, setDescription] = useState(project.description);
  const [budget, setBudget] = useState(formatBudget(project.daily_budget_micros));
  const [tokenBudget, setTokenBudget] = useState(formatTokenBudget(project.daily_budget_tokens));
  const [parallelIssueRuns, setParallelIssueRuns] = useState(formatParallelIssueRuns(project.max_parallel_issue_runs));
  const [agentsMayMerge, setAgentsMayMerge] = useState(project.agents_may_merge);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // Archiving is asked about before it happens; un-archiving is not. Only one
  // of the two takes a board away from everyone looking at it, and a question
  // in front of the restorative direction is a question nobody reads.
  const [confirmingArchive, setConfirmingArchive] = useState(false);
  const archived = project.archived_at_ms != null;
  // What the board has already spent against the ceilings being typed. The
  // two fields are the only place a ceiling is chosen, and choosing one
  // blind is how a board ends up stopped by a number nobody meant — see
  // `STAYS_STOPPED`. Fetched on open, like the switcher's own meter, and
  // absent is absent: a board with no spend today is left out of the
  // response and must read as zero rather than as unknown.
  const spent = activityFor(useBoardActivity(true, 0), project.id);

  async function save() {
    const micros = parseBudget(budget);
    if (micros === undefined) {
      setError(BUDGET_REFUSAL);
      return;
    }
    const tokens = parseTokenBudget(tokenBudget);
    if (tokens === undefined) {
      setError(TOKEN_BUDGET_REFUSAL);
      return;
    }
    const slots = parseParallelIssueRuns(parallelIssueRuns);
    if (slots === undefined) {
      setError('Parallel issue runs must be a whole number. Use 0 to stop the board starting work.');
      return;
    }
    setBusy(true);
    setError(null);
    const outcome = await updateProject(client, project.id, {
      name,
      description,
      daily_budget_micros: micros,
      daily_budget_tokens: tokens,
      max_parallel_issue_runs: slots,
      agents_may_merge: agentsMayMerge,
    });
    setBusy(false);
    if (outcome.kind === 'unauthorized') {
      logout();
      return;
    }
    if (outcome.kind === 'failed') {
      setError(outcome.message);
      return;
    }
    onSaved(outcome.value);
    onClose();
  }

  async function toggleArchive() {
    setBusy(true);
    setError(null);
    const outcome = await setProjectArchived(client, project.id, !archived);
    setBusy(false);
    if (outcome.kind === 'unauthorized') {
      logout();
      return;
    }
    if (outcome.kind === 'failed') {
      setError(outcome.message);
      return;
    }
    onSaved(outcome.value);
    onClose();
  }

  return (
    <div className="fixed inset-0 z-40 bg-black/40 flex items-start justify-center p-6 overflow-y-auto">
      <div className="bg-surface border-[3px] border-black rounded-md shadow-brutal w-full max-w-md my-auto p-4 flex flex-col gap-3">
        <h2 className="font-mono text-sm font-bold">Project settings</h2>

        <div>
          <label className={fieldLabel} htmlFor="settings-name">
            Name
          </label>
          <input
            id="settings-name"
            className={textInput}
            value={name}
            disabled={archived}
            onChange={(event) => {
              setName(event.target.value);
            }}
          />
        </div>

        <div>
          {/* A `<span>`, not a `<label>`: what it names is a ProseMirror
              surface rather than a form control, and the editor carries the
              same words as its own `aria-label`. */}
          <span className={fieldLabel}>Description</span>
          <div
            className={`w-full min-h-[3.5rem] px-3 py-2 border-2 border-black rounded-md shadow-brutal-xs md-editor ${
              archived ? 'bg-canvas opacity-60' : 'bg-white'
            }`}
          >
            <MarkdownEditor
              initialValue={project.description}
              ariaLabel="Description"
              placeholder="What this project is about"
              editable={!archived}
              onChange={setDescription}
            />
          </div>
        </div>

        <div>
          <label className={fieldLabel} htmlFor="settings-budget">
            Daily budget (USD)
          </label>
          <input
            id="settings-budget"
            className={textInput}
            value={budget}
            inputMode="decimal"
            disabled={archived}
            placeholder="e.g. 5.00 — empty for no ceiling"
            onChange={(event) => {
              setBudget(event.target.value);
            }}
          />
          <p className="mt-1 text-[0.7rem] text-ink-soft leading-snug">
            {budgetHint(parseBudget(budget) ?? null, spent?.burn_micros ?? 0)}
          </p>
        </div>

        <div>
          <label className={fieldLabel} htmlFor="settings-token-budget">
            Daily token budget (tokens)
          </label>
          <input
            id="settings-token-budget"
            className={textInput}
            value={tokenBudget}
            inputMode="numeric"
            disabled={archived}
            placeholder="e.g. 2000000 — empty for no ceiling"
            onChange={(event) => {
              setTokenBudget(event.target.value);
            }}
          />
          <p className="mt-1 text-[0.7rem] text-ink-soft leading-snug">
            {tokenBudgetHint(parseTokenBudget(tokenBudget) ?? null, spent?.burn_tokens ?? 0)}
          </p>
        </div>

        <div>
          <label className={fieldLabel} htmlFor="settings-parallel-issue-runs">
            Parallel issue runs
          </label>
          <input
            id="settings-parallel-issue-runs"
            className={textInput}
            value={parallelIssueRuns}
            inputMode="numeric"
            disabled={archived}
            onChange={(event) => {
              setParallelIssueRuns(event.target.value);
            }}
          />
          <p className="mt-1 text-[0.7rem] text-ink-soft leading-snug">
            {parallelIssueRunsHint(parseParallelIssueRuns(parallelIssueRuns))}
          </p>
        </div>

        <div>
          <label className="flex items-center gap-1.5 font-mono text-[0.78rem] cursor-pointer">
            <input
              type="checkbox"
              checked={agentsMayMerge}
              disabled={archived}
              onChange={(event) => {
                setAgentsMayMerge(event.target.checked);
              }}
            />
            Agents may merge
          </label>
          <p className="mt-1 text-[0.7rem] text-ink-soft leading-snug">
            {agentsMayMergeHint(agentsMayMerge)}
          </p>
        </div>

        <p className="text-[0.7rem] text-ink-soft leading-snug break-words">
          Working directory: <span className="font-mono">{project.workdir}</span> — set once, at
          creation.
        </p>

        {error !== null ? (
          <p className="border-2 border-err text-err rounded-md px-2 py-1 font-mono text-[0.68rem] break-words">
            {error}
          </p>
        ) : null}

        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            disabled={busy}
            onClick={() => {
              if (archived) {
                void toggleArchive();
                return;
              }
              setConfirmingArchive(true);
            }}
            className={`border-2 rounded-md px-3 py-1 font-mono text-[0.68rem] disabled:opacity-50 ${
              archived ? 'border-black bg-brand' : 'border-warn text-warn bg-surface'
            }`}
          >
            {archived ? 'Unarchive' : 'Archive'}
          </button>
          <span className="font-mono text-[0.62rem] text-ink-soft">
            {archived
              ? 'An archived board is read-only. Its cards and history stay.'
              : 'Archiving hides the board and stops it taking work. Nothing is deleted.'}
          </span>
          <button
            type="button"
            onClick={onClose}
            className="ml-auto border-2 border-black rounded-md px-3 py-1 font-mono text-[0.68rem] bg-surface"
          >
            Close
          </button>
          {archived ? null : (
            <button
              type="button"
              disabled={busy || name.trim() === ''}
              onClick={() => {
                void save();
              }}
              className="border-2 border-black rounded-md px-3 py-1 font-mono text-[0.68rem] bg-brand disabled:opacity-50"
            >
              {busy ? 'Saving…' : 'Save'}
            </button>
          )}
        </div>
      </div>

      {confirmingArchive ? (
        <ArchiveConfirm
          name={project.name}
          busy={busy}
          onCancel={() => {
            setConfirmingArchive(false);
          }}
          onConfirm={() => {
            setConfirmingArchive(false);
            void toggleArchive();
          }}
        />
      ) : null}
    </div>
  );
}

/// What archiving costs, asked at the moment it is being decided. It sits over
/// the settings panel rather than replacing its buttons so the numbers being
/// archived stay on screen behind the question.
function ArchiveConfirm({
  name,
  busy,
  onCancel,
  onConfirm,
}: {
  name: string;
  busy: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Archive project"
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-6"
      onClick={onCancel}
    >
      <div
        className="w-full max-w-sm bg-surface border-[3px] border-black rounded-md shadow-brutal p-4 flex flex-col gap-3"
        onClick={(event) => {
          event.stopPropagation();
        }}
      >
        <h3 className="font-mono text-sm font-bold">Archive {name}?</h3>
        <p className="font-mono text-[0.68rem] leading-snug text-ink-soft">
          The board leaves the switcher and stops taking work — running cards finish, nothing new
          starts. Its cards, runs and history stay, and you can unarchive it from here.
        </p>
        <div className="flex justify-end gap-2">
          <button
            type="button"
            onClick={onCancel}
            className="border-2 border-black rounded-md px-3 py-1 font-mono text-[0.68rem] bg-surface"
          >
            Never mind
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={onConfirm}
            className="border-2 border-warn bg-warn text-white rounded-md px-3 py-1 font-mono text-[0.68rem] font-bold disabled:opacity-50"
          >
            Archive it
          </button>
        </div>
      </div>
    </div>
  );
}
