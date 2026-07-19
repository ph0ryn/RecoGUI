import { readFile, readdir } from "node:fs/promises";
import { basename, resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");
const schema = JSON.parse(
  await readFile(resolve(root, "protocol/reco-protocol.schema.json"), "utf8"),
);
const fixtureDirectory = resolve(root, "protocol/fixtures");
const fixtureNames = (await readdir(fixtureDirectory)).filter((name) => name.endsWith(".json"));

const fail = (fixtureName, message) => {
  throw new Error(`${basename(fixtureName)}: ${message}`);
};

for (const fixtureName of fixtureNames) {
  const message = JSON.parse(await readFile(resolve(fixtureDirectory, fixtureName), "utf8"));

  if (message.protocolVersion !== 1) {
    fail(fixtureName, "protocolVersion must be 1");
  }

  if (!schema.$defs[message.type]) {
    fail(fixtureName, `unsupported message type ${String(message.type)}`);
  }

  for (const field of ["requestId", "sessionId", "sequence", "payload"]) {
    if (!(field in message)) {
      fail(fixtureName, `missing ${field}`);
    }
  }

  if (!Number.isInteger(message.sequence) || message.sequence < 1) {
    fail(fixtureName, "sequence must be a positive integer");
  }
}

console.log(`Validated ${fixtureNames.length} protocol fixtures.`);
