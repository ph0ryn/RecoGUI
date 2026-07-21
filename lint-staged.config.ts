import type { Configuration } from "lint-staged";

const config = {
  "**/!(package).json": "pnpm oxfmt",
  "*.config.ts": "oxfmt",
  "*.mjs": ["oxfmt", "eslint --fix"],
  "*.{css,html}": "oxfmt",
  "package.json": "sort-package-json",
  "protocol/**/*.json": "oxfmt",
  "src-python/**/*.py": [
    "uv run --project src-python ruff check --fix --unsafe-fixes",
    "uv run --project src-python ruff format",
  ],
  "src-python/pyproject.toml": "uv run --project src-python pyproject-fmt",
  "src-tauri/**/*.rs": "rustfmt --edition 2024 --config skip_children=true",
  "src-tauri/Cargo.toml": () =>
    "cargo metadata --manifest-path src-tauri/Cargo.toml --no-deps --format-version 1",
  "src/**/*.{ts,tsx}": ["oxfmt", "eslint --fix", "oxlint --type-aware --type-check --fix"],
  "{tsconfig,tsconfig.node}.json": "oxfmt",
} satisfies Configuration;

export default config;
