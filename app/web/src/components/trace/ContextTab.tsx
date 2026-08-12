/**
 * Where an LLM call's context went, as a matrix of cells.
 *
 * The provider reports one number for the whole prompt, which answers "what
 * did this cost" and not "what is eating my window". This draws the split: one
 * cell per fixed slice of tokens, coloured by the part it belongs to, laid out
 * against the model's context window when one is known so the free space is
 * visible too.
 *
 * The **total is exact** (the provider's own `input_tokens`) and the **split
 * is an estimate** — tiktoken is not Anthropic's tokenizer. The two are kept
 * visibly apart: the headline is the reported figure, every per-part number
 * carries a `≈`, and the drift against the estimate is stated rather than
 * hidden by silently showing one number in place of the other.
 */
import { useMemo, useState } from 'react';
import { RiLoader4Line } from 'react-icons/ri';
import type { ContextPart, SpanContext } from '../../types/trace';
import { formatTok } from './traceFormat';
import { buildContextGrid, largestSegments } from './contextGrid';

/** Colour + label per context part.
 *
 *  Hues follow the tree's existing vocabulary where the concepts line up —
 *  tools orange, memory blue, model output green — so a reader who has learnt
 *  the tree does not learn a second key. The related pairs share a hue at two
 *  strengths: standing instructions (system prompt / skills) violet, tool
 *  definitions and their results orange, recalled memory and attached media
 *  blue. */
const PART_VISUALS: Record<ContextPart, { label: string; cell: string; hint: string }> = {
  system_prompt: {
    label: 'System prompt',
    cell: 'bg-violet',
    hint: "The leading system row — persona, identity, workspace — plus the notices that amend it when its sources change on disk mid-session.",
  },
  skills: {
    label: 'Skills',
    cell: 'bg-violet/50',
    hint: 'The standing listing of invocable skills, and the notices that tell the model how the registry has changed since.',
  },
  tools: {
    label: 'Tool definitions',
    cell: 'bg-warn',
    hint: 'Every tool this call offered the model: name, description, and JSON schema. See the Tools tab for the list.',
  },
  tool_result: {
    label: 'Tool results',
    cell: 'bg-warn/50',
    hint: 'What the tools returned, carried forward as transcript.',
  },
  memory: {
    label: 'Recalled memory',
    cell: 'bg-info',
    hint: 'Memories pulled from long-term storage and injected to inform this turn.',
  },
  media: {
    label: 'Attachments',
    cell: 'bg-info/50',
    hint: 'Images, audio and files. Priced by the provider\u2019s own media rules (tiles, pages, seconds) rather than by a tokenizer, so this part is not an estimate in the same sense as the rest.',
  },
  user: {
    label: 'User messages',
    cell: 'bg-brand',
    hint: 'What a human actually typed, mid-turn interjections included.',
  },
  cron: {
    label: 'Cron',
    cell: 'bg-magenta',
    hint: "A scheduled job's framed prompt, or a one-shot fire's result delivered back into this conversation.",
  },
  assistant: {
    label: 'Assistant',
    cell: 'bg-ok',
    hint: "The model's own output carried forward as history, thinking blocks included.",
  },
  agent: {
    label: 'Agent-injected',
    cell: 'bg-ink-soft',
    hint: 'Everything else baybo puts in the conversation on the model\u2019s behalf: an invoked skill\u2019s body, a subagent task prompt or its finished notification, compaction instructions, framing reminders. Synthetic user-role rows a human never sent.',
  },
};

const FREE_CELL = 'bg-black/10';
const LARGEST_SHOWN = 8;

function pct(share: number): string {
  if (share <= 0) return '0%';
  return share < 0.001 ? '<0.1%' : `${(share * 100).toFixed(share < 0.1 ? 1 : 0)}%`;
}

