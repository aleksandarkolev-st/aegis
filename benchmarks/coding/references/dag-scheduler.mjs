export async function runGraph(nodes, limit, execute) {
  if (!Number.isSafeInteger(limit) || limit <= 0 || !Array.isArray(nodes) || typeof execute !== 'function') throw new Error('Invalid scheduler input');
  const byId = new Map();
  for (const node of nodes) {
    if (!node || typeof node.id !== 'string' || !node.id || byId.has(node.id) || !Array.isArray(node.deps) || node.deps.some(dependency => typeof dependency !== 'string') || new Set(node.deps).size !== node.deps.length) throw new Error('Invalid node');
    byId.set(node.id, node);
  }
  for (const node of nodes) if (node.deps.some(dependency => !byId.has(dependency))) throw new Error('Missing dependency');
  const visited = new Set();
  while (visited.size < nodes.length) {
    const ready = nodes.filter(node => !visited.has(node.id) && node.deps.every(dependency => visited.has(dependency)));
    if (!ready.length) throw new Error('Cycle');
    ready.forEach(node => visited.add(node.id));
  }
  const result = new Map();
  const active = new Map();
  while (result.size < nodes.length) {
    let blocked;
    do {
      blocked = false;
      for (const node of nodes) if (!result.has(node.id) && !active.has(node.id) && node.deps.some(dependency => ['failed', 'blocked'].includes(result.get(dependency)?.status))) {
        result.set(node.id, { status: 'blocked' });
        blocked = true;
      }
    } while (blocked);
    for (const node of nodes) {
      if (active.size >= limit) break;
      if (result.has(node.id) || active.has(node.id) || !node.deps.every(dependency => result.get(dependency)?.status === 'succeeded')) continue;
      const values = Object.fromEntries(node.deps.map(dependency => [dependency, result.get(dependency).value]));
      const pending = Promise.resolve().then(() => execute(node.id, values)).then(value => result.set(node.id, { status: 'succeeded', value }), error => result.set(node.id, { status: 'failed', error: String(error.message ?? error) })).finally(() => active.delete(node.id));
      active.set(node.id, pending);
    }
    if (active.size) await Promise.race(active.values());
    else if (result.size < nodes.length) throw new Error('Unexpected deadlock');
  }
  return Object.fromEntries(result);
}
