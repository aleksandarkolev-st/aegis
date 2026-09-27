export function applyPatch(document, operations) {
  const result = structuredClone(document);
  for (const operation of operations) {
    const key = operation.path.slice(1);
    if (operation.op === 'remove') delete result[key];
    else result[key] = structuredClone(operation.value);
  }
  return result;
}
