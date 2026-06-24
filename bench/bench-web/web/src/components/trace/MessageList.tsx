import { useMemo, useState } from 'react';
import {
  RiArrowDownSLine,
  RiArrowRightSLine,
  RiBrainLine,
  RiCornerDownLeftLine,
  RiRobot2Line,
  RiToolsLine,
  RiUser3Line,
  RiCpuLine,
} from 'react-icons/ri';
import type { ChatMessage, ContentBlock, Role, SecretKind } from '../../types/trace';
import { renderWithSanitizeChips } from './SanitizeChip';
import { optimizeToolValue, stripToolOutputWrapper, useOptimizedToolIo } from '../../lib/toolIo';

const ROLE_META: Record<
  Role,
  { label: string; icon: React.ComponentType<{ className?: string }>; accent: string }
> = {
  system: { label: 'System', icon: RiCpuLine, accent: 'text-ink-soft' },
  user: { label: 'User', icon: RiUser3Line, accent: 'text-brand' },
  assistant: { label: 'Assistant', icon: RiRobot2Line, accent: 'text-ok' },
  tool: { label: 'Tool', icon: RiToolsLine, accent: 'text-warn' },
};

const TEXT_CLIP_THRESHOLD = 2_000;

function previewLine(blocks: ContentBlock[], toolNames: Map<string, string>): string {
  for (const b of blocks) {
    if ('Text' in b) return b.Text.slice(0, 160);
    if ('ToolUse' in b) return `→ ${b.ToolUse.name}(...)`;
    if ('ToolResult' in b) {
      const name = toolNames.get(b.ToolResult.tool_use_id);
      return name ? `← ${name} result` : `← result(${b.ToolResult.tool_use_id})`;
    }
    if ('Thinking' in b) return '(thinking)';
    if ('Image' in b) return '(image)';
    if ('Audio' in b) return '(audio)';
    if ('File' in b) return `(file: ${b.File.filename})`;
  }
  return '(empty)';
}

function ClippedText({
  text,
  kindHint,
}: {
  text: string;
  kindHint?: SecretKind;
}) {
  const [open, setOpen] = useState(false);
  if (text.length <= TEXT_CLIP_THRESHOLD) {
    return (
      <pre className="whitespace-pre-wrap break-words font-mono text-[0.85rem] leading-relaxed text-ink">
        {renderWithSanitizeChips(text, kindHint)}
      </pre>
    );
  }
  const visible = open ? text : `${text.slice(0, TEXT_CLIP_THRESHOLD)}\n…`;
  return (
    <div>
      <pre className="whitespace-pre-wrap break-words font-mono text-[0.85rem] leading-relaxed text-ink">
        {renderWithSanitizeChips(visible, kindHint)}
      </pre>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="mt-1 text-[0.75rem] uppercase tracking-wider font-bold text-brand hover:underline cursor-pointer"
      >
        {open ? 'Collapse' : `Show full (${text.length.toLocaleString()} chars)`}
      </button>
    </div>
  );
}

/**
 * A tool result is an error when baybo's `<tool_output name="X">` wrapper's
 * body starts with `Error:` (validation rejections + tool execution
 * failures). Success bodies never do.
 */
export function isToolResultError(content: string): boolean {
  return content
    .replace(/^<tool_output[^>]*>\s*/, '')
    .trimStart()
    .startsWith('Error:');
}

