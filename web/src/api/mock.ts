import { useSearchParams } from 'react-router-dom';
import type { components } from './schema';

type LogEntry = components['schemas']['LogEntry'];

export interface TraceSession {
  id: string;
  trigger: string;
  status: 'active' | 'completed' | 'error';
  spanCount: number;
  activeTime: string;
  createTime: string;
  lastMessage: string;
}

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

function generateMockSessions(count: number): TraceSession[] {
  const sessions: TraceSession[] = [];
  const now = Date.now();
  const statuses: TraceSession['status'][] = ['active', 'completed', 'error', 'completed', 'completed'];
  const triggers = ['GET /api/v1/users', 'POST /api/v1/login', 'GET /api/v1/status', 'PUT /api/v1/settings', 'DELETE /api/v1/cache'];
  const messages = [
    'Successfully fetched 50 users from the primary database shard.',
    'Authentication failed: Invalid credentials provided for user admin@aura.io',
    'Health check passed: all downstream services are reachable and healthy.',
    'Settings updated: modified default timeout from 30s to 45s.',
    'Cache cleared: invalidated 1,240 entries across the regional cluster.',
    'Processing trace data for high-priority background worker task #9421.',
    'Internal Server Error: Connection reset by peer during database migration script execution.',
  ];
  
  for (let i = 0; i < count; i++) {
    const activeTimeDate = new Date(now - i * 1000 * 60 * 15 - Math.random() * 10000000);
    const createTimeDate = new Date(activeTimeDate.getTime() - Math.random() * 1000 * 60 * 60);
    
    sessions.push({
      id: `trace-session-${Math.random().toString(36).substring(2, 12)}-${Math.random().toString(36).substring(2, 8)}`,
      trigger: triggers[Math.floor(Math.random() * triggers.length)],
      status: statuses[Math.floor(Math.random() * statuses.length)],
      spanCount: Math.floor(Math.random() * 500) + 1,
      activeTime: activeTimeDate.toISOString(),
      createTime: createTimeDate.toISOString(),
      lastMessage: messages[Math.floor(Math.random() * messages.length)],
    });
  }
  return sessions.sort((a, b) => new Date(b.activeTime).getTime() - new Date(a.activeTime).getTime());
}

export const MOCK_TRACES = import.meta.env.DEV ? generateMockSessions(150) : [];

export interface TraceSpan {
  id: string;
  type: 'llm_call' | 'tool_call';
  name: string;
  input: any;
  output: any;
  status: 'success' | 'error';
  durationMs: number;
  meta?: Record<string, any>;
  children?: TraceSpan[];
}

export interface TraceTurn {
  id: string;
  userMessage: string;
  aiResponse: string;
  spans: TraceSpan[];
  timestamp: string;
}

export interface TraceSessionDetail {
  sessionId: string;
  turns: TraceTurn[];
}

