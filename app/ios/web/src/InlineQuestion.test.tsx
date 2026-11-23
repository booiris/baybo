import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import "./i18n";
import { InlineQuestion } from "./InlineQuestion";
import type { InlineQuestionPrompt } from "./types";
import { postToNative } from "./bridge";

vi.mock("./bridge", () => ({ postToNative: vi.fn() }));

const prompt: InlineQuestionPrompt = {
  ask_id: "trip", active: 0, connected: true, submitting: false, can_continue: false, auto_advance: true,
  tabs: [{ title: "Travel", answered: false }, { title: "Interests", answered: false }],
  question: { header: "Travel", question: "How will you travel?", multi_select: false,
    options: [{ label: "Train", description: "Along the river." }] },
  answer: { header: "Travel", selected: [] },
};

describe("inline questions", () => {
  it("holds the painted selection before advancing, and cancels the hold on interaction", () => {
    vi.useFakeTimers();
    try {
      const { rerender } = render(<InlineQuestion prompt={prompt} sessionId="chat" />);
      const train = screen.getByRole("button", { name: "Train" });
      fireEvent.click(train);
      act(() => { vi.advanceTimersByTime(1000); });
      expect(train).toHaveAttribute("aria-pressed", "false");
      expect(postToNative).toHaveBeenLastCalledWith(expect.objectContaining({ action: "select" }));
      const selected = { ...prompt, answer: { ...prompt.answer, selected: ["Train"] } };
      rerender(<InlineQuestion prompt={selected} sessionId="chat" />);
      act(() => { vi.advanceTimersToNextFrame(); });
      act(() => { vi.advanceTimersByTime(219); });
      expect(train).toHaveAttribute("aria-pressed", "true");
      expect(postToNative).toHaveBeenLastCalledWith(expect.objectContaining({ action: "select" }));
      act(() => { vi.advanceTimersByTime(1); });
      expect(postToNative).toHaveBeenLastCalledWith(expect.objectContaining({ action: "autoNext", active: 0, index: 0 }));

      fireEvent.click(train);
      fireEvent.click(screen.getByRole("button", { name: "Interests" }));
      act(() => { vi.advanceTimersByTime(1000); });
      expect(postToNative).toHaveBeenLastCalledWith(expect.objectContaining({ action: "tab" }));
      fireEvent.click(train);
      fireEvent.click(screen.getByRole("button", { name: "Cancel question" }));
      act(() => { vi.advanceTimersByTime(1000); });
      expect(postToNative).toHaveBeenLastCalledWith(expect.objectContaining({ action: "select" }));
    } finally {
      vi.useRealTimers();
    }
  });
  it("addresses the originating session and tab, and waits for native selection state", () => {
    const { rerender } = render(<InlineQuestion prompt={prompt} sessionId="chat" />);
    const train = screen.getByRole("button", { name: "Train" });
    expect(train).toHaveAccessibleDescription("Along the river.");
    fireEvent.click(train);
    expect(postToNative).toHaveBeenLastCalledWith({ type: "questionAction", targetId: "chat", askId: "trip", active: 0, action: "select", index: 0 });
    expect(train).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByRole("button", { name: "Next question" })).toBeDisabled();
    rerender(<InlineQuestion prompt={{ ...prompt, active: 1, question: { ...prompt.question, multi_select: true },
      answer: { header: "Interests", selected: ["Train"], other: "Bike" } }} sessionId="chat" />);
    expect(train).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByRole("button", { name: "Submit answers" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: /Write your own answer/ }));
    expect(postToNative).toHaveBeenLastCalledWith({ type: "questionAction", targetId: "chat", askId: "trip", active: 1, action: "other" });
  });

  it("requires confirmation before sending a cancellation", () => {
    vi.mocked(postToNative).mockClear();
    render(<InlineQuestion prompt={prompt} sessionId="chat" />);
    fireEvent.click(screen.getByRole("button", { name: "Cancel question" }));
    fireEvent.click(screen.getByRole("button", { name: "Keep answering" }));
    expect(postToNative).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Cancel question" }));
    fireEvent.click(screen.getByRole("button", { name: "Confirm cancellation" }));
    expect(postToNative).toHaveBeenCalledExactlyOnceWith({ type: "questionAction", targetId: "chat", askId: "trip", active: 0, action: "cancel" });
  });
});
