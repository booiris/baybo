import { lazy, memo, Suspense } from "react";

const LazyMarkdown = lazy(() => import("./Markdown"));

export const MarkdownBody = memo(function MarkdownBody({ text }: { text: string }) {
  return (
    <Suspense fallback={<div className="md">{text}</div>}>
      <LazyMarkdown text={text} />
    </Suspense>
  );
});

export function prefetchMarkdown(): void {
  void import("./Markdown");
}
