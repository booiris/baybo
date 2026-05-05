import { useSearchParams } from 'react-router-dom';
import type { components } from './schema';
import type {
  SessionReplay,
  Span,
  Step,
  StepKind,
  ChatMessage,
} from '../types/trace';

type LogEntry = components['schemas']['LogEntry'];
type TraceSessionSummary = components['schemas']['TraceSessionSummary'];

/**
 * Hook to check if mock mode is enabled via URL parameters.
 * Only works in development mode.
 */
export function useMockMode() {
  if (!import.meta.env.DEV) return false;
  const [searchParams] = useSearchParams();
  const isMock = searchParams.get('mock') === 'true' || searchParams.get('mock') === '1';
  return isMock;
}

// --- Logs Mock Data ---

function generateMockLogs(count: number): LogEntry[] {
  const logs: LogEntry[] = [];
  const levels: LogEntry['level'][] = ['info', 'warn', 'error', 'debug', 'trace'];
  const targets = ['auth_service', 'gateway', 'storage_node', 'api_v2', 'worker_pool'];
  const messages = [
    'User authenticated successfully',
    'Failed to connect to database',
    'Cache miss on key: user_profile_123',
    'Starting background synchronization',
    'Rate limit exceeded for IP: 192.168.1.45',
    'Incoming request: GET /v1/status',
    'Storage node 4 reported high latency',
    'Token validation failed: expired',
  ];

  const now = Date.now();
  for (let i = 0; i < count; i++) {
    logs.push({
      id: i + 1,
      timestamp: new Date(now - i * 1000 * 30).toISOString(),
      level: levels[Math.floor(Math.random() * levels.length)],
      target: targets[Math.floor(Math.random() * targets.length)],
      message: messages[Math.floor(Math.random() * messages.length)],
      fields: [
        { name: 'pid', value: Math.floor(Math.random() * 10000).toString() },
        { name: 'node', value: 'aura-node-01' },
      ],
    });
  }
  return logs;
}

// Using a ternary with import.meta.env.DEV ensures that in production:
// 1. The condition becomes 'false ? generateMockLogs(200) : []'
// 2. The minifier (Terser/Esbuild) simplifies this to just '[]'
// 3. Since generateMockLogs is now unused, it's completely tree-shaken out.
export const MOCK_LOGS = import.meta.env.DEV ? generateMockLogs(200) : [];

// --- Traces Mock Data ---

function makeJobStatus(
  kind: components['schemas']['JobStatusKind'],
): components['schemas']['JobStatus'] {
  if (kind === 'cancelled') {
    return { kind, cancel_reason: 'user_preempt', partial_artifacts: [] };
  }
  if (kind === 'failed' || kind === 'stuck') {
    return { kind, reason: 'simulated failure', partial_artifacts: [] };
  }
  return { kind, partial_artifacts: [] };
}

function generateMockSummaries(count: number): TraceSessionSummary[] {
  const out: TraceSessionSummary[] = [];
  const now = Date.now();
  const statuses: components['schemas']['JobStatusKind'][] = [
    'in_progress',
    'completed',
    'failed',
    'completed',
    'cancelled',
    'completed',
  ];

  for (let i = 0; i < count; i++) {
    const lastActive = new Date(now - i * 1000 * 60 * 15 - Math.random() * 10_000_000);
    const created = new Date(lastActive.getTime() - Math.random() * 1000 * 60 * 60);
    const kind = statuses[Math.floor(Math.random() * statuses.length)];
    out.push({
      session_id: `sess-${Math.random().toString(36).substring(2, 12)}-${Math.random()
        .toString(36)
        .substring(2, 8)}`,
      created_at: created.toISOString(),
      last_active: lastActive.toISOString(),
      latest_job_status: makeJobStatus(kind),
      job_count: Math.floor(Math.random() * 4) + 1,
      span_count: Math.floor(Math.random() * 60) + 1,
      input_tokens: Math.floor(Math.random() * 80_000),
      output_tokens: Math.floor(Math.random() * 40_000),
    });
  }
  return out.sort(
    (a, b) => new Date(b.last_active).getTime() - new Date(a.last_active).getTime(),
  );
}

export const MOCK_TRACE_SUMMARIES: TraceSessionSummary[] = import.meta.env.DEV
  ? generateMockSummaries(80)
  : [];

// --- Cron Mock Data ---

