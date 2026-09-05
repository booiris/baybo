import tseslint from 'typescript-eslint';
import reactHooks from 'eslint-plugin-react-hooks';
import globals from 'globals';

// Static-analysis gate for the iOS transcript bundle. Like app/web, the vitest
// suite exercises pure reducers (the fold/merge helpers in Transcript.tsx), so a
// whole class of *wiring* bugs — a value that isn't a real boolean used in a
// condition, `{count && …}` printing a literal 0, a conditional that silently
// never fires — is invisible to it and to `tsc`. The two type-aware rules below
// reject them. Scope: `src` only. Mirrors app/web/eslint.config.js.
export default tseslint.config(
  { ignores: ['dist', 'node_modules'] },
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
      'react-hooks/rules-of-hooks': 'error',
      'react-hooks/exhaustive-deps': 'warn',
      // `allowNumber: false` rejects `{count && <X/>}`, which renders a literal 0.
      '@typescript-eslint/strict-boolean-expressions': ['error', { allowNumber: false }],
      '@typescript-eslint/no-unnecessary-condition': 'error',
    },
  },
);
