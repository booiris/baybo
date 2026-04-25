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