function generateMockCrons(count: number): components['schemas']['CronJob'][] {
  const crons: components['schemas']['CronJob'][] = [];
  const now = Date.now();
  const channels = ['telegram', 'weixin', 'discord', 'tui', 'http'];
  const statuses: components['schemas']['CronStatus'][] = ['enabled', 'disabled', 'enabled', 'enabled'];
  const schedules = ['0 9 * * *', '*/30 * * * *', '0 0 * * 1', '0 18 * * 1-5'];
  const prompts = [
    'Summarize my unread messages',
    'Generate a daily weather report',
    'Check for new releases in my watched repos',
    'Reminder: Weekly sync starting in 10 minutes',
  ];

  for (let i = 0; i < count; i++) {
    const isCron = Math.random() > 0.2;
    const createdAt = new Date(now - i * 1000 * 60 * 60 * 24);
    const updatedAt = new Date(now - i * 1000 * 60 * 60 * 2);
    
    crons.push({
      id: `cron-${Math.random().toString(36).substring(2, 9)}`,
      user_id: `user-${Math.floor(Math.random() * 100)}`,
      channel: channels[Math.floor(Math.random() * channels.length)],
      status: statuses[Math.floor(Math.random() * statuses.length)],
      schedule: isCron 
        ? { kind: 'cron', expr: schedules[Math.floor(Math.random() * schedules.length)] }
        : { kind: 'at', time: new Date(now + Math.random() * 1000 * 60 * 60 * 24).toISOString() },
      action: {
        kind: 'prompt',
        prompt: prompts[Math.floor(Math.random() * prompts.length)],
      },
      timezone: 'UTC',
      last_triggered_at: Math.random() > 0.3 ? new Date(now - Math.random() * 1000 * 60 * 60).toISOString() : null,
      next_trigger_at: new Date(now + Math.random() * 1000 * 60 * 60).toISOString(),
      created_at: createdAt.toISOString(),
      updated_at: updatedAt.toISOString(),
      origin_session_id: `session-${Math.random().toString(36).substring(2, 6)}`,
    });
  }
  return crons;
}

export const MOCK_CRONS = import.meta.env.DEV ? generateMockCrons(20) : [];

// Analytics mock lives inline in `pages/AnalyticsPage.tsx` so it can
// produce the exact `AnalyticsResponse` shape from the OpenAPI schema
// without having a parallel TS interface to keep in sync.

// --- Session detail (full Job→Step→Span tree) ---

let mockIdCounter = 0;
function mid(prefix: string): string {
  mockIdCounter += 1;
  return `${prefix}-${mockIdCounter.toString(36).padStart(6, '0')}`;
}

function step(jobId: string, kind: StepKind, started: Date, ended: Date | null): Step {
  return {
    id: mid('step'),
    job_id: jobId,
    kind,
    started_at: started.toISOString(),
    ended_at: ended?.toISOString() ?? null,
    outcome: ended ? { outcome: 'ok' } : { outcome: 'pending' },
  };
}

function llmSpan(
  stepId: string,
  model: string,
  messages: ChatMessage[],
  output: string,
  toolCalls: { id: string; name: string; arguments: unknown }[],
  inTok: number,
  outTok: number,
  startedAt: Date,
  endedAt: Date,
): Span {
  return {
    id: mid('span'),
    step_id: stepId,
    kind: {
      kind: 'llm_call',
      begin: {
        model_id: model,
        provider: 'anthropic',
        provider_config_hash: 'mock-hash',
        input_messages: messages,
        temperature: 0.7,
      },
      result: {
        output_content: output,
        thinking: null,
        tool_calls: toolCalls,
        input_tokens: inTok,
        output_tokens: outTok,
      },
    },
    parallel_group: null,
    started_at: startedAt.toISOString(),
    ended_at: endedAt.toISOString(),
    outcome: { outcome: 'ok' },
    events: [],
  };
}

function toolSpan(
  stepId: string,
  toolName: string,
  llmSpanId: string,
  toolUseId: string,
  params: unknown,
  output: unknown,
  startedAt: Date,
  endedAt: Date,
  parallelGroup: string | null = null,
): Span {
  return {
    id: mid('span'),
    step_id: stepId,
    kind: {
      kind: 'tool_call',
      begin: {
        tool_name: toolName,
        tool_artifact_hash: 'mock-tool-hash',
        triggered_by: { llm_span_id: llmSpanId, tool_use_id: toolUseId },
        params,
      },
      result: { output, success: true },
    },
    parallel_group: parallelGroup,
    started_at: startedAt.toISOString(),
    ended_at: endedAt.toISOString(),
    outcome: { outcome: 'ok' },
    events: [],
  };
}

