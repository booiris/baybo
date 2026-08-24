// How many issue runs a board starts by itself, as the settings forms read
// and write it.
//
// There is no upper bound to mirror: how much a board may start at once is
// the operator's call, and the server refuses only what it cannot represent
// (a negative). So this parses the shape and nothing else.

export function parseParallelIssueRuns(text: string): number | undefined {
  const trimmed = text.trim();
  if (trimmed === '') return undefined;
  if (!/^\d+$/.test(trimmed)) return undefined;
  const value = Number(trimmed);
  return Number.isSafeInteger(value) ? value : undefined;
}

export function formatParallelIssueRuns(slots: number | null | undefined): string {
  return slots == null ? '' : String(slots);
}

// Whether the board's agents land their own branches. Says what each
// setting *does*, because "agents may merge" alone reads as a permission
// with no consequence, and the consequence is the whole point: with it on,
// a reviewed card's branch reaches the repository without anyone running
// git. It is not a lock — a run carries a shell and a writable checkout —
// so the off state promises what it can keep and no more.
export function agentsMayMergeHint(may: boolean): string {
  return may
    ? 'On: once a card is reviewed, its assignee merges the branch into whatever the repository is checked out on, and the card records where it went.'
    : 'Off: IssueMerge refuses, and a finished card hands over its branch for someone to merge.';
}

export function parallelIssueRunsHint(slots: number | undefined): string {
  if (slots === undefined) return 'A whole number of issue runs.';
  if (slots === 0) {
    return 'Off: cards stay in Todo until you move them into In Progress yourself.';
  }
  const runs = slots === 1 ? '1 run' : `${slots} runs`;
  return `The board keeps ${runs} going, taking the most urgent staffed card out of Todo whenever one finishes.`;
}
