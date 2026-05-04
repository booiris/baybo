import { useState } from 'react';
import {
  RiArrowDownSLine,
  RiArrowRightSLine,
  RiBrainLine,
  RiRobot2Line,
  RiToolsLine,
  RiUser3Line,
  RiCpuLine,
} from 'react-icons/ri';
import type { ChatMessage, ContentBlock, Role, SecretKind } from '../../types/trace';
import { renderWithSanitizeChips } from './SanitizeChip';

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

function previewLine(blocks: ContentBlock[]): string {
  for (const b of blocks) {
    if ('Text' in b) return b.Text.slice(0, 160);
    if ('ToolUse' in b) return `→ ${b.ToolUse.name}(...)`;
    if ('ToolResult' in b) return `← result(${b.ToolResult.tool_use_id})`;
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

function ContentBlockView({
  block,
  kindHint,
}: {
  block: ContentBlock;
  kindHint?: SecretKind;
}) {
  // Tool / structured blocks default-collapsed (they're noisy).
  const isStructured =
    'ToolUse' in block ||
    'ToolResult' in block ||
    'Thinking' in block ||
    'Image' in block ||
    'Audio' in block ||
    'File' in block;
  const [open, setOpen] = useState(!isStructured);

  if ('Text' in block) {
    return <ClippedText text={block.Text} kindHint={kindHint} />;
  }

  let label = '';
  let body: React.ReactNode = null;
  if ('ToolUse' in block) {
    label = `tool_use → ${block.ToolUse.name}`;
    body = (
      <pre className="whitespace-pre-wrap break-all font-mono text-[0.8rem] text-ink-soft bg-gray-50 border-2 border-black rounded-md p-2">
        {JSON.stringify(block.ToolUse.input, null, 2)}
      </pre>
    );
  } else if ('ToolResult' in block) {
    label = `tool_result (${block.ToolResult.tool_use_id})`;
    body = (
      <pre className="whitespace-pre-wrap break-words font-mono text-[0.8rem] text-ink bg-gray-50 border-2 border-black rounded-md p-2">
        {renderWithSanitizeChips(block.ToolResult.content, kindHint)}
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
    <div className="border-2 border-black rounded-md bg-canvas">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="w-full flex items-center gap-2 px-2 py-1 text-left text-[0.75rem] font-bold uppercase tracking-wider text-ink-soft hover:text-ink cursor-pointer"
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
  index,
  kindHint,
}: {
  message: ChatMessage;
  index: number;
  kindHint?: SecretKind;
}) {
  const meta = ROLE_META[message.role];
  const Icon = meta.icon;
  // Default collapsed for system messages (always boilerplate) and any
  // message past the first 5 in long threads.
  const [open, setOpen] = useState(message.role !== 'system' && index < 5);

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
        <span className="ml-auto text-ink-soft text-[0.8rem] font-mono truncate max-w-[60%]">
          {previewLine(message.content)}
        </span>
      </button>
      {open && (
        <div className="px-3 pb-3 pt-1 space-y-2 border-t border-black/20">
          {message.content.map((c, i) => (
            <ContentBlockView key={i} block={c} kindHint={kindHint} />
          ))}
        </div>
      )}
    </div>
  );
}

export function MessageList({
  messages,
  kindHint,
}: {
  messages: ChatMessage[];
  kindHint?: SecretKind;
}) {
  if (messages.length === 0) {
    return <div className="text-ink-soft text-[0.85rem]">No messages.</div>;
  }
  return (
    <div className="space-y-2">
      <div className="text-[0.7rem] text-ink-soft uppercase font-bold tracking-wider">
        <RiBrainLine className="inline mr-1" />
        {messages.length} {messages.length === 1 ? 'message' : 'messages'}
      </div>
      {messages.map((m, i) => (
        <MessageCard key={i} message={m} index={i} kindHint={kindHint} />
      ))}
    </div>
  );
}
