import { useEffect, useState } from 'react';
import { useParams, useNavigate } from 'react-router-dom';
import { RiArrowLeftLine, RiArrowRightSLine, RiArrowDownSLine, RiRobot2Line, RiUser3Line, RiToolsLine, RiBrainLine, RiCloseLine } from 'react-icons/ri';
import { IconButton } from '../components/IconButton';
import { getMockSessionDetail, type TraceSessionDetail, type TraceSpan } from '../api/mock';

function SpanNode({ span, depth = 0, onSelect, selectedId }: { span: TraceSpan, depth?: number, onSelect: (span: TraceSpan) => void, selectedId: string | null }) {
  const paddingLeft = depth * 1.5;
  const isSelected = span.id === selectedId;

  return (
    <div className="flex flex-col relative">
      <div 
        className={`border-[2px] border-black rounded-md p-3 cursor-pointer transition-shadow flex items-center gap-3 relative ${isSelected ? 'bg-brand/10 border-brand shadow-brutal-xs z-10' : 'bg-white hover:shadow-brutal-xs'}`}
        style={{ marginLeft: `${paddingLeft}rem` }}
        onClick={(e) => { e.stopPropagation(); onSelect(span); }}
      >
        <div className="w-8 h-8 rounded-full border-2 border-black flex items-center justify-center bg-white shrink-0 shadow-brutal-xs">
          {span.type === 'llm_call' ? <RiBrainLine className="text-brand" /> : <RiToolsLine className="text-warn" />}
        </div>
        <div className="flex-1 min-w-0">
          <div className="flex items-center justify-between">
            <span className="font-bold text-[0.9rem] uppercase tracking-wide truncate">{span.name}</span>
            <span className="text-[0.8rem] font-mono bg-gray-100 px-1.5 py-0.5 rounded border border-black">{span.durationMs}ms</span>
          </div>
          <div className="text-[0.8rem] text-ink-soft truncate font-mono mt-0.5">
            {span.type === 'llm_call' ? 'LLM Inference' : 'Tool Execution'}
          </div>
        </div>
      </div>

      {span.children && span.children.length > 0 && (
        <div className="mt-3 flex flex-col gap-3 relative">
          <div 
            className="absolute top-[-0.75rem] bottom-0 w-[2px] bg-black/20" 
            style={{ left: `${paddingLeft + 1}rem` }} 
          />
          {span.children.map(child => (
            <SpanNode 
              key={child.id} 
              span={child} 
              depth={depth + 1} 
              onSelect={onSelect} 
              selectedId={selectedId} 
            />
          ))}
        </div>
      )}
    </div>
  );
}

