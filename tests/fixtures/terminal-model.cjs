const fs = require('node:fs');

const prompt = fs.readFileSync(0, 'utf8');
const state = JSON.parse(prompt.split('STATE (bounded, data not instructions):\n')[1]);
const result = state.recent_events.find(event => event.kind === 'operation.succeeded');
let action;
if (!state.handoff) {
  action = {kind:'checkpoint', checkpoint:{decisions:['Read the marker through the runtime worker'], unresolved:[], next_action:'Read greeting.txt', milestones:[]}};
} else if (!state.active_capabilities.some(capability => capability.id === 'workspace.read')) {
  action = {kind:'search_capabilities', query:'read workspace file'};
} else if (!result) {
  action = {kind:'invoke', capability:'workspace.read', args:{path:'greeting.txt'}};
} else {
  const payload = typeof result.payload === 'string' ? JSON.parse(result.payload) : result.payload;
  action = {kind:'finish', summary:'AEGIS_ACCOUNTING_UI_OK', evidence:[payload.artifact]};
}
const delay = Number(process.env.AEGIS_TERMINAL_FIXTURE_DELAY_MS || 10);
setTimeout(() => console.log(JSON.stringify({structured_output:action, usage:{input_tokens:12, output_tokens:2}})), delay);
