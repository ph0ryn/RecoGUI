import { defineConfig } from "oxfmt";

export default defineConfig({
  arrowParens: "always",
  bracketSpacing: true,
  endOfLine: "lf",
  ignorePatterns: ["package.json", "pnpm-*.yaml", "*.md", "Cargo.toml", "pyproject.toml"],
  insertFinalNewline: true,
  printWidth: 100,
  semi: true,
  singleQuote: false,
  sortImports: {
    groups: [
      ["value-builtin"],
      ["value-external"],
      ["value-internal"],
      ["value-parent", "value-sibling", "value-index"],
      ["type-import"],
      ["unknown"],
    ],
    newlinesBetween: true,
    order: "asc",
  },
  sortPackageJson: false,
  tabWidth: 2,
  trailingComma: "all",
  useTabs: false,
});
