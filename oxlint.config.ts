import { defineConfig } from "oxlint";

interface ImportMeta {
  resolve(specifier: string): string;
}

export default defineConfig({
  categories: {
    correctness: "error",
    nursery: "warn",
    pedantic: "off",
    restriction: "off",
    style: "error",
    suspicious: "off",
  },
  env: {
    node: true,
  },
  ignorePatterns: ["src-tauri/**"],
  jsPlugins: [
    {
      name: "@stylistic",
      specifier: (import.meta as ImportMeta).resolve("@stylistic/eslint-plugin"),
    },
  ],
  plugins: ["typescript"],
  rules: {
    "capitalized-comments": "off",
    "func-style": "off",
    "id-length": "off",
    "max-statements": "off",
    "no-inferrable-types": "off",
    "no-magic-numbers": "off",
    "prefer-destructuring": "off",
    "sort-imports": "off",

    "@stylistic/no-multiple-empty-lines": [
      "error",
      {
        max: 1,
        maxEOF: 0,
      },
    ],
    "@stylistic/padding-line-between-statements": [
      "error",
      {
        blankLine: "always",
        prev: "*",
        next: ["return", "multiline-expression", "block-like", "try", "throw"],
      },
      {
        blankLine: "always",
        prev: ["multiline-expression", "block-like", "const", "let"],
        next: "*",
      },
      {
        blankLine: "any",
        prev: ["const", "let"],
        next: ["const", "let"],
      },
    ],
  },
  overrides: [
    {
      files: ["oxlint.config.ts"],
      rules: {
        "sort-keys": "off",
      },
    },
  ],
});
