// CodeMirror 6 for the repo editor. A file's language comes from its extension, each pack loaded on first
// use. Theme and highlighting use Madlabs tokens only, so the editor follows the app's light or dark theme.
import { basicSetup } from 'codemirror';
import { EditorState, type Extension, type Text } from '@codemirror/state';
import { EditorView } from '@codemirror/view';
import { HighlightStyle, StreamLanguage, syntaxHighlighting } from '@codemirror/language';
import { tags } from '@lezer/highlight';

const js = (typescript: boolean) => () => import('@codemirror/lang-javascript').then((m) => m.javascript({ jsx: true, typescript }));
const html = () => import('@codemirror/lang-html').then((m) => m.html());
const shell = () => import('@codemirror/legacy-modes/mode/shell').then((m) => StreamLanguage.define(m.shell));
const yaml = () => import('@codemirror/lang-yaml').then((m) => m.yaml());
const markdown = () => import('@codemirror/lang-markdown').then((m) => m.markdown());

const BY_EXTENSION: Record<string, () => Promise<Extension>> = {
  ts: js(true), tsx: js(true), mts: js(true), cts: js(true),
  js: js(false), jsx: js(false), mjs: js(false), cjs: js(false),
  rs: () => import('@codemirror/lang-rust').then((m) => m.rust()),
  go: () => import('@codemirror/lang-go').then((m) => m.go()),
  py: () => import('@codemirror/lang-python').then((m) => m.python()),
  json: () => import('@codemirror/lang-json').then((m) => m.json()),
  yaml, yml: yaml,
  md: markdown, markdown,
  html, htm: html, svelte: html, vue: html,
  css: () => import('@codemirror/lang-css').then((m) => m.css()),
  sh: shell, bash: shell, zsh: shell,
};

/** The language of `path` by its extension; plain text for any other. */
export function language_of(path: string): Promise<Extension> {
  const name = path.slice(path.lastIndexOf('/') + 1).toLowerCase();
  return BY_EXTENSION[name.slice(name.lastIndexOf('.') + 1)]?.() ?? Promise.resolve([]);
}

const theme = EditorView.theme({
  '&': { height: '100%', color: 'var(--fg-1)', backgroundColor: 'var(--surface-sunken)', fontSize: '13px' },
  '&.cm-focused': { outline: 'none' },
  '.cm-scroller': { fontFamily: 'var(--font-mono)', lineHeight: '1.6' },
  '.cm-content': { caretColor: 'var(--fg-1)' },
  '.cm-cursor, .cm-dropCursor': { borderLeftColor: 'var(--fg-1)' },
  '.cm-gutters': { backgroundColor: 'var(--surface-sunken)', color: 'var(--fg-2)', border: 'none', borderRight: '1px solid var(--border-subtle)' },
  '.cm-activeLine, .cm-activeLineGutter': { backgroundColor: 'var(--bg-subtle)' },
  '&.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground, .cm-selectionBackground, .cm-content ::selection': {
    backgroundColor: 'var(--accent-soft-2)',
  },
  '.cm-matchingBracket': { backgroundColor: 'var(--accent-soft)', outline: '1px solid var(--border-strong)' },
  '.cm-searchMatch': { backgroundColor: 'var(--warning-soft)' },
  '.cm-panels, .cm-tooltip': { backgroundColor: 'var(--surface-card)', color: 'var(--fg-1)', border: '1px solid var(--border-default)' },
  '.cm-foldPlaceholder': { backgroundColor: 'var(--bg-subtle)', color: 'var(--fg-2)', border: 'none' },
});

const highlight = HighlightStyle.define([
  { tag: [tags.keyword, tags.modifier, tags.operatorKeyword, tags.controlKeyword, tags.definitionKeyword], color: 'var(--fg-accent)' },
  { tag: [tags.string, tags.special(tags.string), tags.regexp, tags.character], color: 'var(--success-fg)' },
  { tag: [tags.number, tags.bool, tags.null, tags.atom], color: 'var(--warning-fg)' },
  { tag: [tags.typeName, tags.className, tags.tagName, tags.namespace], color: 'var(--info-fg)' },
  { tag: [tags.function(tags.variableName), tags.function(tags.propertyName), tags.attributeName], color: 'var(--fg-link)' },
  { tag: tags.comment, color: 'var(--fg-2)', fontStyle: 'italic' },
  { tag: tags.heading, color: 'var(--fg-accent)', fontWeight: 'bold' },
  { tag: tags.link, textDecoration: 'underline' },
  { tag: tags.emphasis, fontStyle: 'italic' },
  { tag: tags.strong, fontWeight: 'bold' },
  { tag: tags.invalid, color: 'var(--danger-fg)' },
]);

/** A file's editor state; `on_change` gets the document after every edit. */
export function editor_state(content: string, language: Extension, readonly: boolean, on_change: (doc: Text) => void): EditorState {
  return EditorState.create({
    doc: content,
    extensions: [
      basicSetup,
      theme,
      syntaxHighlighting(highlight),
      language,
      EditorState.readOnly.of(readonly),
      EditorView.updateListener.of((update) => update.docChanged && on_change(update.state.doc)),
    ],
  });
}
