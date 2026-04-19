export function TopNav({ current }: { current: string }) {
  return (
    <header className="px-8 py-5 border-b-2 border-black bg-white shrink-0">
      <div className="text-[0.9rem] font-semibold text-ink-soft">
        <span>Home</span>
        <span className="mx-2">/</span>
        <span className="text-ink font-bold">{current}</span>
      </div>
    </header>
  );
}
