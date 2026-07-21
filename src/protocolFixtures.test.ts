import { describe, expect, it } from "vitest";

import eventFixture from "../protocol/fixtures/event.segment-persisted.json";
import requestFixture from "../protocol/fixtures/request.engine-get-state.json";
import modelSelectFixture from "../protocol/fixtures/request.model-select.json";
import responseFixture from "../protocol/fixtures/response.engine-get-state.json";
import modelListFixture from "../protocol/fixtures/response.model-list.json";

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

    expect(modelSelectFixture).toMatchObject({
      command: "model.select",
      payload: { repoId: expect.any(String), revision: expect.any(String) },
    });

    expect(modelListFixture.payload.models).toEqual([
      expect.objectContaining({ repoId: expect.any(String), revision: expect.any(String) }),
    ]);

    expect(eventFixture).toMatchObject({
      event: "segment.persisted",
      payload: {
        characters: 3,
        mediaDurationMs: 1_000,
        rowVersion: 3,
        segment: { segmentIndex: 0 },
        totalSegments: 1,
      },
      protocolVersion: 1,
      sequence: 2,
      type: "event",
    });
  });
});
