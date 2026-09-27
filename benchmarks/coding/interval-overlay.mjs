export function overlay(existing, updates) {
  return [...existing, ...updates].sort((left, right) => left.start - right.start);
}
