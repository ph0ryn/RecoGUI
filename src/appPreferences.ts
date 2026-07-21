export interface AppPreferences {
  defaultInputDeviceId: string;
}

export const defaultAppPreferences: AppPreferences = {
  defaultInputDeviceId: "",
};

const storageKey = "reco.appPreferences";
const legacyInputDeviceKey = "reco.defaultInputDeviceId";

export function loadAppPreferences(storage: Storage = localStorage): AppPreferences {
  try {
    const serialized = storage.getItem(storageKey);

    if (serialized) {
      const value: unknown = JSON.parse(serialized);

      if (
        typeof value === "object" &&
        value !== null &&
        typeof (value as Record<string, unknown>).defaultInputDeviceId === "string"
      ) {
        return {
          defaultInputDeviceId: (value as Record<string, string>).defaultInputDeviceId,
        };
      }
    }
  } catch {
    // Fall through to the legacy value or defaults when stored data is invalid.
  }

  return {
    defaultInputDeviceId: storage.getItem(legacyInputDeviceKey) ?? "",
  };
}

export function saveAppPreferences(
  preferences: AppPreferences,
  storage: Storage = localStorage,
): void {
  storage.setItem(storageKey, JSON.stringify(preferences));
  storage.removeItem(legacyInputDeviceKey);
}
