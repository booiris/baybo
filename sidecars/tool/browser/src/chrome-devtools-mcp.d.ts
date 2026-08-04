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
    // Keys are `category<Capitalised ToolCategory value>`; CDDM derives
    // them from the category string at runtime, so a mistyped key is not
    // a type error there — it just never matches and the category keeps
    // its default.
    categoryNavigation: boolean;
    categoryDebugging: boolean;
    categoryEmulation: boolean;
    categoryPerformance: boolean;
    categoryNetwork: boolean;
    categoryExtensions: boolean;
    slim: boolean;
    experimentalPageIdRouting?: boolean | undefined;
    experimentalStructuredContent?: boolean | undefined;
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
