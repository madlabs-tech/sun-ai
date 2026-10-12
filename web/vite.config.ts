/// <reference types="node" />
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { gzipSync } from 'node:zlib';
import { defineConfig, type Plugin } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

/** Self-host (C5): the design system points icons at unpkg and fonts at Google Fonts.
 *  Icons are served from public/icons, Sora and JetBrains Mono from src/assets/fonts.
 *  The icon base is absolute: browsers resolve a url() inside a CSS variable against
 *  different bases (Chrome uses the stylesheet that reads it, assets/). */
const SELF_HOST: [file: string, from: RegExp, to: string][] = [
  ['madlabs-design-system/svelte/cx.mjs', /https:\/\/unpkg\.com\/lucide-static@[^/]+\/icons\//, '/icons/'],
  ['madlabs-design-system/tokens/fonts.css', /@import url\('https:\/\/fonts\.googleapis\.com[^)]*\);/, ''],
];

function self_host(): Plugin {
  return {
    name: 'sun-self-host',
    enforce: 'pre',
    transform(code, id) {
      const rule = SELF_HOST.find(([file]) => id.replace(/\\/g, '/').includes(file));
      if (!rule) return;
      const out = code.replace(rule[1], rule[2]);
      // A design-system upgrade changed the CDN line: fail the build, never ship a third-party request.
      if (out === code) this.error(`self-host: no CDN reference to replace in ${rule[0]}`);
      return out;
    },
  };
}

/** Pre-compress text assets; sun-api serves `<file>.gz` when the browser accepts gzip. */
function gzip_assets(): Plugin {
  let out_dir = 'dist';
  return {
    name: 'sun-gzip',
    apply: 'build',
    configResolved: (c) => void (out_dir = c.build.outDir),
    closeBundle() {
      for (const name of readdirSync(out_dir, { recursive: true }) as string[]) {
        if (!/\.(html|js|css|svg|json|txt)$/.test(name)) continue;
        const path = join(out_dir, name);
        const raw = readFileSync(path);
        if (raw.length > 1024) writeFileSync(path + '.gz', gzipSync(raw, { level: 9 }));
      }
    },
  };
}

// Static build, embedded in the sun binary with rust-embed.
// In dev, /api is proxied to a local kernel; add ?mock=1 to run without one.
export default defineConfig({
  plugins: [self_host(), svelte(), gzip_assets()],
  base: './',
  // The mock reads the role templates from ../templates (dev server file access).
  server: { proxy: { '/api': 'http://127.0.0.1:7700' }, fs: { allow: ['.', '../templates'] } },
  optimizeDeps: { exclude: ['@hryer/madlabs-design-system'] },
});
