import { useSearchParams } from 'react-router-dom';
import type { components } from './schema';
import type {
  ChatMessage,
  JobTrace,
  Span,
  Step,
  StepKind,
  TraceOverview,
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
  // Weighted to reflect typical traffic: user chats dominate, subagent
  // spawns + cron are common.
  const kinds: components['schemas']['SessionKind'][] = [
    'user',
    'user',
    'user',
    'user',
    'cron',
    'cron',
    'subagent',
    'subagent',
  ];

  for (let i = 0; i < count; i++) {
    const lastActive = new Date(now - i * 1000 * 60 * 15 - Math.random() * 10_000_000);
    const created = new Date(lastActive.getTime() - Math.random() * 1000 * 60 * 60);
    const status = statuses[Math.floor(Math.random() * statuses.length)];
    const kind = kinds[Math.floor(Math.random() * kinds.length)];
    out.push({
      session_id: `sess-${Math.random().toString(36).substring(2, 12)}-${Math.random()
        .toString(36)
        .substring(2, 8)}`,
      created_at: created.toISOString(),
      last_active: lastActive.toISOString(),
      latest_job_status: makeJobStatus(status),
      kind,
      job_count: Math.floor(Math.random() * 4) + 1,
      span_count: Math.floor(Math.random() * 60) + 1,
      input_tokens: Math.floor(Math.random() * 80_000),
      output_tokens: Math.floor(Math.random() * 40_000),
      cached_input_tokens: Math.floor(Math.random() * 50_000),
      cache_creation_input_tokens: Math.floor(Math.random() * 8_000),
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
      prompt: prompts[Math.floor(Math.random() * prompts.length)],
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

export const MOCK_BACKGROUND_JOBS: components['schemas']['BackgroundJobsResponse'] = {
  jobs: [
    {
      handle: 'bg-7f3a',
      session_id: 'sess-mock-1',
      kind: 'explorer',
      summary: 'Map the auth subsystem',
    },
    {
      handle: 'bg-2e10',
      session_id: 'sess-mock-2',
      kind: 'command',
      summary: 'cargo test --workspace',
    },
    {
      handle: 'bg-aa44',
      session_id: 'sess-mock-2',
      kind: 'planner',
      summary: 'Draft the rollout plan',
    },
  ],
};

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
        cached_input_tokens: Math.floor(inTok * 0.6),
        cache_creation_input_tokens: Math.floor(inTok * 0.1),
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
  return { role: 'user', content: [{ Text: text }], source: 'user' };
}

function systemMsg(text: string): ChatMessage {
  return { role: 'system', content: [{ Text: text }], source: 'agent' };
}

function interjectMsg(text: string): ChatMessage {
  return { role: 'user', content: [{ Text: text }], source: 'user_interjection' };
}

// Shared mock fixture: builds one job's worth of steps/spans + the
// session_messages log it would have produced. Kept module-private so
// the overview and job-trace mocks stay in lock-step. Cached per
// session id so successive calls return stable ids (the page issues
// overview + job fetches separately).
interface MockSessionFixture {
  job: JobTrace;
  overview: TraceOverview;
}
const mockFixtures = new Map<string, MockSessionFixture>();

function buildMockSession(sessionId: string): MockSessionFixture {
  const cached = mockFixtures.get(sessionId);
  if (cached) return cached;

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
        source: 'agent',
      },
      {
        role: 'tool',
        content: [
          { ToolResult: { tool_use_id: 'tu_1', content: '{"files":["README.md","src","package.json"]}' } },
          { ToolResult: { tool_use_id: 'tu_2', content: '{"content":"..."}' } },
        ],
        source: 'agent',
      },
      interjectMsg('Actually, also tell me the package name.'),
    ],
    'There are 3 entries: README.md, src/, package.json. README documents Aura. The package is "aura".',
    [],
    220,
    52,
    it2Start,
    it2End,
  );

  const createdAt = new Date(t0 - 120).toISOString();
  const startedAt = new Date(t0).toISOString();
  const endedAt = new Date(t0 + 2400).toISOString();

  const job: JobTrace = {
    job_id: job1,
    session_id: sessionId,
    job_status_kind: 'completed',
    created_at: createdAt,
    started_at: startedAt,
    ended_at: endedAt,
    steps: [
      { step: skillStep, spans: [skillLlm] },
      { step: it1Step, spans: [it1Llm, tool1, tool2] },
      { step: it2Step, spans: [it2Llm] },
    ],
  };

  const overview: TraceOverview = {
    session_id: sessionId,
    // Mock spans use `LlmCallInputs::Inline`, so the transcript log isn't
    // needed for message rendering. We seed one mid-turn interjection row
    // (created between iteration 1 and the final response, inside the job
    // window) so the sidebar / job-summary interjection markers light up.
    session_messages: [
      {
        ordinal: 1,
        superseded_by: null,
        created_at: new Date(t0 + 1650).toISOString(),
        message: interjectMsg('Actually, also tell me the package name.'),
      },
    ],
    jobs: [
      {
        job_id: job1,
        session_id: sessionId,
        job_status_kind: 'completed',
        created_at: createdAt,
        started_at: startedAt,
        ended_at: endedAt,
        input_tokens: 380,
        output_tokens: 96,
        cached_input_tokens: 228,
        cache_creation_input_tokens: 38,
      },
    ],
  };

  const fixture = { job, overview };
  mockFixtures.set(sessionId, fixture);
  return fixture;
}

export function getMockTraceOverview(sessionId: string): TraceOverview {
  return buildMockSession(sessionId).overview;
}

export function getMockJobTrace(sessionId: string, jobId: string): JobTrace | null {
  const fx = buildMockSession(sessionId);
  return fx.job.job_id === jobId ? fx.job : null;
}


