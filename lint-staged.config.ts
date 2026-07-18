export default {
  "**/!(package).json": "pnpm oxfmt",
  "*.{js,mjs}": () => "pnpm run format",
  "package.json": () => "sort-package-json",
  "src/**/*.ts": () => "pnpm run precommit",
};
