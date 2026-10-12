// Design-canvas pins: what the preview bridge may say, design token checks, and the
// steer message a pin becomes. The bridge runs inside agent-written code, so its messages are
// untrusted input: parse_bridge accepts only the documented shapes, bounded like the kernel.
import colors_css from '@hryer/madlabs-design-system/tokens/colors.css?raw';
import spacing_css from '@hryer/madlabs-design-system/tokens/spacing.css?raw';
import typography_css from '@hryer/madlabs-design-system/tokens/typography.css?raw';
import type { DesignFile, Pin, PinTarget } from './api/types';

export type BridgeMessage =
  | { sun: 'ready' | 'pick_cancelled' }
  | { sun: 'picked'; target: PinTarget }
  | { sun: 'scroll'; x: number; y: number };

const is_text = (v: unknown, max: number): v is string => typeof v === 'string' && v.length <= max;
const is_number = (v: unknown): v is number => typeof v === 'number' && Number.isFinite(v) && Math.abs(v) <= 1e6;
const is_record = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null && !Array.isArray(v);

function parse_target(v: unknown): PinTarget | null {
  if (!is_record(v)) return null;
  const { selector, tag, text, width, rect, styles, source, path = '' } = v;
  if (!is_text(path, 1000) || (path && (!path.startsWith('/') || /[\s\p{Cc}]/u.test(path)))) return null;
  if (!is_text(selector, 1000) || !selector.trim() || !is_text(tag, 64) || !tag || !is_text(text, 500)) return null;
  if (!is_number(width) || width < 1 || width > 10_000 || !is_record(rect) || !is_record(styles)) return null;
  const box = { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
  if (!Object.values(box).every(is_number)) return null;
  const entries = Object.entries(styles);
  if (entries.length > 64 || !entries.every(([k, s]) => k.length <= 64 && is_text(s, 500))) return null;
  let loc: PinTarget['source'] = null;
  if (source != null) {
    if (!is_record(source) || !is_text(source.file, 1000) || !source.file) return null;
    const line = Number(source.line), column = Number(source.column);
    if (!Number.isInteger(line) || !Number.isInteger(column) || line < 0 || column < 0) return null;
    loc = { file: source.file, line, column };
  }
  return {
    selector, tag, text, width: Math.round(width), rect: box as PinTarget['rect'],
    styles: Object.fromEntries(entries) as Record<string, string>, source: loc, path,
  };
}

/** A bridge message, or null when it isn't one of the documented shapes. */
export function parse_bridge(data: unknown): BridgeMessage | null {
  if (!is_record(data)) return null;
  switch (data.sun) {
    case 'ready':
    case 'pick_cancelled':
      return { sun: data.sun };
    case 'scroll':
      return is_number(data.x) && is_number(data.y) ? { sun: 'scroll', x: data.x, y: data.y } : null;
    case 'picked': {
      const target = parse_target(data.target);
      return target && { sun: 'picked', target };
    }
  }
  return null;
}

// ---------- Design token checks ----------

type Token = { name: string; value: number[] };
type Entry = readonly [name: string, value: string];
/** CSS custom properties and SCSS variables, named as written: `--name: value` / `$name: value`. */
const declarations = (css: string): Entry[] =>
  [...css.matchAll(/((?:--|\$)[\w-]+)\s*:\s*([^;}]+)/g)].map((m) => [m[1], m[2].trim()] as const);

/** JSON tokens (W3C `$value`, Style Dictionary `value`, or plain leaves) as `group-name` entries. */
function json_entries(text: string): Entry[] {
  let root: unknown;
  try {
    root = JSON.parse(text);
  } catch {
    return [];
  }
  const out: Entry[] = [];
  const walk = (v: unknown, name: string) => {
    if (typeof v === 'string' || typeof v === 'number') out.push([name, String(v)]);
    else if (is_record(v)) {
      const leaf = v.$value ?? v.value;
      if (typeof leaf === 'string' || typeof leaf === 'number') out.push([name, String(leaf)]);
      else for (const [k, child] of Object.entries(v)) if (!k.startsWith('$')) walk(child, name ? `${name}-${k}` : k);
    }
  };
  walk(root, '');
  return out;
}

/** `#rgb[a]`, `#rrggbb[aa]`, `rgb()`/`rgba()` → [r, g, b, a] (a rounded to 2 places), else null. */
export function parse_color(text: string): number[] | null {
  const hex = /^#([0-9a-f]{3,8})$/i.exec(text.trim());
  if (hex) {
    let h = hex[1];
    if (h.length <= 4) h = [...h].map((c) => c + c).join('');
    if (h.length !== 6 && h.length !== 8) return null;
    const n = (i: number) => parseInt(h.slice(i, i + 2), 16);
    return [n(0), n(2), n(4), h.length === 8 ? Math.round((n(6) / 255) * 100) / 100 : 1];
  }
  const fn = /^rgba?\(([^)]+)\)$/i.exec(text.trim());
  if (!fn) return null;
  const parts = fn[1].split(/[\s,/]+/).filter(Boolean).map(Number);
  if (parts.length < 3 || parts.some(Number.isNaN)) return null;
  return [parts[0], parts[1], parts[2], Math.round((parts[3] ?? 1) * 100) / 100];
}

