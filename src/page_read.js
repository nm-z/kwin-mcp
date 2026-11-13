(args => {
  // browser_read: one page of a document's text, outer HTML or links. Reads the
  // DOM only: never document.cookie, storage, or the live value of any field.
  const {format, selector, offset, max, bare, dedupe} = args;
  let roots = [document.documentElement];
  if (selector) {
    let all;
    try { all = document.querySelectorAll(selector); } catch (error) { return JSON.stringify({problem:'invalid_selector', detail:String(error && error.message || error)}); }
    // Matches come in document order; one inside an earlier match is already in it.
    roots = [];
    for (const el of all) if (!roots.length || !roots[roots.length - 1].contains(el)) roots.push(el);
  }
  // A password field, also one a "show password" switch turned into a text field.
  const secret = el => el.localName === 'input' && (el.type === 'password' || /passw|pwd/i.test([el.getAttribute('autocomplete'), el.name, el.id].join(' ')));
  // Style sheets and what scripts hold are left out: code, and data blocks, where a
  // page keeps its state and tokens. Only ld+json, metadata published for crawlers, stays.
  const kept = el => el.localName === 'style' || (el.localName === 'script' && !/^application\/ld\+json$/i.test((el.getAttribute('type') || '').trim()));
  // A <template> keeps its markup in a fragment that selectors do not reach.
  const marked = 'input,script,style,template';
  const scrub = els => {
    for (const el of els) {
      if (el.localName === 'template') scrub(el.content.querySelectorAll(marked));
      else if (secret(el)) el.removeAttribute('value');
      else if (kept(el)) el.textContent = '';
    }
  };
  const html = root => {
    const copy = root.cloneNode(true);
    scrub([copy, ...copy.querySelectorAll(marked)]);
    return copy.outerHTML;
  };
  const line = text => (text || '').replace(/\s+/g, ' ').trim();
  const target = el => {
    const href = el.href;
    if (typeof href === 'string') return href;
    try { return href && href.baseVal ? new URL(href.baseVal, el.baseURI).href : ''; } catch (error) { return ''; }
  };
  // strip_query: the address without its query and fragment, where tracking lives.
  const strip = href => {
    try { const url = new URL(href); url.search = ''; url.hash = ''; return url.href; } catch (error) { return href.replace(/[?#][^]*$/, ''); }
  };
  const links = root => {
    const anchors = [...root.querySelectorAll('a[href],area[href]')];
    if (root.matches('a[href],area[href]')) anchors.unshift(root);
    let rows = anchors.map(el => {
      const image = el.querySelector('img[alt]');
      const name = line(el.getAttribute('aria-label')) || line(el.innerText || el.textContent) || line(el.getAttribute('alt')) || line(image && image.alt) || line(el.getAttribute('title'));
      const href = target(el);
      return [[...name].slice(0, 120).join(''), bare && href ? strip(href) : href];
    }).filter(([, href]) => href);
    // dedupe: one row per address, in the order addresses first appear, named by
    // the first of its links that has a name (a card often links an image first).
    if (dedupe) {
      const named = new Map();
      for (const [name, href] of rows) if (!named.get(href)) named.set(href, name);
      rows = [...named].map(([href, name]) => [name, href]);
    }
    return rows.map(([name, href]) => name + '\t' + href).join('\n');
  };
  // The DOM's own text, blocks on lines of their own: what a page holds before
  // Chrome renders it. A results page that reveals its results after load had
  // them in the DOM while innerText still had its header and footer (#978).
  const skipped = new Set(['script', 'style', 'noscript', 'template', 'head', 'svg', 'math', 'iframe', 'object', 'embed', 'canvas', 'select', 'input', 'textarea']);
  const blocks = new Set(['address', 'article', 'aside', 'blockquote', 'br', 'caption', 'dd', 'details', 'dialog', 'div', 'dl', 'dt', 'fieldset', 'figcaption', 'figure', 'footer', 'form', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'header', 'hgroup', 'hr', 'legend', 'li', 'main', 'nav', 'ol', 'p', 'pre', 'section', 'summary', 'table', 'tbody', 'tfoot', 'thead', 'tr', 'ul']);
  const domText = root => {
    const parts = [];
    const walk = node => {
      if (node.nodeType === Node.TEXT_NODE) { parts.push(node.data.replace(/\s+/g, ' ')); return; }
      if (node.nodeType !== Node.ELEMENT_NODE || skipped.has(node.localName)) return;
      const block = blocks.has(node.localName);
      if (block) parts.push('\n');
      else if (node.localName === 'td' || node.localName === 'th') parts.push('\t');
      for (const child of node.childNodes) walk(child);
      if (block) parts.push('\n');
    };
    walk(root);
    return parts.join('').replace(/ *\n\s*/g, '\n').trim();
  };
  // innerText when Chrome rendered the page; the DOM's text when it rendered
  // under a quarter of it, which the result reports.
  let renderedChars = 0, fromDom = false;
  const text = root => {
    const rendered = typeof root.innerText === 'string' ? root.innerText : (root.textContent || '');
    renderedChars += rendered.length;
    const dom = domText(root);
    if (rendered.trim().length * 4 < dom.length && dom.length - rendered.length > 400) {
      fromDom = true;
      return dom;
    }
    return rendered;
  };
  const read = {html, links, text}[format];
  // A lone surrogate has no JSON form the server accepts; it becomes U+FFFD.
  const whole = roots.map(read).join('\n');
  const body = whole.toWellFormed ? whole.toWellFormed() : whole;
  // Offsets count UTF-16 units, as the page does; a cut never splits a pair.
  const low = at => at > 0 && at < body.length && (body.charCodeAt(at) & 0xFC00) === 0xDC00;
  let start = Math.min(offset, body.length);
  if (low(start)) start += 1;
  let end = Math.min(start + max, body.length);
  if (low(end)) end = end - 1 > start ? end - 1 : end + 1;
  return JSON.stringify({url:location.href, title:document.title, matched:roots.length, total:body.length, start, end, content:body.slice(start, end), from_dom:fromDom, rendered_chars:renderedChars});
})
