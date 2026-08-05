import { useState, type FormEvent } from 'react';
import { useNavigate } from 'react-router-dom';

import { Button } from '../../components/Button';
import { useAdminClient, useAuth } from '../../api/auth';
import { createProject } from './api';
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
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (submitting) return;
    setSubmitting(true);
    setError(null);
    const trimmedWorkdir = workdir.trim();
    const outcome = await createProject(client, {
      name,
      description,
      // An omitted directory is the signal to make one; sending an empty
      // string would be a different, invalid request.
      ...(trimmedWorkdir.length > 0 ? { workdir: trimmedWorkdir } : {}),
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
