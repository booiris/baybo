declare module "chrome-devtools-mcp" {
  export interface CreateMcpServerArgs {
    headless: boolean;
    isolated: boolean;
    usageStatistics: boolean;
    performanceCrux: boolean;
    userDataDir: string;
    executablePath?: string | undefined;
    channel?: string | undefined;
    chromeArg?: string[] | undefined;
    ignoreDefaultChromeArg?: string[] | undefined;
    acceptInsecureCerts?: boolean | undefined;
    proxyServer?: string | undefined;
    viewport?: { width: number; height: number } | undefined;
    redactNetworkHeaders: boolean;
    categoryNavigationAutomation: boolean;
    categoryDebugging: boolean;
    categoryEmulation: boolean;
    categoryPerformance: boolean;
    categoryNetwork: boolean;
    categoryExtensions: boolean;
    slim: boolean;
    [extra: string]: unknown;
  }

  export interface CreateMcpServerOptions {
    logFile?: unknown;
  }

  export function createMcpServer(
    args: CreateMcpServerArgs,
    options: CreateMcpServerOptions,
  ): Promise<{
    server: { connect(transport: unknown): Promise<void>; close(): Promise<void> };
    clearcutLogger: unknown;
  }>;
}
