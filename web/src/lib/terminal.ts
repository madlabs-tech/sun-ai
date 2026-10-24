// One editor terminal: an xterm.js screen on the kernel's PTY socket (crates/sun-api/src/terminal.rs). Binary
// frames are terminal bytes both ways; text frames are JSON tagged on `kind`. A socket that drops without an
// `exit`, `dead` or `error` reattaches with `?pty_id=` while the kernel keeps the shell (30 s). The kernel
// replays nothing, so the screen is what this page kept.
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebLinksAddon } from '@xterm/addon-web-links';
import '@xterm/xterm/css/xterm.css';
import { api, terminal_url, type EditorAt } from './api/client';

export type TerminalState =
  | { kind: 'connecting' | 'live' | 'reconnecting' | 'dead' | 'lost' | 'revoked' }
  | { kind: 'exit'; code: number }
  | { kind: 'error'; message: string };

const GRACE_MS = 30_000;
const RETRY_MS = 2_000;

/** Opens a shell in `element`. `app_key` keys (the editor's shortcuts) go to the page, not the shell. */
export function open_terminal(element: HTMLElement, at: EditorAt, on_state: (s: TerminalState) => void, app_key: (e: KeyboardEvent) => boolean) {
  const token = (name: string) => getComputedStyle(element).getPropertyValue(name).trim();
  const term = new Terminal({
    cursorBlink: true,
    fontFamily: token('--font-mono') || 'monospace',
    fontSize: 13,
    theme: { background: token('--surface-sunken'), foreground: token('--fg-1'), cursor: token('--accent') },
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.loadAddon(new WebLinksAddon());
  term.open(element);
  fit.fit();
  // Copy and paste ride the browser's own copy and paste events, which xterm handles; so ⌘/Ctrl+C with a
  // selection and ⌘/Ctrl+V never reach the shell. Ctrl+C without a selection is still an interrupt.
  term.attachCustomKeyEventHandler((e) => {
    if (e.type !== 'keydown') return true;
    if (app_key(e)) return false;
    const key = e.key.toLowerCase();
    if (!(e.ctrlKey || e.metaKey) || e.altKey) return true;
    return !((key === 'c' && term.hasSelection()) || key === 'v');
  });

  let socket: WebSocket | null = null;
  let pty_id = '';
  let live = false;
  let done = false;
  let dropped_at = 0;
  let retry: ReturnType<typeof setTimeout> | undefined;

  const send = (data: string | Uint8Array) => live && socket?.readyState === WebSocket.OPEN && socket.send(data);
  const send_size = () => send(JSON.stringify({ kind: 'resize', cols: term.cols, rows: term.rows }));
  const encoder = new TextEncoder();
  term.onData((data) => send(encoder.encode(data)));
  term.onBinary((data) => send(Uint8Array.from(data, (c) => c.charCodeAt(0))));
  term.onResize(send_size);
  const observer = new ResizeObserver(() => fit.fit());
  observer.observe(element);

  function finish(state: TerminalState) {
    live = false;
    done = true;
    on_state(state);
    socket?.close();
  }

  function connect() {
    on_state({ kind: dropped_at ? 'reconnecting' : 'connecting' });
    const ws = new WebSocket(terminal_url(at, pty_id ? { pty_id } : { cols: String(term.cols), rows: String(term.rows) }));
    ws.binaryType = 'arraybuffer';
    ws.onmessage = (m) => {
      if (typeof m.data !== 'string') return term.write(new Uint8Array(m.data as ArrayBuffer));
      const message = JSON.parse(m.data) as { kind: string; pty_id?: string; code?: number; message?: string };
      if (message.kind === 'open') {
        pty_id = message.pty_id ?? '';
        live = true;
        dropped_at = 0;
        on_state({ kind: 'live' });
        send_size();
      } else if (message.kind === 'exit') finish({ kind: 'exit', code: message.code ?? 0 });
      else if (message.kind === 'dead') finish({ kind: 'dead' });
      else if (message.kind === 'error') finish({ kind: 'error', message: message.message ?? 'No terminal opened.' });
    };
    ws.onclose = () => ws === socket && !done && dropped();
    socket = ws;
  }

  function dropped() {
    live = false;
    if (!pty_id) return finish({ kind: 'error', message: 'The terminal couldn’t connect. Check that sun is running and try again.' });
    dropped_at ||= Date.now();
    if (Date.now() - dropped_at > GRACE_MS) return finish({ kind: 'lost' });
    on_state({ kind: 'reconnecting' });
    retry = setTimeout(async () => {
      // A refused upgrade shows the page no status: ask whether `terminal` is still held (a 403 means revoked).
      const held = await api.editor_me(at.workspace, at.repo).then((me) => me.capabilities.includes('terminal'), () => true);
      if (done) return;
      if (!held) return finish({ kind: 'revoked' });
      connect();
    }, RETRY_MS);
  }

  connect();
  return {
    focus: () => term.focus(),
    close: () => {
      done = true;
      clearTimeout(retry);
      observer.disconnect();
      socket?.close();
      term.dispose();
    },
  };
}
