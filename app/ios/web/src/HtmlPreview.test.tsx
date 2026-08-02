import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { I18nextProvider } from "react-i18next";

const { postMaximized } = vi.hoisted(() => ({
  postMaximized: vi.fn(),
}));
vi.mock("./bridge", () => ({
  postHtmlPreviewMaximized: postMaximized,
}));

import i18n from "./i18n";
import { HtmlPreview, InvalidHtmlPreview } from "./HtmlPreview";
import { htmlPreviewBlobId } from "./htmlPreviewProtocol";

const BLOB_ID = `sha256:${"a".repeat(64)}.${"b".repeat(32)}`;

function renderPreview(blobId = BLOB_ID) {
  return render(
    <I18nextProvider i18n={i18n}>
      <HtmlPreview blobId={blobId} />
    </I18nextProvider>,
  );
}

describe("HTML preview marker", () => {
  it("accepts exactly one capability blob id in a baybo-html fence", () => {
    expect(htmlPreviewBlobId("language-baybo-html", `\n${BLOB_ID}\n`)).toBe(BLOB_ID);
    expect(htmlPreviewBlobId("language-html", BLOB_ID)).toBeNull();
    expect(htmlPreviewBlobId("language-baybo-html", "<html>raw</html>")).toBe("");
    expect(htmlPreviewBlobId("language-baybo-html", `${BLOB_ID}\nextra`)).toBe("");
  });
});

describe("HtmlPreview", () => {
  it("loads the blob route in a script-only opaque-origin sandbox", () => {
    renderPreview();
    const frame = screen.getByTitle("Agent-created HTML preview");
    expect(frame).toHaveAttribute("sandbox", "allow-scripts");
    expect(frame.getAttribute("sandbox")).not.toContain("allow-same-origin");
    expect(frame).toHaveAttribute("loading", "lazy");
    expect(frame).toHaveAttribute(
      "src",
      `baybo-transcript://localhost/html-preview/${BLOB_ID}?reload=0`,
    );
  });

  it("maximizes the same iframe and reports native chrome state", async () => {
    const user = userEvent.setup();
    renderPreview();
    const frame = screen.getByTitle("Agent-created HTML preview");

    await user.click(screen.getByRole("button", { name: "Show HTML preview full screen" }));
    expect(screen.getByTitle("Agent-created HTML preview")).toBe(frame);
    expect(frame.closest(".html-preview")).toHaveClass("is-maximized");
    expect(document.documentElement).toHaveClass("html-preview-maximized");
    expect(postMaximized).toHaveBeenLastCalledWith(true);

    await user.click(screen.getByRole("button", { name: "Close full-screen HTML preview" }));
    expect(screen.getByTitle("Agent-created HTML preview")).toBe(frame);
    expect(document.documentElement).not.toHaveClass("html-preview-maximized");
    expect(postMaximized).toHaveBeenLastCalledWith(false);
  });

  it("renders an explicit error instead of navigating for a bad marker", () => {
    render(
      <I18nextProvider i18n={i18n}>
        <InvalidHtmlPreview />
      </I18nextProvider>,
    );
    expect(screen.getByRole("alert")).toHaveTextContent("Invalid HTML preview blob id");
    expect(screen.queryByTitle("Agent-created HTML preview")).toBeNull();
  });
});