export function ContextTab({
  context,
  loading,
  onRetry,
}: {
  context: SpanContext | undefined;
  loading: boolean;
  onRetry: () => void;
}) {
  const [hovered, setHovered] = useState<ContextPart | null>(null);
  const grid = useMemo(
    () =>
      context
        ? buildContextGrid(
            context.segments,
            context.reported_input_tokens ?? context.estimated_total_tokens,
            context.context_window ?? null,
          )
        : null,
    [context],
  );
  const largest = useMemo(
    () => (grid ? largestSegments(grid.segments, LARGEST_SHOWN) : []),
    [grid],
  );

  if (context == null || grid == null) {
    return loading ? (
      <div className="text-ink-soft text-[0.85rem] italic flex items-center gap-2">
        <RiLoader4Line className="animate-spin" /> Measuring the context…
      </div>
    ) : (
      <div className="space-y-3">
        <div className="text-ink-soft text-[0.85rem]">
          The context for this call could not be reconstructed.
        </div>
        <button
          type="button"
          onClick={onRetry}
          className="border-2 border-black rounded-md px-3 py-1 text-[0.8rem] font-bold uppercase tracking-wider bg-white hover:bg-gray-50 cursor-pointer"
        >
          Retry
        </button>
      </div>
    );
  }

  const used = context.reported_input_tokens ?? context.estimated_total_tokens;
  const measured = context.reported_input_tokens != null;
  // Only worth stating when it is big enough to change a reading. tiktoken
  // runs within ~10% of Anthropic's tokenizer, so a 3% gap is the tool
  // working, not a discrepancy the reader has to account for.
  const drift =
    measured && context.estimated_total_tokens > 0
      ? Math.abs(used - context.estimated_total_tokens) / context.estimated_total_tokens
      : 0;

  const cells: { key: string; className: string; title: string }[] = [];
  for (const category of grid.categories) {
    const visual = PART_VISUALS[category.part];
    for (let i = 0; i < category.cells; i++) {
      cells.push({
        key: `${category.part}-${i}`,
        className: visual.cell,
        title: `${visual.label} · ≈${category.tokens.toLocaleString()} tokens (${pct(category.share)})`,
      });
    }
  }
  for (let i = 0; i < grid.freeCells; i++) {
    cells.push({
      key: `free-${i}`,
      className: FREE_CELL,
      title:
        grid.freeTokens != null
          ? `Free · ${grid.freeTokens.toLocaleString()} tokens left in the window`
          : 'Free',
    });
  }

  return (
    <div className="space-y-5">
      <section>
        <div className="flex items-baseline justify-between gap-3 flex-wrap">
          <h4 className="font-bold uppercase tracking-wider text-[0.8rem]">Context</h4>
          <div className="font-mono text-[0.8rem]">
            <span className="font-bold">{used.toLocaleString()}</span>
            {context.context_window != null && (
              <span className="text-ink-soft"> / {context.context_window.toLocaleString()}</span>
            )}
            <span className="text-ink-soft"> tokens</span>
            {context.context_window != null && context.context_window > 0 && (
              <span className="ml-2 text-brand font-bold">
                {pct(used / context.context_window)} full
              </span>
            )}
          </div>
        </div>
        <div className="mt-2 flex flex-wrap gap-[2px] border-2 border-black rounded-md p-2 bg-canvas">
          {cells.map((c) => (
            <span
              key={c.key}
              title={c.title}
              className={`h-2.5 w-2.5 rounded-[1px] ${c.className}`}
            />
          ))}
        </div>
        <p className="mt-1 font-mono text-[0.65rem] text-ink-soft">
          one cell ≈ {formatTok(grid.tokensPerCell)} tokens
          {context.context_window == null && ' · no context window known for this model'}
        </p>
      </section>

      <section>
        {/* Every percentage on this panel is a share of what was SENT, never
            of the window — one denominator, so the legend and the per-item
            list below can be read against each other. The window shows up as
            the headline's "% full" and as the free cells, not as a second
            percentage. */}
        <div className="mb-1 flex items-baseline justify-between font-mono text-[0.65rem] uppercase tracking-wider text-ink-soft">
          <span>Part</span>
          <span>share of input</span>
        </div>
        {/* Real rows, not a CSS grid of `display: contents` wrappers. A
            `contents` element generates no box at all, so it has no hit area
            and can host neither a hover handler nor a `title` — the previous
            shape promised an explanation (dotted underline and all) that could
            never appear. Column alignment comes from the fixed widths instead,
            which also makes this list match the one below it. */}
        <div className="font-mono text-[0.8rem]">
          {grid.categories
            .filter((c) => c.cells > 0 || c.tokens > 0)
            .map((category) => {
              const visual = PART_VISUALS[category.part];
              return (
                <div
                  key={category.part}
                  title={visual.hint}
                  onMouseEnter={() => setHovered(category.part)}
                  onMouseLeave={() => setHovered(null)}
                  className={`flex items-center gap-3 py-0.5 px-1 -mx-1 rounded cursor-help ${
                    hovered === category.part ? 'bg-black/5' : ''
                  }`}
                >
                  <span
                    className={`h-3 w-3 shrink-0 rounded-[2px] border border-black/40 ${visual.cell}`}
                  />
                  <span className="flex-1 min-w-0 truncate">{visual.label}</span>
                  <span className="shrink-0 tabular-nums text-right">
                    ≈{category.tokens.toLocaleString()}
                  </span>
                  <span className="shrink-0 tabular-nums text-right text-ink-soft w-12">
                    {pct(category.share)}
                  </span>
                </div>
              );
            })}
          {grid.freeTokens != null && (
            <div className="flex items-center gap-3 py-0.5 px-1 -mx-1">
              <span className={`h-3 w-3 shrink-0 rounded-[2px] border border-black/40 ${FREE_CELL}`} />
              <span className="flex-1 min-w-0 truncate text-ink-soft">Free</span>
              <span className="shrink-0 tabular-nums text-right text-ink-soft">
                {grid.freeTokens.toLocaleString()}
              </span>
              {/* No percentage: this one is a share of the WINDOW, and printing
                  it in the same column as the shares above invites exactly the
                  comparison that does not hold. */}
              <span className="shrink-0 tabular-nums text-right text-ink-soft w-12">left</span>
            </div>
          )}
        </div>
        {/* A reserved line rather than a floating tooltip: the detail panel is
            a scroll container, so an absolutely-positioned bubble would clip at
            its edges, and the native `title` delay is long enough to read as
            "nothing happened". Fixed height, so nothing reflows on hover. */}
        <p className="mt-2 min-h-[2.5rem] text-[0.72rem] leading-snug text-ink-soft border-t border-black/10 pt-1.5">
          {hovered != null ? PART_VISUALS[hovered].hint : 'Hover a part to see what falls into it.'}
        </p>
      </section>

      {largest.length > 0 && (
        <section>
          <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">
            Largest pieces
          </h4>
          <div className="space-y-1">
            {largest.map((segment, i) => {
              const visual = PART_VISUALS[segment.part];
              return (
                <div
                  key={`${segment.part}-${segment.index}-${i}`}
                  className="flex items-center gap-2 font-mono text-[0.78rem]"
                  title={`${visual.label} · message #${segment.index + 1} of the input`}
                >
                  <span
                    className={`h-2.5 w-2.5 rounded-[1px] shrink-0 border border-black/40 ${visual.cell}`}
                  />
                  {/* The position, because the labels repeat: five `read_file`
                      calls produce five identically-named rows, and without
                      this there is no way to tell which one is the 40k one. */}
                  <span className="shrink-0 tabular-nums text-ink-soft w-8 text-right">
                    #{segment.index + 1}
                  </span>
                  <span className="flex-1 min-w-0 truncate">{segment.label}</span>
                  <span className="shrink-0 tabular-nums">
                    ≈{segment.tokens.toLocaleString()}
                  </span>
                  <span className="shrink-0 tabular-nums text-ink-soft w-12 text-right">
                    {pct(segment.share)}
                  </span>
                </div>
              );
            })}
          </div>
        </section>
      )}

      <p className="text-[0.72rem] text-ink-soft italic leading-snug">
        {measured
          ? 'The total is what the provider billed. The split is a tiktoken estimate — accurate enough for proportions, not for accounting.'
          : 'This call recorded no usage, so both the total and the split are tiktoken estimates.'}
        {drift > 0.15 &&
          ` The estimate came to ${context.estimated_total_tokens.toLocaleString()}, ${pct(drift)} off the billed total.`}
      </p>
    </div>
  );
}