function userMsg(text: string): ChatMessage {
  return { role: 'user', content: [{ Text: text }] };
}

function systemMsg(text: string): ChatMessage {
  return { role: 'system', content: [{ Text: text }] };
}

export function getMockSessionReplay(sessionId: string): SessionReplay {
  const t0 = Date.now() - 5 * 60 * 1000;
  const job1 = mid('job');

  // Step 1: skill selection
  const skillStarted = new Date(t0);
  const skillEnded = new Date(t0 + 80);
  const skillStep = step(job1, { kind: 'skill_selection' }, skillStarted, skillEnded);
  const skillLlm = llmSpan(
    skillStep.id,
    'claude-sonnet-4-6',
    [systemMsg('Pick a skill.'), userMsg('Help me list files.')],
    JSON.stringify({ skill: 'codebase_investigator' }),
    [],
    40,
    8,
    skillStarted,
    skillEnded,
  );

  // Step 2: LLM iteration with parallel tool calls
  const it1Start = new Date(t0 + 100);
  const it1End = new Date(t0 + 1600);
  const it1Step = step(job1, { kind: 'llm_iteration' }, it1Start, it1End);
  const it1Llm = llmSpan(
    it1Step.id,
    'claude-sonnet-4-6',
    [
      systemMsg('You are a helpful assistant.'),
      userMsg('List files and read README.md'),
    ],
    '',
    [
      { id: 'tu_1', name: 'list_directory', arguments: { path: '.' } },
      { id: 'tu_2', name: 'read_file', arguments: { path: 'README.md' } },
    ],
    120,
    36,
    it1Start,
    new Date(t0 + 850),
  );
  const parallelGroup = mid('pg');
  const tool1 = toolSpan(
    it1Step.id,
    'list_directory',
    it1Llm.id,
    'tu_1',
    { path: '.' },
    { files: ['README.md', 'src', 'package.json'] },
    new Date(t0 + 870),
    new Date(t0 + 1000),
    parallelGroup,
  );
  const tool2 = toolSpan(
    it1Step.id,
    'read_file',
    it1Llm.id,
    'tu_2',
    { path: 'README.md' },
    { content: 'Aura — intelligent assistant framework. <api key: [{REDACTED_SECRET_a1b2}]>' },
    new Date(t0 + 880),
    new Date(t0 + 1450),
    parallelGroup,
  );
  // Sanitize event on tool2 (mock placeholder hit)
  tool2.events = [
    {
      span_id: tool2.id,
      seq: 0,
      at: new Date(t0 + 1450).toISOString(),
      kind: {
        kind: 'sanitize_hit',
        hits_count: 1,
        kinds: ['api_key'],
        placeholder_ids: ['[{REDACTED_SECRET_a1b2}]'],
      },
    },
  ];

  // Step 3: final LLM iteration (response)
  const it2Start = new Date(t0 + 1700);
  const it2End = new Date(t0 + 2400);
  const it2Step = step(job1, { kind: 'llm_iteration' }, it2Start, it2End);
  const it2Llm = llmSpan(
    it2Step.id,
    'claude-sonnet-4-6',
    [
      systemMsg('You are a helpful assistant.'),
      userMsg('List files and read README.md'),
      {
        role: 'assistant',
        content: [
          { Text: 'Listing and reading.' },
          {
            ToolUse: { id: 'tu_1', name: 'list_directory', input: { path: '.' } },
          },
        ],
      },
      {
        role: 'tool',
        content: [
          { ToolResult: { tool_use_id: 'tu_1', content: '{"files":["README.md","src","package.json"]}' } },
          { ToolResult: { tool_use_id: 'tu_2', content: '{"content":"..."}' } },
        ],
      },
    ],
    'There are 3 entries: README.md, src/, package.json. README documents Aura.',
    [],
    220,
    52,
    it2Start,
    it2End,
  );

  return {
    session_id: sessionId,
    jobs: [
      {
        job_id: job1,
        job_status_kind: 'completed',
        steps: [
          { step: skillStep, spans: [skillLlm] },
          { step: it1Step, spans: [it1Llm, tool1, tool2] },
          { step: it2Step, spans: [it2Llm] },
        ],
      },
    ],
  };
}


