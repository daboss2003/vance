import prettier from 'eslint-config-prettier';
import js from '@eslint/js';
import svelte from 'eslint-plugin-svelte';
import ts from 'typescript-eslint';
import globals from 'globals';

const tsParser = ts.parser;

export default [
  js.configs.recommended,
  ...svelte.configs['flat/recommended'],
  prettier,
  ...svelte.configs['flat/prettier'],
  {
    languageOptions: {
      globals: {
        ...globals.browser,
        ...globals.node
      }
    }
  },
  // The UI is TypeScript: parse `.ts` files and `<script lang="ts">` blocks.
  {
    files: ['**/*.ts'],
    languageOptions: { parser: tsParser }
  },
  {
    files: ['**/*.svelte', '**/*.svelte.ts'],
    languageOptions: { parserOptions: { parser: tsParser } }
  },
  {
    files: ['**/*.ts', '**/*.svelte'],
    plugins: { '@typescript-eslint': ts.plugin },
    rules: {
      // Intentional: off for TypeScript — `no-undef` doesn't understand
      // type-only names, and svelte-check (part of `pnpm lint`) already
      // reports undefined identifiers with full type information.
      'no-undef': 'off',
      // The core rule flags parameter names inside type signatures
      // (`(rgb: Rgb) => void`); the TS-aware rule doesn't. `_name` = unused on purpose.
      'no-unused-vars': 'off',
      '@typescript-eslint/no-unused-vars': [
        'error',
        { argsIgnorePattern: '^_', varsIgnorePattern: '^_', caughtErrors: 'none' }
      ]
    }
  },
  {
    ignores: ['build/', '.svelte-kit/', 'node_modules/']
  }
];
