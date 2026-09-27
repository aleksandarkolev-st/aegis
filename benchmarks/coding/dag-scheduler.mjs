export async function runGraph(nodes, limit, execute) {
  const result = {};
  for (const node of nodes) {
    result[node.id] = { status: 'succeeded', value: await execute(node.id, result) };
  }
  return result;
}
