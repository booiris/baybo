import { useState, type FormEvent } from 'react';
import { useNavigate } from 'react-router-dom';

import { Button } from '../../components/Button';
import { useAdminClient, useAuth } from '../../api/auth';
import { createProject } from './api';
import {
  BUDGET_REFUSAL,
  TOKEN_BUDGET_REFUSAL,
  budgetHint,
  parseBudget,
  parseTokenBudget,
  tokenBudgetHint,
} from './budgetModel';
import { parallelIssueRunsHint, parseParallelIssueRuns } from './driverModel';
import { writeLastProjectId } from './lastProject';

export const fieldLabel = 'block text-[0.7rem] font-bold uppercase tracking-wider text-ink-soft mb-1';

export const textInput =
  'w-full px-3 py-2 bg-white border-2 border-black rounded-md shadow-brutal-xs font-mono text-[0.9rem] outline-none disabled:opacity-60 disabled:bg-canvas';

export function CreateProjectForm({ onDone }: { onDone?: () => void }) {
  const client = useAdminClient();
  const { logout } = useAuth();
  const navigate = useNavigate();
  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [workdir, setWorkdir] = useState('');
  const [budget, setBudget] = useState('');
  const [tokenBudget, setTokenBudget] = useState('');
  const [parallelIssueRuns, setParallelIssueRuns] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (submitting) return;
    setSubmitting(true);
    setError(null);
    const trimmedWorkdir = workdir.trim();
    const micros = parseBudget(budget);
    if (micros === undefined) {
      setSubmitting(false);
      setError(BUDGET_REFUSAL);
      return;
    }
    const tokens = parseTokenBudget(tokenBudget);
    if (tokens === undefined) {
      setSubmitting(false);
      setError(TOKEN_BUDGET_REFUSAL);
      return;
    }
    const slots = parseParallelIssueRuns(parallelIssueRuns);
    if (parallelIssueRuns.trim() !== '' && slots === undefined) {
      setSubmitting(false);
      setError('Parallel issue runs must be a whole number, or empty for the default.');
      return;
    }
    const outcome = await createProject(client, {
      name,
      description,
      ...(trimmedWorkdir.length > 0 ? { workdir: trimmedWorkdir } : {}),
      ...(micros === null ? {} : { daily_budget_micros: micros }),
      ...(tokens === null ? {} : { daily_budget_tokens: tokens }),
      ...(slots === undefined ? {} : { max_parallel_issue_runs: slots }),
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
    writeLastProjectId(outcome.value.id);
    onDone?.();
    navigate(`/projects/${encodeURIComponent(outcome.value.id)}`);
  }

  return (
    <form
      onSubmit={(event) => {
        void submit(event);
      }}
      className="flex flex-col gap-3"
    >
      <div>
        <label className={fieldLabel} htmlFor="project-name">
          Name
        </label>
        <input
          id="project-name"
          className={textInput}
          value={name}
          autoFocus
          onChange={(event) => {
            setName(event.target.value);
          }}
          placeholder="baybo"
        />
      </div>
      <div>
        <label className={fieldLabel} htmlFor="project-description">
          Description
        </label>
        <input
          id="project-description"
          className={textInput}
          value={description}
          onChange={(event) => {
            setDescription(event.target.value);
          }}
          placeholder="What this project is about"
        />
      </div>
      <div>
        <label className={fieldLabel} htmlFor="project-workdir">
          Working directory — optional
        </label>
        <input
          id="project-workdir"
          className={textInput}
          value={workdir}
          onChange={(event) => {
            setWorkdir(event.target.value);
          }}
          placeholder="Leave empty to create one under work/"
        />
        <p className="mt-1 text-[0.7rem] text-ink-soft leading-snug">
          An absolute path to an existing git repository. Left empty, one is created and
          initialised for you.
        </p>
      </div>
      <div>
        <label className={fieldLabel} htmlFor="project-budget">
          Daily budget — optional
        </label>
        <input
          id="project-budget"
          className={textInput}
          value={budget}
          inputMode="decimal"
          onChange={(event) => {
            setBudget(event.target.value);
          }}
          placeholder="Leave empty for no ceiling"
        />
        <p className="mt-1 text-[0.7rem] text-ink-soft leading-snug">
          {budgetHint(parseBudget(budget) ?? null)} You can change it later.
        </p>
      </div>
      <div>
        <label className={fieldLabel} htmlFor="project-token-budget">
          Daily token budget — optional
        </label>
        <input
          id="project-token-budget"
          className={textInput}
          value={tokenBudget}
          inputMode="numeric"
          onChange={(event) => {
            setTokenBudget(event.target.value);
          }}
          placeholder="Leave empty for no ceiling"
        />
        <p className="mt-1 text-[0.7rem] text-ink-soft leading-snug">
          {tokenBudgetHint(parseTokenBudget(tokenBudget) ?? null)} You can change it later.
        </p>
      </div>
      <div>
        <label className={fieldLabel} htmlFor="project-parallel-issue-runs">
          Parallel issue runs — optional
        </label>
        <input
          id="project-parallel-issue-runs"
          className={textInput}
          value={parallelIssueRuns}
          inputMode="numeric"
          onChange={(event) => {
            setParallelIssueRuns(event.target.value);
          }}
          placeholder="Leave empty for the default"
        />
        <p className="mt-1 text-[0.7rem] text-ink-soft leading-snug">
          {parallelIssueRunsHint(parseParallelIssueRuns(parallelIssueRuns))} You can change it later.
        </p>
      </div>
      {error != null ? (
        <div className="bg-white border-2 border-err text-err rounded-md px-3 py-2 font-mono text-[0.78rem] break-words">
          {error}
        </div>
      ) : null}
      <div className="flex justify-end gap-2">
        {onDone !== undefined ? (
          <Button type="button" onClick={onDone}>
            Cancel
          </Button>
        ) : null}
        <Button type="submit" variant="primary" disabled={submitting || name.trim().length === 0}>
          {submitting ? 'Creating…' : 'Create project'}
        </Button>
      </div>
    </form>
  );
}
