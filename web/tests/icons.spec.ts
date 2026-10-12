import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { expect, test } from '@playwright/test';

// Icons are self-hosted from public/icons (vite.config.ts); a name without its SVG renders blank.
const src = new URL('../src/', import.meta.url);
const has_icon = (name: string) => existsSync(new URL(`../public/icons/${name}.svg`, import.meta.url));

const sources = (readdirSync(src, { recursive: true }) as string[])
  .filter((file) => /\.(svelte|ts)$/.test(file))
  .map((file) => [file, readFileSync(new URL(file, src), 'utf8')] as const);

const ICON_LITERAL = /(?:\b(?:icon|iconLeft|iconRight)="([a-z0-9-]+)"|<Icon name="([a-z0-9-]+)"|\bicon: '([a-z0-9-]+)')/g;

test('every literal icon name in src has its SVG', () => {
  const missing = sources.flatMap(([file, code]) =>
    [...code.matchAll(ICON_LITERAL)].map((m) => m[1] ?? m[2] ?? m[3]).filter((name) => !has_icon(name)).map((name) => `${file}: ${name}`),
  );
  expect(missing).toEqual([]);
});

test('every audit ACTION icon has its SVG', () => {
  const audit = readFileSync(new URL('screens/SettingsAudit.svelte', src), 'utf8');
  const table = audit.match(/const ACTION[^{]*\{([\s\S]*?)\};/)?.[1] ?? '';
  const names = [...table.matchAll(/\['([a-z0-9-]+)', '/g)].map((m) => m[1]);
  expect(names.length).toBeGreaterThan(5);
  expect(names.filter((name) => !has_icon(name))).toEqual([]);
});
