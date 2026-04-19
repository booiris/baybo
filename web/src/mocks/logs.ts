import type { LogEntry, LogPage } from '../types';

const MOCK_LOGS: LogEntry[] = [
  {
    id: '1',
    timestamp: '2023-10-27T14:32:01.045Z',
    level: 'error',
    source: 'auth-service',
    message: 'Failed to connect to Redis cache at 10.0.4.22:6379',
  },
  {
    id: '2',
    timestamp: '2023-10-27T14:31:45.922Z',
    level: 'warn',
    source: 'api-gateway',
    message: 'High latency detected on /api/v1/users endpoint (450ms)',
  },
  {
    id: '3',
    timestamp: '2023-10-27T14:30:12.110Z',
    level: 'info',
    source: 'payment-worker',
    message: 'Successfully processed batch of 450 transactions',
  },
  {
    id: '4',
    timestamp: '2023-10-27T14:28:55.002Z',
    level: 'info',
    source: 'user-service',
    message: 'User profile updated for ID: 89432',
  },
  {
    id: '5',
    timestamp: '2023-10-27T14:25:33.441Z',
    level: 'error',
    source: 'db-cluster-01',
    message: 'Deadlock detected on transaction ID 44920. Rolled back.',
  },
];

export async function fetchLogs(): Promise<LogPage> {
  await new Promise((r) => setTimeout(r, 200));
  return {
    entries: MOCK_LOGS,
    total: 2492,
    page: 1,
    pageSize: 5,
  };
}
