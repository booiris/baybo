import { useCallback, useEffect, useState } from 'react';
import { Navigate } from 'react-router-dom';
import { RiLoader4Line } from 'react-icons/ri';

import { useAdminClient, useAuth } from '../../api/auth';
import { fetchProjects } from './api';
import { resolveLanding } from './boardModel';
import { readLastProjectId } from './lastProject';
import { CreateProjectForm } from './CreateProjectForm';

export function ProjectsIndex() {
  const client = useAdminClient();
  const { logout } = useAuth();
  const [state, setState] = useState<
    | { kind: 'loading' }
    | { kind: 'empty' }
    | { kind: 'go'; id: string }
    | { kind: 'failed'; message: string }
  >({ kind: 'loading' });

  const load = useCallback(async () => {
    const outcome = await fetchProjects(client, false);
    if (outcome.kind === 'unauthorized') {
      logout();
      return null;
    }
    if (outcome.kind === 'failed') {
      return { kind: 'failed' as const, message: outcome.message };
    }
    return resolveLanding(outcome.value, readLastProjectId());
  }, [client, logout]);

  useEffect(() => {
    let canceled = false;
    async function run() {
      const next = await load();
      if (canceled || next === null) return;
      setState(next);
    }
    void run();
    return () => {
      canceled = true;
    };
  }, [load]);

  if (state.kind === 'go') {
    return <Navigate to={`/projects/${encodeURIComponent(state.id)}`} replace />;
  }

  if (state.kind === 'loading') {
    return (
      <div className="flex flex-1 items-center justify-center">
        <RiLoader4Line className="text-3xl text-ink-soft animate-spin" />
      </div>
    );
  }

  if (state.kind === 'failed') {
    return (
      <div className="p-6">
        <div className="bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm break-words">
          {state.message}
        </div>
      </div>
    );
  }

  return (
    <div className="flex flex-1 items-center justify-center p-6">
      <div className="w-full max-w-lg">
        <h1 className="font-bold text-lg uppercase tracking-wider mb-1">Open a project</h1>
        <p className="text-ink-soft text-[0.85rem] mb-4">
          A project is a working directory and a board. Leave the directory empty and one is
          created for you.
        </p>
        <CreateProjectForm />
      </div>
    </div>
  );
}
