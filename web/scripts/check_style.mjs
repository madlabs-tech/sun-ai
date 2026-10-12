// Design-system lint, run by `npm run check`: fails on raw hex colours, emoji and
// Unicode-as-icon glyphs in web/src. Colours come from Madlabs tokens (var(--...)), icons
// from <Icon>; the only Unicode allowed as an icon is → · ≈ ⌘ ⌫ (madlabs readme).
import { readdirSync, readFileSync } from 'node:fs';
import { join, relative } from 'node:path';

const ROOT = new URL('../src', import.meta.url).pathname;
// Mocked kernel responses: they carry real tool output (Maestro's ✅, a user's CSS file), not UI.
const SKIP = new Set(['lib/api/mock.ts']);
const RULES = [
  ['raw hex colour, use a Madlabs token', /(?<![&\w])#(?:[0-9a-f]{8}|[0-9a-f]{6}|[0-9a-f]{3,4})\b/i],
  ['emoji, not used in Madlabs UI', /\p{Extended_Pictographic}/u],
  ['Unicode icon, use <Icon>', /[←↑↓⇐⇑⇒⇓✓✔✕✖✗✘☰⋮⋯▲△▼▽▶▷◀◁▸▹▾▿◂◃►◄★☆●○■□]/u],
];

const files = (dir) =>
  readdirSync(dir, { withFileTypes: true }).flatMap((e) =>
    e.isDirectory() ? files(join(dir, e.name)) : /\.(svelte|ts|js|css|html)$/.test(e.name) ? [join(dir, e.name)] : [],
  );

let failures = 0;
for (const file of files(ROOT)) {
  const name = relative(ROOT, file);
  if (SKIP.has(name)) continue;
  readFileSync(file, 'utf8')
    .split('\n')
    .forEach((line, i) => {
      for (const [why, re] of RULES) {
        const hit = line.match(re);
        if (hit) {
          failures++;
          console.error(`src/${name}:${i + 1}: ${why}: ${hit[0]}`);
        }
      }
    });
}
if (failures) {
  console.error(`check_style: ${failures} design-system violation(s)`);
  process.exit(1);
}
