import readline from 'node:readline';

const input = readline.createInterface({ input: process.stdin });
const send = (id, result) => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id, result }) + '\n');

input.on('line', (line) => {
  const request = JSON.parse(line);
  if (request.method === 'initialize') {
    send(request.id, {
      protocolVersion: request.params.protocolVersion,
      capabilities: { tools: { listChanged: false } },
      serverInfo: { name: 'fixture', version: '1.0.0' },
    });
  } else if (request.method === 'tools/list') {
    send(request.id, { tools: [
      { name: 'echo', description: 'Echo a provided text', inputSchema: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'] } },
      { name: 'ungranted', description: 'Do not expose this tool', inputSchema: { type: 'object', properties: {} } },
    ] });
  } else if (request.method === 'tools/call') {
    send(request.id, { content: [{ type: 'text', text: request.params.arguments.text ?? '' }], isError: false });
  }
});
