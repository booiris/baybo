import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';
import { render } from '@testing-library/react';

import { MarkdownBody } from './ChatPage';

// Ported from `app/ios/web/src/Markdown.test.tsx`: the two clients render the
// same assistant message, so they are held to the same cases.
//
// The math pipeline (normalizeMath -> remark-math -> rehype-katex -> KaTeX) is
// wired end to end here — the `mathDelimiters` unit tests can't prove the
// plugins are actually attached to <ReactMarkdown>. KaTeX builds plain DOM with
// inline styles (no layout/measurement), so it renders in jsdom.
describe('MarkdownBody math', () => {
  it('renders inline $...$ as KaTeX', () => {
    const { container } = render(<MarkdownBody text={'energy is $E = mc^2$ today'} />);
    expect(container.querySelector('.katex')).not.toBeNull();
    // The rendered math carries the source symbols.
    expect(container.textContent).toContain('E');
    expect(container.textContent).toContain('m');
  });

  it('renders a single-line $$...$$ as a KaTeX display block', () => {
    const { container } = render(<MarkdownBody text={'$$\\int_0^1 x\\,dx$$'} />);
    expect(container.querySelector('.katex-display')).not.toBeNull();
  });

  it('renders \\[...\\] as a KaTeX display block', () => {
    const { container } = render(<MarkdownBody text={'\\[a^2 + b^2 = c^2\\]'} />);
    expect(container.querySelector('.katex-display')).not.toBeNull();
  });

  it('renders the \\(...\\) delimiter form as inline KaTeX', () => {
    const { container } = render(<MarkdownBody text={'mass \\(a^2 + b^2\\) here'} />);
    expect(container.querySelector('.katex')).not.toBeNull();
    expect(container.querySelector('.katex-display')).toBeNull();
  });

  it('does not render math inside a code span', () => {
    const { container } = render(<MarkdownBody text={'literal `$x$` text'} />);
    expect(container.querySelector('.katex')).toBeNull();
    expect(container.querySelector('code')?.textContent).toBe('$x$');
  });

  it('does not throw on a malformed expression, and colors it from the palette', () => {
    const { container } = render(<MarkdownBody text={'broken $\\frac{1}{$ end'} />);
    // No exception, and the surrounding prose still rendered.
    expect(container.textContent).toContain('end');
    // KaTeX inlines the error color, so it can only come from the plugin
    // options — not from a stylesheet rule.
    const err = container.querySelector('.katex-error');
    expect(err?.getAttribute('style')).toContain('var(--color-err)');
  });

  it('leaves paired currency as literal text, not math', () => {
    const { container } = render(<MarkdownBody text={'It costs $5 and $3 total.'} />);
    expect(container.querySelector('.katex')).toBeNull();
    expect(container.textContent).toContain('$5');
    expect(container.textContent).toContain('$3');
  });

  it('still renders digit/decimal/arithmetic inline math (not mistaken for money)', () => {
    for (const src of ['$3.14$', '$2 + 2 = 4$', '$5 = x$']) {
      const { container } = render(<MarkdownBody text={`value ${src} ok`} />);
      expect(container.querySelector('.katex')).not.toBeNull();
    }
  });

  it('keeps a numbered list intact when an item contains display math', () => {
    const { container } = render(
      <MarkdownBody text={'1. Plug in: $$x^2$$\n2. Simplify\n3. Done'} />,
    );
    // The promotion must not fracture the list into multiple <ol>s.
    expect(container.querySelectorAll('ol')).toHaveLength(1);
    expect(container.querySelectorAll('ol > li')).toHaveLength(3);
    // The embedded math still renders (inline, not a display block).
    expect(container.querySelector('.katex')).not.toBeNull();
    expect(container.querySelector('.katex-display')).toBeNull();
  });

  it('keeps a GFM table intact when a cell contains math', () => {
    const md = '| a | b |\n| --- | --- |\n| $$x^2$$ | y |';
    const { container } = render(<MarkdownBody text={md} />);
    expect(container.querySelector('table')).not.toBeNull();
    expect(container.querySelectorAll('tbody td')).toHaveLength(2);
    expect(container.querySelector('.katex')).not.toBeNull();
  });

  // KaTeX emits a duplicate MathML tree next to the visual one, hidden ONLY by
  // `katex.min.css` (`.katex-mathml { clip-path: inset(50%) }`). Drop that
  // import and every equation renders twice — once as unstyled MathML text.
  // jsdom applies no stylesheets, so no render assertion can catch it; the
  // import order matters too, since `index.css` carries the `.chat-prose
  // .katex*` overrides that must win the cascade.
  it('imports the KaTeX stylesheet, before index.css', () => {
    const main = readFileSync(resolve(process.cwd(), 'src/main.tsx'), 'utf8');
    const katexAt = main.indexOf("import 'katex/dist/katex.min.css'");
    const indexAt = main.indexOf("import './index.css'");
    expect(katexAt).toBeGreaterThan(-1);
    expect(indexAt).toBeGreaterThan(katexAt);
  });

  it('emits the hidden MathML tree the stylesheet is there to hide', () => {
    const { container } = render(<MarkdownBody text={'$E = mc^2$'} />);
    expect(container.querySelector('.katex-mathml')).not.toBeNull();
  });
});
