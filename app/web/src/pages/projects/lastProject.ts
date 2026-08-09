
const LAST_PROJECT_KEY = 'baybo.projects.last';

export function readLastProjectId(): string | null {
  try {
    return window.localStorage.getItem(LAST_PROJECT_KEY);
  } catch {
    return null;
  }
}

export function writeLastProjectId(projectId: string): void {
  try {
    window.localStorage.setItem(LAST_PROJECT_KEY, projectId);
  } catch {
    /* storage disabled; the rail falls back to most-recent for this session */
  }
}
