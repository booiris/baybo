import { useState } from 'react';

import { useAdminClient, useAuth } from '../../api/auth';
import { setProjectArchived, updateProject } from './api';
import type { Project } from './boardModel';
import { budgetHint, formatBudget, parseBudget } from './budgetModel';
import { fieldLabel, textInput } from './CreateProjectForm';

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
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const archived = project.archived_at_ms != null;

  async function save() {
    const micros = parseBudget(budget);
    if (micros === undefined) {
      setError('Daily budget must be an amount in dollars, or empty for no ceiling.');
      return;
    }
    setBusy(true);
    setError(null);
    const outcome = await updateProject(client, project.id, {
      name,
      description,
      daily_budget_micros: micros,
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
          <label className={fieldLabel} htmlFor="settings-description">
            Description
          </label>
          <input
            id="settings-description"
            className={textInput}
            value={description}
            disabled={archived}
            onChange={(event) => {
              setDescription(event.target.value);
            }}
          />
        </div>

        <div>
          <label className={fieldLabel} htmlFor="settings-budget">
            Daily budget
          </label>
          <input
            id="settings-budget"
            className={textInput}
            value={budget}
            inputMode="decimal"
            disabled={archived}
            placeholder="Leave empty for no ceiling"
            onChange={(event) => {
              setBudget(event.target.value);
            }}
          />
          <p className="mt-1 text-[0.7rem] text-ink-soft leading-snug">
            {budgetHint(parseBudget(budget) ?? null)}
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
              void toggleArchive();
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
    </div>
  );
}
