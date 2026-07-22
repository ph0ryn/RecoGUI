export interface SequencedEvent {
  sequence: string;
}

export interface RevisionedValue {
  revision: string;
}

export interface BufferedEventResult<Event extends SequencedEvent> {
  contiguous: boolean;
  events: Event[];
}

const decimalPattern = /^(?:0|[1-9]\d*)$/;

export function compareDecimalStrings(left: string, right: string): -1 | 0 | 1 {
  assertDecimalString(left);
  assertDecimalString(right);

  if (left.length !== right.length) {
    if (left.length < right.length) {
      return -1;
    }

    return 1;
  }

  if (left === right) {
    return 0;
  }

  if (left < right) {
    return -1;
  }

  return 1;
}

export function incrementDecimalString(value: string): string {
  assertDecimalString(value);

  const digits = value.split("");
  let carry = 1;

  for (let index = digits.length - 1; index >= 0 && carry === 1; index -= 1) {
    const digit = Number(digits[index]) + carry;

    digits[index] = String(digit % 10);
    carry = 0;

    if (digit >= 10) {
      carry = 1;
    }
  }

  if (carry === 1) {
    digits.unshift("1");
  }

  return digits.join("");
}

export function newestRevisionedValue<Value extends RevisionedValue>(
  current: Value,
  incoming: Value,
): Value {
  if (compareDecimalStrings(incoming.revision, current.revision) >= 0) {
    return incoming;
  }

  return current;
}

export function eventsAfterSnapshot<Event extends SequencedEvent>(
  snapshotSequence: string,
  bufferedEvents: readonly Event[],
): BufferedEventResult<Event> {
  assertDecimalString(snapshotSequence);

  const events = Array.from(bufferedEvents)
    .filter(({ sequence }) => compareDecimalStrings(sequence, snapshotSequence) > 0)
    .sort(({ sequence: left }, { sequence: right }) => compareDecimalStrings(left, right));
  let expected = incrementDecimalString(snapshotSequence);

  for (const event of events) {
    if (event.sequence !== expected) {
      return { contiguous: false, events: [] };
    }

    expected = incrementDecimalString(event.sequence);
  }

  return { contiguous: true, events };
}

function assertDecimalString(value: string): void {
  if (!decimalPattern.test(value)) {
    throw new Error(`Invalid decimal sequence: ${value}`);
  }
}
