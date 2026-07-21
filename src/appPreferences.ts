export interface AppPreferences {
  defaultInputDeviceId: string;
  transcriptionLanguage: string | null;
}

export const defaultAppPreferences: AppPreferences = {
  defaultInputDeviceId: "",
  transcriptionLanguage: null,
};

const storageKey = "reco.appPreferences";
const legacyInputDeviceKey = "reco.defaultInputDeviceId";

export function loadAppPreferences(storage: Storage = localStorage): AppPreferences {
  try {
    const serialized = storage.getItem(storageKey);

    if (serialized) {
      const value: unknown = JSON.parse(serialized);

      if (typeof value === "object" && value !== null) {
        const stored = value as Record<string, unknown>;

        if (
          typeof stored.defaultInputDeviceId === "string" &&
          (stored.transcriptionLanguage === undefined ||
            stored.transcriptionLanguage === null ||
            typeof stored.transcriptionLanguage === "string")
        ) {
          let transcriptionLanguage: string | null = null;

          if (typeof stored.transcriptionLanguage === "string") {
            transcriptionLanguage = stored.transcriptionLanguage;
          }

          return {
            defaultInputDeviceId: stored.defaultInputDeviceId,
            transcriptionLanguage,
          };
        }
      }
    }
  } catch {
    // Fall through to the legacy value or defaults when stored data is invalid.
  }

  return {
    defaultInputDeviceId: storage.getItem(legacyInputDeviceKey) ?? "",
    transcriptionLanguage: null,
  };
}

export function saveAppPreferences(
  preferences: AppPreferences,
  storage: Storage = localStorage,
): void {
  storage.setItem(storageKey, JSON.stringify(preferences));
  storage.removeItem(legacyInputDeviceKey);
}