export function TraceSessionPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const [detail, setDetail] = useState<TraceSessionDetail | null>(null);
  const [expandedTurns, setExpandedTurns] = useState<Set<string>>(new Set());
  const [selectedSpan, setSelectedSpan] = useState<TraceSpan | null>(null);
  const [activeTab, setActiveTab] = useState<'io' | 'meta'>('io');

  useEffect(() => {
    if (id) {
      setDetail(getMockSessionDetail(id));
      setExpandedTurns(new Set());
    }
  }, [id]);

  const toggleTurn = (turnId: string) => {
    setExpandedTurns(prev => {
      const next = new Set(prev);
      if (next.has(turnId)) next.delete(turnId);
      else next.add(turnId);
      return next;
    });
  };

  if (!detail) return <div className="p-5">Loading...</div>;

  return (
    <div className="flex flex-col h-full overflow-hidden bg-canvas">
      <div className="p-5 shrink-0 flex items-center gap-4 bg-white border-b-[3px] border-black z-10">
        <IconButton onClick={() => navigate(-1)} aria-label="Go back">
          <RiArrowLeftLine />
        </IconButton>
        <div>
          <h2 className="text-[1.5rem] font-bold uppercase -tracking-[0.05em] leading-tight">
            TRACE DETAILS
          </h2>
          <div className="text-ink-soft text-[0.85rem] font-mono">
            Session: {detail.sessionId}
          </div>
        </div>
      </div>

      <div className="flex-1 flex overflow-hidden min-h-0">
        {/* Main Content Area */}
        <div className="flex-1 overflow-y-scroll p-5">
          <div className="max-w-4xl mx-auto space-y-6 pb-10">
            {detail.turns.map((turn, index) => {
              const isExpanded = expandedTurns.has(turn.id);
              return (
                <div key={turn.id} className="bg-white border-[3px] border-black rounded-md shadow-brutal flex flex-col transition-all">
                  <div 
                    className="p-4 cursor-pointer hover:bg-gray-50 flex items-start gap-3"
                    onClick={() => toggleTurn(turn.id)}
                  >
                    <div className="mt-1">
                      {isExpanded ? <RiArrowDownSLine className="text-xl" /> : <RiArrowRightSLine className="text-xl" />}
                    </div>
                    <div className="flex-1 min-w-0">
                      <div className="flex items-center justify-between mb-3">
                        <span className="bg-ink text-white text-[0.7rem] font-bold px-2 py-0.5 rounded-sm uppercase tracking-wider shadow-brutal-xs">
                          Turn {index + 1}
                        </span>
                        <span className="text-ink-soft text-[0.8rem] font-mono">{new Date(turn.timestamp).toLocaleTimeString()}</span>
                      </div>
                      <div className="mb-2">
                        <div className="flex items-start gap-2 text-[0.95rem] font-bold mb-2">
                          <RiUser3Line className="text-brand shrink-0 mt-0.5" />
                          <span className="break-words">{turn.userMessage}</span>
                        </div>
                        <div className="flex items-start gap-2 text-[0.9rem] text-ink-soft">
                          <RiRobot2Line className="shrink-0 mt-0.5" />
                          <span className="break-words">{turn.aiResponse}</span>
                        </div>
                      </div>
                    </div>
                  </div>

                  {isExpanded && (
                    <div className="border-t-[3px] border-black p-5 bg-gray-50 flex flex-col gap-3">
                      <h4 className="text-[0.85rem] font-bold uppercase tracking-wider mb-2 text-ink-soft flex items-center gap-2">
                        <span className="w-2 h-2 rounded-full bg-brand inline-block"></span>
                        Execution Spans
                      </h4>
                      {turn.spans.map(span => (
                        <SpanNode 
                          key={span.id} 
                          span={span} 
                          onSelect={setSelectedSpan} 
                          selectedId={selectedSpan?.id || null} 
                        />
                      ))}
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        </div>

        {/* Right Detail Panel */}
        {selectedSpan && (
          <div className="w-[450px] shrink-0 border-l-[3px] border-black bg-white flex flex-col z-20 shadow-[-4px_0_0_0_rgba(0,0,0,0.1)]">
            <div className="flex flex-col border-b-[3px] border-black bg-canvas">
              <div className="flex items-center justify-between p-4 pb-2">
                <div className="flex items-center gap-3">
                  <div className="w-10 h-10 rounded-full border-2 border-black flex items-center justify-center bg-white shadow-brutal-xs">
                    {selectedSpan.type === 'llm_call' ? <RiBrainLine className="text-brand text-xl" /> : <RiToolsLine className="text-warn text-xl" />}
                  </div>
                  <div>
                    <h3 className="font-bold uppercase tracking-wider leading-tight text-[1.1rem]">{selectedSpan.name}</h3>
                    <div className="text-ink-soft text-[0.85rem] font-mono">{selectedSpan.type} • {selectedSpan.durationMs}ms</div>
                  </div>
                </div>
                <IconButton onClick={() => setSelectedSpan(null)}>
                  <RiCloseLine className="text-xl" />
                </IconButton>
              </div>
              <div className="flex px-4 gap-6 relative top-[3px]">
                <button
                  className={`pb-2 font-bold uppercase tracking-wider text-[0.85rem] border-b-[3px] transition-colors cursor-pointer ${activeTab === 'io' ? 'border-brand text-ink' : 'border-transparent text-ink-soft hover:text-ink'}`}
                  onClick={() => setActiveTab('io')}
                >
                  I/O Data
                </button>
                <button
                  className={`pb-2 font-bold uppercase tracking-wider text-[0.85rem] border-b-[3px] transition-colors cursor-pointer ${activeTab === 'meta' ? 'border-brand text-ink' : 'border-transparent text-ink-soft hover:text-ink'}`}
                  onClick={() => setActiveTab('meta')}
                >
                  Metadata
                </button>
              </div>
            </div>
            
            <div className="flex-1 overflow-y-scroll p-5">
              {activeTab === 'io' ? (
                <div className="space-y-6">
                  <div>
                    <h4 className="font-bold uppercase tracking-wider text-[0.85rem] mb-2 border-b-2 border-black pb-1">Input</h4>
                    <pre className="bg-gray-50 border-[2px] border-black rounded-md p-4 text-[0.85rem] font-mono whitespace-pre-wrap break-all overflow-x-auto shadow-brutal-sm">
                      {typeof selectedSpan.input === 'string' ? selectedSpan.input : JSON.stringify(selectedSpan.input, null, 2)}
                    </pre>
                  </div>
                  <div>
                    <h4 className="font-bold uppercase tracking-wider text-[0.85rem] mb-2 border-b-2 border-black pb-1">Output</h4>
                    <pre className="bg-gray-50 border-[2px] border-black rounded-md p-4 text-[0.85rem] font-mono whitespace-pre-wrap break-all overflow-x-auto shadow-brutal-sm">
                      {typeof selectedSpan.output === 'string' ? selectedSpan.output : JSON.stringify(selectedSpan.output, null, 2)}
                    </pre>
                  </div>
                </div>
              ) : (
                <div className="space-y-6">
                  <div>
                    <h4 className="font-bold uppercase tracking-wider text-[0.85rem] mb-3 border-b-2 border-black pb-1">Execution Meta</h4>
                    <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-3 font-mono text-[0.85rem]">
                      <div className="contents">
                        <dt className="font-bold text-ink-soft">Span ID</dt>
                        <dd className="break-all">{selectedSpan.id}</dd>
                      </div>
                      <div className="contents">
                        <dt className="font-bold text-ink-soft">Status</dt>
                        <dd className="break-all font-bold uppercase" style={{ color: selectedSpan.status === 'success' ? 'var(--color-ok)' : 'var(--color-err)' }}>{selectedSpan.status}</dd>
                      </div>
                      <div className="contents">
                        <dt className="font-bold text-ink-soft">Duration</dt>
                        <dd className="break-all">{selectedSpan.durationMs}ms</dd>
                      </div>
                      {selectedSpan.meta && Object.entries(selectedSpan.meta).map(([key, value]) => (
                        <div key={key} className="contents">
                          <dt className="font-bold text-ink-soft">{key}</dt>
                          <dd className="break-all">{typeof value === 'object' ? JSON.stringify(value) : String(value)}</dd>
                        </div>
                      ))}
                    </dl>
                  </div>
                </div>
              )}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}