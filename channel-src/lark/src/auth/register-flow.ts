import type {
  RegistrationContext,
  RegistrationResult,
} from "@aura/channel-sdk";

const APP_ID_PATTERN = /^[A-Za-z0-9_]+$/;

export async function runLarkRegistration(
  ctx: RegistrationContext,
): Promise<RegistrationResult> {
  const appId = (
    await ctx.input("Lark app_id (cli_… or compatible): ", { required: true })
  ).trim();
  if (!APP_ID_PATTERN.test(appId)) {
    throw new Error(
      "lark app_id must be alphanumeric/underscore (e.g. `cli_a1b2c3d4`)",
    );
  }
  const appSecret = (
    await ctx.password("Lark app_secret: ", { required: true })
  ).trim();
  if (!appSecret) {
    throw new Error("lark app_secret must not be empty");
  }
  const baseUrl = (
    await ctx.input(
      "Lark base URL [default https://open.feishu.cn — also https://open.larksuite.com / https://open.volc-feishu.cn]: ",
    )
  ).trim();

  const metadata: Record<string, string> = {};
  if (baseUrl) {
    // Validate at registration so a typo surfaces here instead of at
    // StartBot, where the failure mode is opaque.
    try {
      new URL(baseUrl);
    } catch {
      throw new Error(`lark base URL is not a valid URL: ${baseUrl}`);
    }
    metadata["base_url"] = baseUrl;
  }

  return {
    botId: appId,
    token: appId,
    metadata,
    secrets: { app_secret: appSecret },
  };
}
