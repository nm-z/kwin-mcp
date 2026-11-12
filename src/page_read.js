(args => {
  // browser_read: one page of a document's text, outer HTML or links. Reads the
  // DOM only: never document.cookie, storage, or the live value of any field.
  const {format, selector, offset, max} = args;
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
  const links = root => {
    const anchors = [...root.querySelectorAll('a[href],area[href]')];
    if (root.matches('a[href],area[href]')) anchors.unshift(root);
    return anchors.map(el => {
      const image = el.querySelector('img[alt]');
      const name = line(el.getAttribute('aria-label')) || line(el.innerText || el.textContent) || line(el.getAttribute('alt')) || line(image && image.alt) || line(el.getAttribute('title'));
      return [...name].slice(0, 120).join('') + '\t' + target(el);
    }).filter(row => !row.endsWith('\t')).join('\n');
  };
  const text = root => typeof root.innerText === 'string' ? root.innerText : (root.textContent || '');
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
  return JSON.stringify({url:location.href, title:document.title, matched:roots.length, total:body.length, start, end, content:body.slice(start, end)});
})
