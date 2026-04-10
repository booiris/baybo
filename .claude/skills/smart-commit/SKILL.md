---
name: smart-commit
description: Summarize all uncommitted changes, intelligently split them into logical commits by topic/scope, and commit each group with a clear message.
argument-hint: "[--dry-run]"
disable-model-invocation: true
allowed-tools: Bash(git status *) Bash(git diff *) Bash(git log *) Bash(git add *) Bash(git reset *) Bash(git commit *) Bash(git stash *)
---

# Smart Commit

You are performing an intelligent commit workflow. Your job is to analyze all uncommitted changes (staged + unstaged + untracked), group them into logical commits, and create each commit with a clear message.

If the user passes `--dry-run`, only show the proposed commit plan without actually committing.

## Step 1: Gather information

Run these commands in parallel to understand the current state:

1. `git status` — see all changed/untracked files
2. `git diff` — unstaged changes (full diff)
3. `git diff --cached` — staged changes (full diff)
4. `git log --oneline -10` — recent commit style reference

For untracked files that might be relevant, read their contents to understand what they contain.

## Step 2: Analyze and group changes

Analyze every changed file and hunk. Group changes into **logical commits** based on:

- **Functional cohesion**: changes that work together to achieve one purpose go in one commit
- **Scope**: separate unrelated changes (e.g., a bug fix vs. a new feature vs. a refactor)
- **Crate/module boundary**: when changes span multiple crates, prefer grouping by the feature they serve, not by crate
- **Config/tooling changes**: group config, CI, or tooling changes separately unless they are tightly coupled with a code change

Guidelines:
- If ALL changes are closely related, a single commit is perfectly fine — do NOT force-split
- Each commit should be self-contained and not break the build
- Order commits so that dependencies come first
- Do NOT commit files that look like secrets (.env, credentials, tokens, keys)

## Step 3: Present the plan

Before committing, present a clear summary to the user:

```
Proposed commits:

1. <type>: <short description>
   Files: <list of files>
   
2. <type>: <short description>
   Files: <list of files>
```

Use conventional commit types: `feat`, `fix`, `refactor`, `docs`, `chore`, `test`, `style`, `perf`, `ci`, `build`.

If `--dry-run` was passed, stop here.

## Step 4: Execute commits

For each commit group, in order:

1. `git reset HEAD` to unstage everything (only before the first commit if anything is staged)
2. `git add <specific files>` — stage only the files for this commit
   - If you need to commit only part of a file, use `git add -p` is not available in non-interactive mode. Instead, consider whether the partial file can reasonably go into one commit or needs manual intervention. If it must be split and cannot be done non-interactively, inform the user.
3. `git commit -m "<message>"` with a well-crafted message:
   - First line: `<type>: <concise summary>` (under 72 chars)
   - Blank line
   - Body: brief explanation of what and why (if non-obvious)
   - Footer: `Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>`

Use a HEREDOC to pass the commit message:
```bash
git commit -m "$(cat <<'EOF'
<type>: <summary>

<optional body>

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>
EOF
)"
```

4. After each commit, run `git status` to verify the state is correct before proceeding to the next.

## Step 5: Summary

After all commits are done, show a final summary:

```
Done! Created N commit(s):

<short hash> <type>: <summary>
<short hash> <type>: <summary>
```

## Important rules

- NEVER use `git add -A` or `git add .` — always add specific files
- NEVER commit secrets or credentials
- NEVER use `--no-verify` or skip hooks
- NEVER amend existing commits — always create new ones
- If a pre-commit hook fails, fix the issue and retry with a NEW commit
- Respect the project's existing commit message style from `git log`
