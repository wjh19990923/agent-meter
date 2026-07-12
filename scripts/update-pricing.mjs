import fs from 'node:fs';

const input = process.argv[2];
if (!input) throw new Error('Usage: node scripts/update-pricing.mjs <litellm-pricing.json>');
const source = JSON.parse(fs.readFileSync(input, 'utf8'));
const compact = {};

for (const [model, value] of Object.entries(source)) {
  if (!value || typeof value !== 'object') continue;
  const inputCost = Number(value.input_cost_per_token || 0);
  const outputCost = Number(value.output_cost_per_token || 0);
  if (!inputCost && !outputCost) continue;
  compact[model] = {
    input: inputCost,
    output: outputCost,
    cacheRead: Number(value.cache_read_input_token_cost || inputCost),
    cacheWrite: Number(value.cache_creation_input_token_cost || inputCost)
  };
}

fs.writeFileSync(new URL('../src-tauri/pricing.json', import.meta.url), JSON.stringify(compact));
console.log(`Wrote ${Object.keys(compact).length} model prices.`);
