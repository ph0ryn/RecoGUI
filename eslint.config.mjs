import oxlint from "eslint-plugin-oxlint";
import { defineConfig } from "eslint/config";
import tseslint from "typescript-eslint";

export default defineConfig(
  {
    ignores: ["dist/**"],
  },
  {
    files: ["src/**/*.ts"],
    languageOptions: {
      parser: tseslint.parser,
      parserOptions: {
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
      },
    },
    plugins: {
      "@typescript-eslint": tseslint.plugin,
    },
    rules: {
      "@typescript-eslint/naming-convention": [
        "error",
        {
          format: ["StrictPascalCase"],
          selector: "typeLike",
        },
        {
          format: ["strictCamelCase", "UPPER_CASE"],
          modifiers: ["const"],
          selector: "variable",
        },
        {
          format: ["strictCamelCase"],
          selector: "variable",
        },
        {
          format: ["strictCamelCase"],
          selector: "function",
        },
        {
          format: ["strictCamelCase"],
          leadingUnderscore: "allow",
          selector: "parameter",
        },
      ],
    },
  },
  oxlint.buildFromOxlintConfigFile("./oxlint.config.ts"),
);
