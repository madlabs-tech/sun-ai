// Design-canvas bridge (pins). The kernel appends this script to every HTML page of a preview.
// The preview runs sandboxed in an opaque origin, so the canvas can't reach into it: it talks to
// this script over postMessage. Contract: crates/sun-api/src/preview.rs.
//   canvas -> bridge: {sun:'pick', on}  {sun:'apply', selector, property, value}  {sun:'revert'}
//   bridge -> canvas: {sun:'ready'}  {sun:'picked', target}  {sun:'scroll', x, y}
// Only the parent window at the kernel's own origin is obeyed; replies go to that origin only.
(() => {
  if (window.parent === window || window.__sun_bridge) return;
  window.__sun_bridge = true;
  const kernel_origin = new URL(location.href).origin;
  const STYLE_KEYS = [
    'color', 'background-color', 'font-size', 'font-weight', 'line-height', 'border-radius', 'gap',
    'padding-top', 'padding-right', 'padding-bottom', 'padding-left',
    'margin-top', 'margin-right', 'margin-bottom', 'margin-left',
  ];
  const post = (message) => window.parent.postMessage(message, kernel_origin);
  const clip = (text, max) => String(text).slice(0, max);

  function selector_of(el) {
    const parts = [];
    for (let node = el; node && node !== document.documentElement; node = node.parentElement) {
      if (node.id && document.querySelectorAll('#' + CSS.escape(node.id)).length === 1) {
        parts.unshift('#' + CSS.escape(node.id));
        break;
      }
      const test_id = node.getAttribute('data-testid');
      const by_test_id = test_id && `[data-testid="${CSS.escape(test_id)}"]`;
      if (by_test_id && document.querySelectorAll(by_test_id).length === 1) {
        parts.unshift(by_test_id);
        break;
      }
      let part = node.tagName.toLowerCase();
      const same = node.parentElement ? [...node.parentElement.children].filter((c) => c.tagName === node.tagName) : [];
      if (same.length > 1) part += `:nth-of-type(${same.indexOf(node) + 1})`;
      parts.unshift(part);
    }
    return parts.join(' > ');
  }

  // Where the framework says the element was written: data attributes, then dev-build metadata
  // (Svelte `__svelte_meta`, React <19 `_debugSource`, Vue `__file`). Nearest ancestor wins.
  function source_of(el) {
    for (let node = el, depth = 0; node && depth < 30; node = node.parentElement, depth++) {
      const attr = node.getAttribute('data-sun-source') || node.getAttribute('data-source');
      const m = attr && /^(.+?):(\d+)(?::(\d+))?$/.exec(attr);
      if (m) return { file: m[1], line: +m[2], column: +(m[3] || 0) };
      const loc = node.__svelte_meta?.loc;
      if (loc?.file) return { file: String(loc.file), line: +loc.line || 0, column: +loc.column || 0 };
      const fiber_key = Object.keys(node).find((k) => k.startsWith('__reactFiber$'));
      for (let f = fiber_key && node[fiber_key]; f; f = f._debugOwner || f.return) {
        const s = f._debugSource;
        if (s?.fileName) return { file: String(s.fileName), line: +s.lineNumber || 0, column: +s.columnNumber || 0 };
      }
      const vue_file = node.__vueParentComponent?.type?.__file;
      if (vue_file) return { file: String(vue_file), line: 0, column: 0 };
    }
    return null;
  }

  function target_of(el) {
    const r = el.getBoundingClientRect();
    const computed = getComputedStyle(el);
    const styles = {};
    for (const key of STYLE_KEYS) styles[key] = clip(computed.getPropertyValue(key), 200);
    return {
      selector: clip(selector_of(el), 1000),
      tag: el.tagName.toLowerCase(),
      text: el.children.length ? '' : clip((el.textContent || '').trim(), 500),
      width: document.documentElement.clientWidth || innerWidth,
      rect: { x: r.x + scrollX, y: r.y + scrollY, width: r.width, height: r.height },
      styles,
      source: source_of(el),
      // The page under the preview base, so the kernel screenshots the same page (crops).
      path: clip((location.pathname.replace(/^\/preview\/[^/]+/, '') || '/') + location.search, 1000),
    };
  }

  // Pick mode: outline under the pointer, the next click pins instead of acting.
  let picking = false;
  const outline = document.createElement('div');
  outline.style.cssText =
    'position:fixed;pointer-events:none;z-index:2147483647;border:2px solid #3466FF;border-radius:4px;' +
    'background:rgba(52,102,255,.12);display:none';
  function show(el) {
    const r = el.getBoundingClientRect();
    Object.assign(outline.style, { display: 'block', left: r.x + 'px', top: r.y + 'px', width: r.width + 'px', height: r.height + 'px' });
  }
  function set_picking(on) {
    picking = on;
    if (!on) outline.style.display = 'none';
    else if (!outline.isConnected) document.documentElement.append(outline);
    document.documentElement.style.cursor = on ? 'crosshair' : '';
  }
  addEventListener('mousemove', (e) => picking && e.target instanceof Element && show(e.target), true);
  addEventListener('click', (e) => {
    if (!picking || !(e.target instanceof Element)) return;
    e.preventDefault();
    e.stopPropagation();
    set_picking(false);
    post({ sun: 'picked', target: target_of(e.target) });
  }, true);
  addEventListener('keydown', (e) => picking && e.key === 'Escape' && (set_picking(false), post({ sun: 'pick_cancelled' })), true);

  // "Change it myself": live values, undone by `revert` (or a reload).
  const originals = [];
  function apply(selector, property, value) {
    const el = document.querySelector(selector);
    if (!el) return;
    if (property === 'text') {
      if (el.children.length) return;
      const before = el.textContent;
      originals.push(() => (el.textContent = before));
      el.textContent = value;
    } else {
      const before = el.style.getPropertyValue(property);
      originals.push(() => el.style.setProperty(property, before));
      el.style.setProperty(property, value);
    }
  }

  addEventListener('message', (e) => {
    const m = e.data;
    if (e.source !== window.parent || e.origin !== kernel_origin || !m || typeof m.sun !== 'string') return;
    if (m.sun === 'pick' && typeof m.on === 'boolean') set_picking(m.on);
    else if (m.sun === 'apply' && [m.selector, m.property, m.value].every((v) => typeof v === 'string')
      && (m.property === 'text' || /^[a-z-]{1,64}$/.test(m.property))) apply(m.selector, m.property, m.value);
    else if (m.sun === 'revert') while (originals.length) originals.pop()();
  });

  let queued = false;
  addEventListener('scroll', () => {
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => ((queued = false), post({ sun: 'scroll', x: scrollX, y: scrollY })));
  }, { passive: true });
  post({ sun: 'ready' });
})();
