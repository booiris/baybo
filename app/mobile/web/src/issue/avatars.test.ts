import { beforeEach, describe, expect, it, vi } from "vitest";

import { avatarUrl, forgetAvatars } from "./avatars";

const { fetchBlob } = vi.hoisted(() => ({ fetchBlob: vi.fn() }));

vi.mock("../bridge", () => ({ blobObjectUrl: fetchBlob }));

describe("issue avatars", () => {
  beforeEach(() => {
    fetchBlob.mockReset();
    forgetAvatars();
  });

  it("retries a blob after a transient fetch failure", async () => {
    fetchBlob.mockRejectedValueOnce(new Error("binding changed"));
    fetchBlob.mockResolvedValueOnce("blob:sha256:aa.tok");

    await expect(avatarUrl("sha256:aa.tok")).rejects.toThrow("binding changed");
    await expect(avatarUrl("sha256:aa.tok")).resolves.toBe("blob:sha256:aa.tok");

    expect(fetchBlob).toHaveBeenCalledTimes(2);
  });
});
