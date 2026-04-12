---
name: review-doc
description: Review a design document under docs/ by running local `codex exec` for commentary, then critically judge codex's feedback against the real codebase and propose any needed doc edits for user approval.
argument-hint: "<path to doc under docs/, e.g. docs/modules/agent.md>"
allowed-tools: Bash(codex exec:*) Bash(ls:*) Bash(test:*) Bash(cat:*) Read Grep Glob Edit
---

# Review Design Document

You are reviewing a design document in `docs/` by combining a second opinion from the local `codex` CLI with your own grounded analysis of the real codebase. Your job is **not** to blindly forward codex's feedback — you must judge each point, separate the correct critiques from the wrong ones, and surface clear, actionable insights to the user.

## Inputs

- `$1` — path to the doc to review (e.g. `docs/modules/agent.md`). This is **required**.

If `$1` is missing or the file does not exist, stop and ask the user which doc to review.

## Step 1: Locate and read the doc

1. Verify the file exists with `test -f <path>`.
2. Read the full document with the `Read` tool.
3. Note which crate/module the doc governs (check `docs/modules/README.md` mapping if unclear) so you can ground-truth claims later.

## Step 2: Invoke codex for a design critique

Run `codex exec` non-interactively with a focused review prompt. Use read-only sandbox so codex cannot modify anything. Example:

```bash
codex exec --sandbox read-only "Please review the design document at <path>. Be specific and cite line numbers or sections when possible. Output a structured list of findings, each with: severity (high/medium/low), location, issue, and suggested fix."
```

Notes:

- Prefer passing the prompt as an argument. If it is long, pipe via stdin instead.
- Do NOT use `--dangerously-bypass-approvals-and-sandbox`.
- If codex fails to run (not installed, auth error, timeout), report the error to the user and stop — do not fall back to reviewing on your own without telling them.

Capture codex's full output.

## Step 3: Judge each finding

For **every** point codex raises, you must independently verify it. Do not trust codex by default — it may hallucinate file paths, misread the spec, or flag issues that are actually intentional.

For each finding, do the following:

1. **Re-read the relevant section** of the doc.
2. **Check the actual code** in the corresponding crate (`Grep`, `Read`, `Glob`). Remember: per `CLAUDE.md`, docs in `docs/modules/` are the source of truth for design, but the code may have already implemented (or diverged from) the spec.
3. Classify the finding as one of:
   - **Correct** — the critique is valid and the doc should change.
   - **Partially correct** — there is a real issue but codex's framing or suggested fix is off; note what is actually wrong.
   - **Incorrect** — codex misread the doc or the code; explain why it is wrong with evidence.
   - **Out of scope** — valid observation but not about this doc (e.g. it's a code issue, not a spec issue).

Be concrete. Cite `file:line` when you reference the codebase or doc.

## Step 4: Synthesize your own insights

Beyond codex's findings, add anything **you** noticed while verifying that codex missed — inconsistencies with other module specs, gaps vs. `CLAUDE.md` principles (modular, extensible, secure, governable, observable, reliable), unclear trait boundaries, missing error paths, etc. Mark these clearly as **Additional insights (not from codex)**.

## Step 5: Report to the user

Produce a single, structured report:

```
# Review of <doc path>

## Codex findings
1. [Correct / Partially correct / Incorrect / Out of scope] <short title>
   - Codex said: <summary>
   - My verification: <evidence from code/doc, with file:line refs>
   - Verdict: <what should actually happen, if anything>

2. ...

## Additional insights (not from codex)
- <point>: <evidence and reasoning>

## Recommended doc changes
- [ ] <change 1> — at <section/line>
- [ ] <change 2> — at <section/line>

(If no changes are needed, say so explicitly and stop here.)
```

Keep it tight — one or two sentences per bullet. No filler.

## Step 6: Ask before editing

If the recommended-changes list is non-empty, **do not edit the doc yet**. Present the proposed edits as concrete before/after snippets (or a short diff sketch per change) and ask the user:

> "Apply these changes? (yes / only #N,#M / no / describe changes to make instead)"

Wait for the user's decision. Then:

- `yes` → apply all recommended edits with the `Edit` tool.
- `only #N,#M` → apply only the selected ones.
- `no` → stop without editing and acknowledge.
- otherwise → incorporate the user's guidance and re-propose before editing.

## Rules

- Never edit the doc in Step 5 — editing only happens after user approval in Step 6.
- Never invoke codex with a write-capable sandbox.
- Do not paste codex's raw output verbatim into the final report — always filter through your own judgment.
- If codex and your own analysis both find nothing actionable, say so plainly and stop. Do not invent issues to justify the review.
- Ground every claim in either the doc or the codebase; avoid speculative critiques.
