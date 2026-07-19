import { describe, expect, it } from "vitest";

import eventFixture from "../protocol/fixtures/event.segment-persisted.json";
import requestFixture from "../protocol/fixtures/request.engine-get-state.json";
import responseFixture from "../protocol/fixtures/response.engine-get-state.json";

describe("shared protocol fixtures", () => {
  it("loads the canonical request, response, and event fixtures in TypeScript", () => {
    expect(requestFixture).toMatchObject({
      command: "engine.getState",
      protocolVersion: 1,
      type: "request",
    });

    expect(responseFixture).toMatchObject({
      ok: true,
      protocolVersion: 1,
      type: "response",
    });

    expect(eventFixture).toMatchObject({
      event: "segment.persisted",
      protocolVersion: 1,
      type: "event",
    });
  });
});
