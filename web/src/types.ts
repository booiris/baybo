export type LogLevel = 'error' | 'warn' | 'info';

export interface LogEntry {
  id: string;
  timestamp: string;
  level: LogLevel;
  source: string;
  message: string;
}

export interface LogPage {
  entries: LogEntry[];
  total: number;
  page: number;
  pageSize: number;
}
