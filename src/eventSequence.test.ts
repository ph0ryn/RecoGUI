import { describe, expect, it } from "vitest";

import {
  compareDecimalStrings,
  eventsAfterSnapshot,
  incrementDecimalString,
  newestRevisionedValue,
} from "./eventSequence";

describe("decimal event sequences", () => {
  it("compares values beyond the JavaScript safe integer range", () => {
    expect(compareDecimalStrings("9007199254740993", "9007199254740992")).toBe(1);
    expect(compareDecimalStrings("9999999999999999", "10000000000000000")).toBe(-1);
    expect(compareDecimalStrings("42", "42")).toBe(0);
  });

  it("increments without converting to a number", () => {
    expect(incrementDecimalString("9")).toBe("10");
    expect(incrementDecimalString("9007199254740999")).toBe("9007199254741000");
  });

  it("does not let an older command response overwrite a newer revisioned event", () => {
    const current = { revision: "9007199254740993", value: "event" };

    expect(
      newestRevisionedValue(current, { revision: "9007199254740992", value: "response" }),
    ).toBe(current);

    expect(newestRevisionedValue(current, { revision: "9007199254740994", value: "next" })).toEqual(
      { revision: "9007199254740994", value: "next" },
    );
  });

  it("keeps only contiguous events newer than the snapshot", () => {
    const result = eventsAfterSnapshot("9007199254740992", [
      { sequence: "9007199254740994", type: "second" },
      { sequence: "9007199254740991", type: "old" },
      { sequence: "9007199254740993", type: "first" },
    ]);

    expect(result).toEqual({
      contiguous: true,
      events: [
        { sequence: "9007199254740993", type: "first" },
        { sequence: "9007199254740994", type: "second" },
      ],
    });
  });

  it("reports a gap so the caller can refetch a snapshot", () => {
    expect(
      eventsAfterSnapshot("10", [
        { sequence: "11", type: "first" },
        { sequence: "13", type: "gap" },
      ]),
    ).toEqual({ contiguous: false, events: [] });
  });
});