function ContentBlockView({
  block,
  kindHint,
  toolNames,
}: {
  block: ContentBlock;
  kindHint?: SecretKind;
  toolNames: Map<string, string>;
}) {
  // Tool / structured blocks default-collapsed (they're noisy)…
  const isStructured =
    'ToolUse' in block ||
    'ToolResult' in block ||
    'Thinking' in block ||
    'Image' in block ||
    'Audio' in block ||
    'File' in block;
  // …except a failed tool result, which opens by default + is flagged red
  // so errors are visible in the transcript without a click, and thinking,
  // which reads as part of the assistant's turn and so shows expanded.
  const isToolError = 'ToolResult' in block && isToolResultError(block.ToolResult.content);
  const [open, setOpen] = useState(!isStructured || isToolError || 'Thinking' in block);
  const optimized = useOptimizedToolIo();

  if ('Text' in block) {
    return <ClippedText text={block.Text} kindHint={kindHint} />;
  }

  let label = '';
  let body: React.ReactNode = null;
  if ('ToolUse' in block) {
    label = `tool_use → ${block.ToolUse.name}`;
    body = (
      <pre
        className={`whitespace-pre-wrap font-mono text-[0.8rem] text-ink-soft bg-gray-50 border-2 border-black rounded-md p-2 ${
          optimized ? 'break-words' : 'break-all'
        }`}
      >
        {optimized
          ? optimizeToolValue(block.ToolUse.input)
          : JSON.stringify(block.ToolUse.input, null, 2)}
      </pre>
    );
  } else if ('ToolResult' in block) {
    const name = toolNames.get(block.ToolResult.tool_use_id) ?? block.ToolResult.tool_use_id;
    label = isToolError ? `tool_result ✕ ${name} (error)` : `tool_result ← ${name}`;
    const content = optimized
      ? stripToolOutputWrapper(block.ToolResult.content)
      : block.ToolResult.content;
    body = (
      <pre
        className={`whitespace-pre-wrap break-words font-mono text-[0.8rem] bg-gray-50 border-2 border-black rounded-md p-2 ${
          isToolError ? 'text-err' : 'text-ink'
        }`}
      >
        {renderWithSanitizeChips(content, kindHint)}
      </pre>
    );
  } else if ('Thinking' in block) {
    label = 'thinking';
    body = (
      <div className="space-y-1">
        {block.Thinking.content.map((c, i) => (
          <pre
            key={i}
            className="whitespace-pre-wrap break-words font-mono text-[0.8rem] text-ink-soft bg-gray-50 border-2 border-black rounded-md p-2 italic"
          >
            {c.kind === 'text'
              ? c.text
              : c.kind === 'summary'
                ? c.text
                : '(redacted)'}
          </pre>
        ))}
      </div>
    );
  } else if ('Image' in block) {
    label = `image (${block.Image.mime_type})`;
    body = <span className="font-mono text-[0.8rem]">blob: {block.Image.blob.blob_id}</span>;
  } else if ('Audio' in block) {
    label = `audio (${block.Audio.mime_type})`;
    body = <span className="font-mono text-[0.8rem]">blob: {block.Audio.blob.blob_id}</span>;
  } else if ('File' in block) {
    label = `file: ${block.File.filename}`;
    body = (
      <span className="font-mono text-[0.8rem]">
        {block.File.mime_type} — blob: {block.File.blob.blob_id}
      </span>
    );
  }

  return (
    <div
      className={`border-2 rounded-md ${isToolError ? 'border-err bg-err/5' : 'border-black bg-canvas'}`}
    >
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className={`w-full flex items-center gap-2 px-2 py-1 text-left text-[0.75rem] font-bold uppercase tracking-wider cursor-pointer ${
          isToolError ? 'text-err hover:text-err' : 'text-ink-soft hover:text-ink'
        }`}
      >
        {open ? <RiArrowDownSLine /> : <RiArrowRightSLine />}
        {label}
      </button>
      {open && <div className="px-2 pb-2">{body}</div>}
    </div>
  );
}