export function getMockSessionDetail(sessionId: string): TraceSessionDetail {
  const now = Date.now();
  return {
    sessionId,
    turns: [
      {
        id: 'turn-1',
        userMessage: 'List all files in the current directory',
        aiResponse: 'I have listed the files in the directory. You have 5 files including README.md and src folder.',
        timestamp: new Date(now - 150000).toISOString(),
        spans: [
          {
            id: 'span-1',
            type: 'llm_call',
            name: 'gpt-4o',
            input: { prompt: 'System: You are an assistant.\nUser: List all files in the current directory' },
            output: { response: '', tool_calls: [{ name: 'list_directory', arguments: { path: '.' } }] },
            status: 'success',
            durationMs: 1450,
            meta: { temperature: 0.7, max_tokens: 1024, prompt_tokens: 45, completion_tokens: 18, total_tokens: 63 },
            children: [
              {
                id: 'span-1-1',
                type: 'tool_call',
                name: 'list_directory',
                input: { path: '.' },
                output: { files: ['README.md', 'src', 'package.json', 'tsconfig.json', '.gitignore'] },
                status: 'success',
                durationMs: 120,
                meta: { internal_retries: 0 }
              }
            ]
          },
          {
            id: 'span-2',
            type: 'llm_call',
            name: 'gpt-4o',
            input: { prompt: 'System: You are an assistant.\nUser: List all files in the current directory\nTool list_directory Output: {"files":["README.md","src","package.json","tsconfig.json",".gitignore"]}' },
            output: { response: 'I have listed the files in the directory. You have 5 files including README.md and src folder.' },
            status: 'success',
            durationMs: 1200,
            meta: { temperature: 0.7, max_tokens: 1024, prompt_tokens: 72, completion_tokens: 24, total_tokens: 96 }
          }
        ]
      },
      {
        id: 'turn-2',
        userMessage: 'Read the README.md file and modify it to add a new section',
        aiResponse: 'I have read the file and modified it to include the "Getting Started" section.',
        timestamp: new Date(now - 80000).toISOString(),
        spans: [
          {
            id: 'span-3',
            type: 'llm_call',
            name: 'gpt-4o',
            input: { prompt: 'System: You are an assistant.\nUser: Read the README.md file and modify it to add a new section' },
            output: { response: '', tool_calls: [{ name: 'read_file', arguments: { path: 'README.md' } }] },
            status: 'success',
            durationMs: 850,
            meta: { temperature: 0.7, max_tokens: 1024, prompt_tokens: 50, completion_tokens: 15, total_tokens: 65 },
            children: [
              {
                id: 'span-3-1',
                type: 'tool_call',
                name: 'read_file',
                input: { path: 'README.md' },
                output: { content: '# My Project\n\nThis is a cool project.' },
                status: 'success',
                durationMs: 45
              }
            ]
          },
          {
            id: 'span-4',
            type: 'llm_call',
            name: 'gpt-4o',
            input: { prompt: '... \nTool read_file Output: {"content":"# My Project\\n\\nThis is a cool project."}' },
            output: { response: '', tool_calls: [{ name: 'replace_file', arguments: { path: 'README.md', content: '# My Project\n\nThis is a cool project.\n\n## Getting Started\nRun `npm install`.' } }] },
            status: 'success',
            durationMs: 2100,
            meta: { temperature: 0.7, max_tokens: 1024, prompt_tokens: 80, completion_tokens: 40, total_tokens: 120 },
            children: [
              {
                id: 'span-4-1',
                type: 'tool_call',
                name: 'replace_file',
                input: { path: 'README.md', content: '# My Project\n\nThis is a cool project.\n\n## Getting Started\nRun `npm install`.' },
                output: { success: true },
                status: 'success',
                durationMs: 150
              }
            ]
          },
          {
            id: 'span-5',
            type: 'llm_call',
            name: 'gpt-4o',
            input: { prompt: '... \nTool replace_file Output: {"success":true}' },
            output: { response: 'I have read the file and modified it to include the "Getting Started" section.' },
            status: 'success',
            durationMs: 950,
            meta: { temperature: 0.7, max_tokens: 1024, prompt_tokens: 120, completion_tokens: 20, total_tokens: 140 }
          }
        ]
      },
      {
        id: 'turn-3',
        userMessage: 'Test if the project compiles',
        aiResponse: 'The project compiles successfully without any errors.',
        timestamp: new Date(now - 20000).toISOString(),
        spans: [
          {
            id: 'span-6',
            type: 'llm_call',
            name: 'gpt-4-turbo',
            input: { prompt: 'System: You are an assistant.\nUser: Test if the project compiles' },
            output: { response: '', tool_calls: [{ name: 'run_shell_command', arguments: { command: 'npm run build' } }] },
            status: 'success',
            durationMs: 1300,
            meta: { temperature: 0.2, max_tokens: 2048, prompt_tokens: 35, completion_tokens: 15, total_tokens: 50 },
            children: [
              {
                id: 'span-6-1',
                type: 'tool_call',
                name: 'run_shell_command',
                input: { command: 'npm run build' },
                output: { stdout: '> build\n> tsc\n\nBuild completed.', stderr: '', exitCode: 0 },
                status: 'success',
                durationMs: 3400,
                meta: { working_dir: '/data/aura-web/web', exit_code: 0 }
              }
            ]
          },
          {
            id: 'span-7',
            type: 'llm_call',
            name: 'gpt-4-turbo',
            input: { prompt: '... \nTool run_shell_command Output: {"stdout":"> build\\n> tsc\\n\\nBuild completed.", "stderr":"", "exitCode":0}' },
            output: { response: 'The project compiles successfully without any errors.' },
            status: 'success',
            durationMs: 800,
            meta: { temperature: 0.2, max_tokens: 2048, prompt_tokens: 85, completion_tokens: 12, total_tokens: 97 }
          }
        ]
      }
    ]
  };
}
