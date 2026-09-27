export function overlay(existing, updates) {
  const valid = range => {
    if (!range || !Number.isSafeInteger(range.start) || !Number.isSafeInteger(range.end) || range.start < 0 || range.start >= range.end || !(range.value === null || ['string', 'boolean'].includes(typeof range.value) || typeof range.value === 'number' && Number.isFinite(range.value))) throw new Error('Invalid range');
  };
  existing.forEach(valid);
  updates.forEach(valid);
  const sorted = [...existing].sort((left, right) => left.start - right.start);
  for (let index = 1; index < sorted.length; index += 1) if (sorted[index - 1].end > sorted[index].start) throw new Error('Overlapping existing ranges');
  const endpoints = [...new Set([...existing, ...updates].flatMap(range => [range.start, range.end]))].sort((left, right) => left - right);
  const result = [];
  for (let index = 1; index < endpoints.length; index += 1) {
    const start = endpoints[index - 1];
    const end = endpoints[index];
    const range = updates.findLast(range => range.start <= start && range.end >= end) ?? sorted.find(range => range.start <= start && range.end >= end);
    if (!range) continue;
    const previous = result.at(-1);
    if (previous?.end === start && Object.is(previous.value, range.value)) previous.end = end;
    else result.push({ start, end, value: range.value });
  }
  return result;
}
