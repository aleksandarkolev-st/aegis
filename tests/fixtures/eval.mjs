import readline from 'node:readline';

const count = Number(process.argv[2]);
if (!Number.isInteger(count) || count < 1 || count > 500) process.exit(2);

const names = ['search_code', 'search_repositories', 'search_commits', 'grep', 'find', 'read_file'];
const tools = Array.from({ length: count }, (_, index) => ({
  name: index === 0 ? 'build_log' : names[index % names.length] + '_' + String(index).padStart(3, '0'),
  description: index === 0 ? 'Read compiler output from the latest build log' : 'Find text in repository files by literal substring; search code, logs, and documents',
  inputSchema: { type: 'object', properties: { query: { type: 'string' } }, required: ['query'] },
}));

const input = readline.createInterface({ input: process.stdin });
const send = (id, result) => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id, result }) + '\n');

input.on('line', (line) => {
  const request = JSON.parse(line);
  if (request.method === 'initialize') {
    send(request.id, {
      protocolVersion: request.params.protocolVersion,
      capabilities: { tools: { listChanged: false } },
      serverInfo: { name: 'eval', version: '1.0.0' },
    });
  } else if (request.method === 'tools/list') {
    send(request.id, { tools });
  } else if (request.method === 'tools/call') {
    const text = request.params.name === 'build_log'
      ? 'warning: unused temporary value\n'.repeat(70000) + 'error: AEGIS_EVAL_LOG_FAILURE at codec.rs:73\n'
      : 'No matching repository result';
    send(request.id, { content: [{ type: 'text', text }], isError: false });
  }
});
