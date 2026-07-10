// Popover attached to the sidebar's New-chat button: lets the user bind
// the new session to an agent profile instead of the builtin. Purely
// presentational — the caller owns open/close state and the actual
// `POST /v1/chat/sessions` call.

export interface AgentOption {
  id: string;
  name: string;
  description: string;
  builtin: boolean;
  framework: string;
}

export function AgentPicker({
  agents,
  onPick,
  onClose,
}: {
  agents: AgentOption[];
  onPick: (agentId: string | null) => void;
  onClose: () => void;
}) {
  const ordered = [...agents].sort((a, b) =>
    a.builtin === b.builtin ? a.name.localeCompare(b.name) : a.builtin ? -1 : 1,
  );
  return (
    <div
      role="menu"
      className="absolute z-30 mt-1 w-full bg-canvas border-2 border-black rounded-md shadow-brutal-sm p-1 flex flex-col gap-1"
      onMouseLeave={onClose}
    >
      {ordered.map((a) => {
        const external = a.framework !== 'baybo';
        return (
          <button
            key={a.id}
            type="button"
            disabled={external}
            title={external ? 'External-framework chat is not supported yet' : a.description}
            onClick={() => onPick(a.builtin ? null : a.id)}
            className="flex items-center gap-2 px-2 py-1.5 text-left rounded hover:bg-brand/20 disabled:opacity-40 disabled:cursor-not-allowed"
          >
            <span className="w-6 h-6 shrink-0 rounded-full border-2 border-black bg-brand/40 flex items-center justify-center text-[0.7rem] font-bold uppercase">
              {a.name.slice(0, 1)}
            </span>
            <span className="min-w-0">
              <span className="block text-sm font-bold truncate">{a.name}</span>
              {a.description ? (
                <span className="block text-[0.7rem] text-ink-soft truncate">{a.description}</span>
              ) : null}
            </span>
          </button>
        );
      })}
    </div>
  );
}
