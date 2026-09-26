import readline from 'node:readline';

const count = Number(process.argv[2]);
if (![50, 100, 250, 500].includes(count)) process.exit(2);

const names = ['search_code', 'search_repositories', 'search_commits', 'grep', 'find', 'read_file'];
const tools = Array.from({ length: count }, (_, index) => ({
  name: names[index % names.length] + '_' + String(index).padStart(3, '0'),
  description: 'Find text in repository files by literal substring; search code, logs, and documents',
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
    send(request.id, { content: [{ type: 'text', text: 'No matching repository result' }], isError: false });
  }
});