/** `12px`, `12`, `0.75rem` (16px root) → px, else null. */
const to_px = (v: string) => {
  const m = /^(-?[\d.]+)(px|rem)?$/.exec(v);
  return m ? parseFloat(m[1]) * (m[2] === 'rem' ? 16 : 1) : null;
};
const px_tokens = (entries: Entry[], name: RegExp): Token[] =>
  entries.flatMap(([n, v]) => {
    const px = name.test(n.replace(/^(--|\$)/, '')) ? to_px(v) : null;
    return px === null ? [] : [{ name: n, value: [px] }];
  });

/** The tokens pins are checked against, and where they came from. */
export interface TokenSet {
  source: string;
  colors: Token[];
  space: Token[];
  sizes: Token[];
  radius: Token[];
}

function token_set(source: string, entries: Entry[]): TokenSet {
  return {
    source,
    colors: entries.flatMap(([name, v]) => {
      const value = parse_color(v);
      return value ? [{ name, value }] : [];
    }),
    space: px_tokens(entries, /(^|-)(space|spacing|gap|gutter)(-|$)/),
    sizes: px_tokens(entries, /(^|-)(size|font-size|text|fs)(-|$)/),
    radius: px_tokens(entries, /radius|rounded/),
  };
}

/** Sun's own design system: the fallback when a repo has no token files. */
export const MADLABS = token_set('Madlabs', declarations(colors_css + spacing_css + typography_css));

/** The repo's tokens from its CSS/SCSS/JSON token files; Madlabs when none parse. */
export function repo_tokens(files: DesignFile[]): TokenSet {
  const used = files.filter((f) => /tokens/i.test(f.path));
  const entries = used.flatMap((f) => (f.path.endsWith('.json') ? json_entries(f.text) : declarations(f.text)));
  const set = token_set(used.map((f) => f.path).join(', '), entries);
  return set.colors.length + set.space.length + set.sizes.length + set.radius.length ? set : MADLABS;
}

const RULES: { match: (property: string) => boolean; tokens: (set: TokenSet) => Token[]; kind: 'color' | 'px' }[] = [
  { match: (p) => p === 'color' || p === 'background-color', tokens: (s) => s.colors, kind: 'color' },
  { match: (p) => /^(padding|margin)-/.test(p) || p === 'gap', tokens: (s) => s.space, kind: 'px' },
  { match: (p) => p === 'font-size', tokens: (s) => s.sizes, kind: 'px' },
  { match: (p) => p === 'border-radius', tokens: (s) => s.radius, kind: 'px' },
];

const distance = (a: number[], b: number[]) => a.reduce((sum, v, i) => sum + (v - b[i]) ** 2, 0);
const nearest = (tokens: Token[], value: number[]) =>
  tokens.reduce((best, t) => (distance(t.value, value) < distance(best.value, value) ? t : best));

/** One computed value that isn't a design token, with the closest token to use instead. */
export interface OffToken {
  property: string;
  value: string;
  suggestion: string;
}

/** Values in `styles` that don't match a token of `set` (transparent, 0, auto and normal pass). */
export function off_tokens(styles: Record<string, string>, set: TokenSet = MADLABS): OffToken[] {
  const found: OffToken[] = [];
  for (const [property, value] of Object.entries(styles)) {
    const rule = RULES.find((r) => r.match(property));
    const tokens = rule ? rule.tokens(set) : [];
    if (!rule || !tokens.length) continue;
    if (rule.kind === 'color') {
      const c = parse_color(value);
      // ponytail: only rgb()/hex are checked; color()/oklch() values pass until an app uses them.
      if (!c || c[3] === 0 || tokens.some((t) => distance(t.value, c) === 0)) continue;
      found.push({ property, value, suggestion: nearest(tokens, c).name });
      continue;
    }
    for (const part of value.split(/\s+/)) {
      const px = /^(-?[\d.]+)px$/.exec(part);
      if (!px || Number(px[1]) === 0) continue;
      const n = Math.abs(Number(px[1]));
      if (tokens.some((t) => t.value[0] === n)) continue;
      const t = nearest(tokens, [n]);
      found.push({ property, value, suggestion: `${t.name} (${t.value[0]}px)` });
      break;
    }
  }
  return found;
}

// ---------- Steer message ----------

export const where = (pin: Pin) =>
  pin.target.source ? `${pin.target.source.file}${pin.target.source.line ? `:${pin.target.source.line}` : ''}` : null;

/** The suggested diff of the user's own edits, as unified-diff-like lines. */
export const diff_lines = (pin: Pin) => pin.edits.flatMap((e) => [`- ${e.property}: ${e.from}`, `+ ${e.property}: ${e.to}`]);

/** What the agent receives for a pin: the element, where it lives, the request and any suggested diff. */
export function pin_message(pin: Pin, n: number, set: TokenSet = MADLABS): string {
  const t = pin.target;
  const lines = [`Design canvas pin #${n} (${t.width} px wide, task branch):`, `Element: \`${t.selector}\` <${t.tag}>${t.text ? ` "${t.text.slice(0, 80)}"` : ''}`];
  const source = where(pin);
  if (source) lines.push(`Source: ${source}`);
  if (pin.comment) lines.push(`Request: ${pin.comment}`);
  if (pin.edits.length) lines.push('Apply this change the user made on the canvas:', '```diff', ...diff_lines(pin), '```');
  const off = off_tokens(t.styles, set);
  if (off.length) lines.push(`Not design tokens (${set.source}): ${off.map((o) => `${o.property} ${o.value} (use ${o.suggestion})`).join('; ')}`);
  return lines.join('\n');
}
