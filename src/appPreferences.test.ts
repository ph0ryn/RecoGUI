import { describe, expect, it } from "vitest";

import { defaultAppPreferences, loadAppPreferences, saveAppPreferences } from "./appPreferences";

function createStorage(): Storage {
  const values = new Map<string, string>();

  return {
    clear: () => values.clear(),
    getItem: (key) => values.get(key) ?? null,
    key: (index) => [...values.keys()][index] ?? null,
    get length() {
      return values.size;
    },
    removeItem: (key) => values.delete(key),
    setItem: (key, value) => values.set(key, value),
  };
}

describe("app preferences", () => {
  it("round-trips the default input device", () => {
    const storage = createStorage();

    saveAppPreferences({ defaultInputDeviceId: "7" }, storage);

    expect(loadAppPreferences(storage)).toEqual({ defaultInputDeviceId: "7" });
  });

  it("migrates the legacy input device preference", () => {
    const storage = createStorage();

    storage.setItem("reco.defaultInputDeviceId", "legacy-device");

    expect(loadAppPreferences(storage)).toEqual({ defaultInputDeviceId: "legacy-device" });
  });

  it("uses safe defaults for invalid persisted values", () => {
    const storage = createStorage();

    storage.setItem("reco.appPreferences", JSON.stringify({ defaultInputDeviceId: 7 }));

    expect(loadAppPreferences(storage)).toEqual(defaultAppPreferences);
  });
});
