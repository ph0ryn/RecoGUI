interface ErrorWithMessage {
  message: string;
}

function hasMessage(value: unknown): value is ErrorWithMessage {
  return (
    typeof value === "object" &&
    value !== null &&
    "message" in value &&
    typeof value.message === "string"
  );
}

export function messageFromUnknownError(error: unknown, fallback: string): string {
  if (error instanceof Error || hasMessage(error)) {
    return error.message.trim() || fallback;
  }

  if (typeof error === "string") {
    return error.trim() || fallback;
  }

  return fallback;
}
