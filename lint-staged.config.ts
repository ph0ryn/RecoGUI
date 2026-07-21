export default {
  "**/!(package).json": "oxfmt",
  "*.{js,mjs}": ["oxfmt", "eslint --fix"],
  "package.json": "sort-package-json",
  "src/**/*.{ts,tsx}": ["oxfmt", "eslint --fix", "oxlint --type-aware --type-check --fix"],
  "{lint-staged,oxfmt,oxlint,vite}.config.ts": "oxfmt",
};
