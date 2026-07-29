import tseslint from 'typescript-eslint';
import reactHooks from 'eslint-plugin-react-hooks';
import globals from 'globals';

// Static-analysis gate for the dashboard. The vitest suite exercises pure
// reducers and never renders (see docs/web-unit-tests.md), so a whole
// class of *wiring* bugs — a value that isn't a real boolean used in a
// condition, a condition whose outcome is constant — is invisible to it and to
// `tsc`. `!isCollapsed` where `isCollapsed` is an imported function (the cron
// block that shipped blank, 2026-07-17) type-checks and passes every test; the
// two type-aware rules below reject it. Scope: `src` only — config files carry
// no such logic. See docs/web-unit-tests.md for the suite + roadmap.
export default tseslint.config(
  { ignores: ['dist', 'node_modules', 'src/api/schema.d.ts'] },
  {
    files: ['src/**/*.{ts,tsx}'],
    plugins: {
      '@typescript-eslint': tseslint.plugin,
      'react-hooks': reactHooks,
    },
    languageOptions: {
      parser: tseslint.parser,
      parserOptions: {
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
      },
      globals: { ...globals.browser },
    },
    rules: {
      // Classic hook safety. `rules-of-hooks` catches a conditionally-called
      // hook; `exhaustive-deps` stays a warning, as upstream ships it.
      'react-hooks/rules-of-hooks': 'error',
      'react-hooks/exhaustive-deps': 'warn',

      // The wiring-bug class the pure-reducer tests can't see. `allowNumber:
      // false` is deliberate: it rejects `{count && <Badge/>}`, which renders a
      // literal `0` when count is 0 — write `count > 0 && …`.
      '@typescript-eslint/strict-boolean-expressions': ['error', { allowNumber: false }],
      '@typescript-eslint/no-unnecessary-condition': 'error',
    },
  },
);
