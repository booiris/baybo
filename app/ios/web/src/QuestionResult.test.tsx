import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import "./i18n";
import { WorkBlockView } from "./WorkBlock";
import { QuestionResult } from "./QuestionResult";

describe("completed questions", () => {
  it("expands the original prompts, available choices, selections and custom text between tool steps", () => {
    const tool = {
      kind: "tool" as const,
      label: "Read",
      status: "ok",
      callId: "other",
    };
    const onToggle = vi.fn(() => screen.getByText("Answered 2 questions").closest("details")?.open);
    render(
      <WorkBlockView
        onToggle={onToggle}
        row={{
          id: "turn",
          role: "work",
          active: false,
          steps: [
            tool,
            {
              ...tool,
              callId: "c",
              ask: {
                declined: false,
                questions: [
                  {
                    header: "Interests",
                    question: "What interests you?",
                    multi_select: true,
                    options: [
                      { label: "Art", description: "Visit museums" },
                      { label: "Food", description: "Try local dishes" },
                      { label: "Music", description: "Catch a concert" },
                    ],
                    selected: ["Art", "Music"],
                    other: "Coffee\nby the river",
                  },
                  {
                    header: "Travel",
                    question: "How will you get there?",
                    selected: ["Bike"],
                  },
                ],
              },
            },
            { ...tool, callId: "later" },
          ],
        }}
      />,
    );
    fireEvent.click(screen.getByText("Answered 2 questions"));
    expect(onToggle).toHaveBeenLastCalledWith(true);
    expect(onToggle).toHaveLastReturnedWith(false);
    expect(
      screen.getByRole("heading", { name: "What interests you?" }),
    ).toBeVisible();
    const art = screen.getByText("Art").closest("li")!;
    expect(within(art).getByRole("img", { name: "Selected" })).toBeVisible();
    expect(within(art).getByText("Visit museums")).toBeVisible();
    const food = screen.getByText("Food").closest("li")!;
    expect(within(food).getByRole("img", { name: "Not selected" })).toBeVisible();
    const music = screen.getByText("Music").closest("li")!;
    expect(within(music).getByRole("img", { name: "Selected" })).toBeVisible();
    expect(screen.getByText("Written answer")).toBeVisible();
    expect(screen.getByText("Coffee by the river")).toBeVisible();
    expect(screen.getByText("Bike")).toBeVisible();
    fireEvent.click(screen.getByText("Answered 2 questions"));
    expect(onToggle).toHaveBeenLastCalledWith(false);
    expect(onToggle).toHaveLastReturnedWith(true);
  });

  it("retains the question when a request is cancelled or skipped", () => {
    const q = { header: "City", question: "Which city?", skipped: true };
    const view = render(
      <QuestionResult
        ask={{ declined: false, cancelled: true, questions: [q] }}
      />,
    );
    fireEvent.click(screen.getByText("Question cancelled"));
    expect(screen.getByText("Which city?")).toBeVisible();
    view.rerender(<QuestionResult ask={{ declined: false, questions: [q] }} />);
    expect(screen.getByText("Skipped")).toBeVisible();
  });
});
