import { readFile, readdir } from "node:fs/promises";
import { basename, resolve } from "node:path";

import addFormats from "ajv-formats";
import Ajv2020 from "ajv/dist/2020.js";

const root = resolve(import.meta.dirname, "..");
const schema = JSON.parse(
  await readFile(resolve(root, "protocol/reco-protocol.schema.json"), "utf8"),
);
const fixtureDirectory = resolve(root, "protocol/fixtures");
const invalidFixtureDirectory = resolve(root, "protocol/invalid-fixtures");
const fixtureNames = (await readdir(fixtureDirectory)).filter((name) => name.endsWith(".json"));
const invalidFixtureNames = (await readdir(invalidFixtureDirectory)).filter((name) =>
  name.endsWith(".json"),
);
const ajv = new Ajv2020({ allErrors: true, strict: true });

addFormats(ajv);

const validate = ajv.compile(schema);

const fail = (fixtureName, message) => {
  throw new Error(`${basename(fixtureName)}: ${message}`);
};

for (const fixtureName of fixtureNames) {
  const message = JSON.parse(await readFile(resolve(fixtureDirectory, fixtureName), "utf8"));

  if (!validate(message)) {
    fail(fixtureName, ajv.errorsText(validate.errors, { separator: "; " }));
  }
}

for (const fixtureName of invalidFixtureNames) {
  const message = JSON.parse(await readFile(resolve(invalidFixtureDirectory, fixtureName), "utf8"));

  if (validate(message)) {
    fail(fixtureName, "invalid fixture unexpectedly passed validation");
  }
}

console.log(
  `Validated ${fixtureNames.length} valid and ${invalidFixtureNames.length} invalid protocol fixtures.`,
);
