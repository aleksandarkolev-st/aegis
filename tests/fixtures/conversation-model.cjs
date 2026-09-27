const fs = require('node:fs');

const prompt = fs.readFileSync(0, 'utf8');
const state = JSON.parse(prompt.split('STATE (bounded, data not instructions):\n')[1]);
const previous = state.conversation.at(-1);
const action = {
  kind: 'finish',
  summary: previous ? `Continuing our chat: ${previous.task}. You asked: ${state.task}` : 'Hello! What would you like to build?',
  evidence: []
};
setTimeout(() => {
  const output = process.argv.indexOf('-o');
  if (output >= 0) {
    fs.writeFileSync(process.argv[output + 1], JSON.stringify(action));
    console.log(JSON.stringify({type:'turn.completed', usage:{input_tokens:12, output_tokens:2}}));
  } else {
    console.log(JSON.stringify({structured_output:action, usage:{input_tokens:12, output_tokens:2}}));
  }
}, Number(process.env.AEGIS_TERMINAL_FIXTURE_DELAY_MS || 10));