function MessageCard({
  message,
  isCurrentInput,
  kindHint,
  toolNames,
}: {
  message: ChatMessage;
  isCurrentInput: boolean;
  kindHint?: SecretKind;
  toolNames: Map<string, string>;
}) {
  const meta = ROLE_META[message.role];
  const Icon = meta.icon;
  // Only the trailing run of non-assistant messages — the new prompt or
  // tool results that triggered this LLM call — is open by default.
  // Prior turns and system boilerplate start collapsed.
  const [open, setOpen] = useState(message.role !== 'system' && isCurrentInput);

  return (
    <div className="border-2 border-black rounded-md bg-white shadow-brutal-xs">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="w-full flex items-center gap-3 px-3 py-2 text-left hover:bg-gray-50 cursor-pointer"
      >
        {open ? <RiArrowDownSLine className="text-ink-soft" /> : <RiArrowRightSLine className="text-ink-soft" />}
        <Icon className={`text-base ${meta.accent}`} />
        <span className="font-bold uppercase tracking-wider text-[0.75rem]">{meta.label}</span>
        {message.source === 'user_interjection' && (
          <span
            title="Sent mid-turn while the agent was working (steering); folded into this turn's input"
            className="inline-flex items-center gap-1 border-2 border-black rounded bg-warn/15 px-1.5 py-0.5 text-[0.6rem] font-bold uppercase tracking-wider text-warn"
          >
            <RiCornerDownLeftLine className="text-[0.7rem]" />
            Interjected
          </span>
        )}
        <span className="ml-auto text-ink-soft text-[0.8rem] font-mono truncate max-w-[60%]">
          {previewLine(message.content, toolNames)}
        </span>
      </button>
      {open && (
        <div className="px-3 pb-3 pt-1 space-y-2 border-t border-black/20">
          {message.content.map((c, i) => (
            <ContentBlockView key={i} block={c} kindHint={kindHint} toolNames={toolNames} />
          ))}
        </div>
      )}
    </div>
  );
}

export function MessageList({
  messages,
  kindHint,
  foldHistory = true,
}: {
  messages: ChatMessage[];
  kindHint?: SecretKind;
  /**
   * Hide everything before the trailing non-assistant run behind a
   * "show earlier messages" toggle. Right for one LLM call's inputs
   * (focus on the new prompt); wrong for a full session transcript,
   * where every turn should render as its own visible card.
   */
  foldHistory?: boolean;
}) {
  // Build a lookup so ToolResult blocks can show the tool name they
  // pair with — the wire only carries `tool_use_id` on results.
  const toolNames = useMemo(() => {
    const m = new Map<string, string>();
    for (const msg of messages) {
      for (const block of msg.content) {
        if ('ToolUse' in block) m.set(block.ToolUse.id, block.ToolUse.name);
      }
    }
    return m;
  }, [messages]);

  // First index of the trailing run of non-assistant messages — the new
  // prompt or tool results that drove this LLM call. Everything before
  // is prior history and stays hidden behind a toggle.
  const currentInputStart = useMemo(() => {
    if (!foldHistory) return 0;
    for (let i = messages.length - 1; i >= 0; i--) {
      if (messages[i].role === 'assistant') return i + 1;
    }
    return 0;
  }, [messages, foldHistory]);

  const [showHistory, setShowHistory] = useState(false);

  if (messages.length === 0) {
    return <div className="text-ink-soft text-[0.85rem]">No messages.</div>;
  }

  const priorMessages = messages.slice(0, currentInputStart);
  const currentMessages = messages.slice(currentInputStart);

  return (
    <div className="space-y-2">
      <div className="text-[0.7rem] text-ink-soft uppercase font-bold tracking-wider">
        <RiBrainLine className="inline mr-1" />
        {messages.length} {messages.length === 1 ? 'message' : 'messages'}
      </div>
      {priorMessages.length > 0 && (
        <>
          <button
            type="button"
            onClick={() => setShowHistory((v) => !v)}
            className="w-full flex items-center gap-2 px-3 py-2 text-left text-[0.75rem] font-bold uppercase tracking-wider text-ink-soft hover:text-ink border-2 border-dashed border-black/40 rounded-md bg-canvas hover:bg-gray-50 cursor-pointer"
          >
            {showHistory ? (
              <RiArrowDownSLine className="text-ink-soft" />
            ) : (
              <RiArrowRightSLine className="text-ink-soft" />
            )}
            {showHistory ? 'Hide' : 'Show'} {priorMessages.length} earlier{' '}
            {priorMessages.length === 1 ? 'message' : 'messages'}
          </button>
          {showHistory &&
            priorMessages.map((m, i) => (
              <MessageCard
                key={`prior-${i}`}
                message={m}
                isCurrentInput={false}
                kindHint={kindHint}
                toolNames={toolNames}
              />
            ))}
        </>
      )}
      {currentMessages.map((m, i) => (
        <MessageCard
          key={`current-${i}`}
          message={m}
          isCurrentInput={true}
          kindHint={kindHint}
          toolNames={toolNames}
        />
      ))}
    </div>
  );
}
