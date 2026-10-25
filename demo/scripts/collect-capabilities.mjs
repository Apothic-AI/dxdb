// One-off evidence collector: prints each engine's capability probe + version.
// Not part of the test suite; run with `node scripts/collect-capabilities.mjs`.
import { chromium, firefox, webkit } from 'playwright-core';
import { spawn } from 'node:child_process';

const PORT = 4177;
const server = spawn(process.execPath, ['scripts/serve.mjs', String(PORT)], {
  stdio: 'ignore',
});
await new Promise((r) => setTimeout(r, 600));

for (const [name, type] of [['chromium', chromium], ['firefox', firefox], ['webkit', webkit]]) {
  const browser = await type.launch();
  const page = await browser.newPage();
  await page.goto(`http://127.0.0.1:${PORT}/`);
  await page.waitForFunction(() => document.body.dataset.status && document.body.dataset.status !== 'pending', null, { timeout: 30000 });
  const h = await page.evaluate(() => window.__dxdbHarness);
  console.log(`\n### ${name} ${browser.version()}`);
  console.log(`status: ${h.status}`);
  console.log(`ua: ${h.environment.userAgent}`);
  for (const c of h.capabilities) console.log(`  ${c.ok ? 'yes' : 'NO '}  ${c.name} — ${c.detail}`);
  console.log(`  wasm: ${JSON.stringify(h.wasm)}`);
  await browser.close();
}
server.kill();
process.exit(0);
