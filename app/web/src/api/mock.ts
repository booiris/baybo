import { useSearchParams } from 'react-router-dom';
import type { components } from './schema';
import type {
  ChatMessage,
  JobTrace,
  LifecycleState,
  Span,
  Step,
  StepKind,
  TraceJobSummary,
  TraceOverview,
} from '../types/trace';

type LogEntry = components['schemas']['LogEntry'];
type TraceSessionSummary = components['schemas']['TraceSessionSummary'];

/**
 * Hook to check if mock mode is enabled via URL parameters.
 * Only works in development mode.
 */
export function useMockMode() {
  // The hook must run unconditionally (rules of hooks) — every caller is a page
  // under the root <HashRouter>, so useSearchParams is always in context. The
  // DEV guard then discards the result in production.
  const [searchParams] = useSearchParams();
  if (!import.meta.env.DEV) return false;
  return searchParams.get('mock') === 'true' || searchParams.get('mock') === '1';
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
        { name: 'node', value: 'baybo-node-01' },
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
  const titles = ['Unread digest', 'Weather report', 'Release watch', 'Weekly sync reminder'];

  for (let i = 0; i < count; i++) {
    const isCron = Math.random() > 0.2;
    const createdAt = new Date(now - i * 1000 * 60 * 60 * 24);
    const updatedAt = new Date(now - i * 1000 * 60 * 60 * 2);
    // Every fifth job sits in the recycle bin, so the deleted view has content.
    const deletedAt = i % 5 === 4 ? new Date(now - i * 1000 * 60 * 30) : null;

    const pick = Math.floor(Math.random() * prompts.length);
    crons.push({
      id: `cron-${Math.random().toString(36).substring(2, 9)}`,
      user_id: `user-${Math.floor(Math.random() * 100)}`,
      channel: channels[Math.floor(Math.random() * channels.length)],
      title: titles[pick],
      pinned: false,
      status: statuses[Math.floor(Math.random() * statuses.length)],
      schedule: isCron
        ? { kind: 'cron', expr: schedules[Math.floor(Math.random() * schedules.length)] }
        : { kind: 'at', time: new Date(now + Math.random() * 1000 * 60 * 60 * 24).toISOString() },
      prompt: prompts[pick],
      timezone: 'UTC',
      last_triggered_at: Math.random() > 0.3 ? new Date(now - Math.random() * 1000 * 60 * 60).toISOString() : null,
      next_trigger_at: new Date(now + Math.random() * 1000 * 60 * 60).toISOString(),
      created_at: createdAt.toISOString(),
      updated_at: updatedAt.toISOString(),
      deleted_at: deletedAt ? deletedAt.toISOString() : null,
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

function step(
  jobId: string,
  kind: StepKind,
  started: Date,
  ended: Date | null,
  outcome?: LifecycleState,
): Step {
  return {
    id: mid('step'),
    job_id: jobId,
    kind,
    started_at: started.toISOString(),
    ended_at: ended?.toISOString() ?? null,
    outcome: outcome ?? (ended ? { outcome: 'ok' } : { outcome: 'pending' }),
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
  outcome: LifecycleState = { outcome: 'ok' },
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
    outcome,
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
  outcome: LifecycleState = { outcome: 'ok' },
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
      result: { output, success: outcome.outcome === 'ok' },
    },
    parallel_group: parallelGroup,
    started_at: startedAt.toISOString(),
    ended_at: endedAt.toISOString(),
    outcome,
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
  jobs: JobTrace[];
  overview: TraceOverview;
}
const mockFixtures = new Map<string, MockSessionFixture>();

function buildMockSession(sessionId: string): MockSessionFixture {
  const cached = mockFixtures.get(sessionId);
  if (cached) return cached;

  const t0 = Date.now() - 5 * 60 * 1000;
  const job1 = mid('job');

  // Step 1: a context compaction — the transcript so far is replaced by a
  // summary. This is a real emitted StepKind (unlike skill_selection, which the
  // wire defines but no production path records).
  const compStarted = new Date(t0);
  const compEnded = new Date(t0 + 80);
  const compStep = step(job1, { kind: 'compression', trigger: 'inline' }, compStarted, compEnded);
  const compLlm = llmSpan(
    compStep.id,
    'claude-sonnet-4-6',
    [systemMsg('Summarize the conversation so far.'), userMsg('Help me list files.')],
    'Earlier: the user asked to list files and read README.md.',
    [],
    12_400,
    260,
    compStarted,
    compEnded,
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
    { content: 'Baybo — intelligent assistant framework. <api key: [{REDACTED_SECRET_a1b2}]>' },
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
    'There are 3 entries: README.md, src/, package.json. README documents Baybo. The package is "baybo".',
    [],
    220,
    52,
    it2Start,
    it2End,
  );

  const createdAt = new Date(t0 - 120).toISOString();
  const startedAt = new Date(t0).toISOString();
  const endedAt = new Date(t0 + 2400).toISOString();

  const job1Trace: JobTrace = {
    job_id: job1,
    session_id: sessionId,
    job_status_kind: 'completed',
    created_at: createdAt,
    started_at: startedAt,
    ended_at: endedAt,
    steps: [
      { step: compStep, spans: [compLlm] },
      { step: it1Step, spans: [it1Llm, tool1, tool2] },
      { step: it2Step, spans: [it2Llm] },
    ],
  };

  const job1Summary: TraceJobSummary = {
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
  };

  // Second job: a FAILED turn — a single-span memory-recall step (deep-links
  // straight to its span) followed by an LLM iteration whose `run_bash` tool
  // call fails, failing the step and the job. Exercises the failure-path
  // auto-expand, the red roll-up badge, and the Step detail panel.
  const job2 = mid('job');
  const t1 = t0 + 60_000;
  const j2Created = new Date(t1 - 200).toISOString();
  const j2Started = new Date(t1).toISOString();
  const j2Ended = new Date(t1 + 1400).toISOString();

  const recallStart = new Date(t1);
  const recallEnd = new Date(t1 + 120);
  const recallStep = step(job2, { kind: 'memory_recall' }, recallStart, recallEnd);
  const recallTool = toolSpan(
    recallStep.id,
    'recall_memory',
    '',
    '',
    { query: 'prior context about the package' },
    { hits: 2, memories: ['pkg is baybo', 'tests live under app/web'] },
    recallStart,
    recallEnd,
  );

  const it3Start = new Date(t1 + 150);
  const it3End = new Date(t1 + 1400);
  const it3Step = step(job2, { kind: 'llm_iteration' }, it3Start, it3End, {
    outcome: 'failed',
    reason: 'tool run_bash failed',
  });
  const it3Llm = llmSpan(
    it3Step.id,
    'claude-sonnet-4-6',
    [systemMsg('You are a helpful assistant.'), userMsg('Run the tests.')],
    '',
    [{ id: 'tu_b1', name: 'run_bash', arguments: { cmd: 'pytest -q' } }],
    300,
    40,
    it3Start,
    new Date(t1 + 700),
  );
  const failTool = toolSpan(
    it3Step.id,
    'run_bash',
    it3Llm.id,
    'tu_b1',
    { cmd: 'pytest -q' },
    'E   assert 1 == 2\n1 failed, 0 passed',
    new Date(t1 + 720),
    new Date(t1 + 1350),
    null,
    { outcome: 'failed', reason: 'bash exited with code 1' },
  );

  const spawnStart = new Date(t1 + 200);
  const spawnEnd = new Date(t1 + 690);
  const spawnSpan = toolSpan(
    it3Step.id,
    'spawn_subagent',
    it3Llm.id,
    'tu_sa1',
    { profile: 'explorer', task: 'Find where the test fixtures live' },
    { child_session_id: 'sess-child-explorer', final_text: 'Fixtures live under app/web/src/test.' },
    spawnStart,
    spawnEnd,
  );

  const job2Trace: JobTrace = {
    job_id: job2,
    session_id: sessionId,
    job_status_kind: 'failed',
    created_at: j2Created,
    started_at: j2Started,
    ended_at: j2Ended,
    steps: [
      { step: recallStep, spans: [recallTool] },
      { step: it3Step, spans: [it3Llm, spawnSpan, failTool] },
    ],
  };

  const job2Summary: TraceJobSummary = {
    job_id: job2,
    session_id: sessionId,
    job_status_kind: 'failed',
    created_at: j2Created,
    started_at: j2Started,
    ended_at: j2Ended,
    input_tokens: 300,
    output_tokens: 40,
    cached_input_tokens: 180,
    cache_creation_input_tokens: 30,
  };

  const overview: TraceOverview = {
    session_id: sessionId,
    // Mock spans use `LlmCallInputs::Inline`, so the transcript log isn't
    // needed for message rendering. We seed one mid-turn interjection row
    // (created between iteration 1 and the final response, inside job 1's
    // window) so the sidebar / job-summary interjection markers light up.
    session_messages: [
      {
        ordinal: 1,
        superseded_by: null,
        created_at: new Date(t0 + 1650).toISOString(),
        message: interjectMsg('Actually, also tell me the package name.'),
      },
    ],
    jobs: [job1Summary, job2Summary],
    supersede_watermark: null,
  };

  const fixture: MockSessionFixture = { jobs: [job1Trace, job2Trace], overview };
  mockFixtures.set(sessionId, fixture);
  return fixture;
}

export function getMockTraceOverview(sessionId: string): TraceOverview {
  return buildMockSession(sessionId).overview;
}

export function getMockJobTrace(sessionId: string, jobId: string): JobTrace | null {
  const fx = buildMockSession(sessionId);
  return fx.jobs.find((j) => j.job_id === jobId) ?? null;
}


// --- Agent profiles Mock Data ---

type AgentProfileDto = components['schemas']['AgentProfileDto'];

export const MOCK_AGENT_PROFILES: AgentProfileDto[] = import.meta.env.DEV
  ? [
      {
        id: 'baybo',
        name: 'baybo',
        description:
          "Baybo's default persona: workspace Soul prompt, default model, full skill and tool set.",
        framework: 'baybo',
        builtin: true,
        created_at: '2026-01-01T00:00:00Z',
        updated_at: '2026-01-01T00:00:00Z',
      },
      {
        id: '01JMOCKAGENTREVIEWER000000',
        name: 'Code Reviewer',
        description: 'Reviews diffs with a security-first eye. Terse, actionable findings only.',
        system_prompt: 'You are a rigorous code reviewer. Report only defects.',
        framework: 'claude',
        builtin: false,
        created_at: '2026-02-10T09:30:00Z',
        updated_at: '2026-03-01T18:12:00Z',
      },
      {
        id: '01JMOCKAGENTWRITER00000000',
        name: 'Docs Writer',
        description: 'Turns raw notes into polished documentation.',
        llm: 'fast',
        framework: 'baybo',
        builtin: false,
        created_at: '2026-02-20T14:00:00Z',
        updated_at: '2026-02-21T10:45:00Z',
      },
    ]
  : [];
