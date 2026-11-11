JSON.stringify((() => {
  const text = s => (s || '').trim();
  const roleOf = el => el.getAttribute('role') || ({A:'link', BUTTON:'button', TEXTAREA:'textbox', SELECT:'combobox', INPUT:({button:'button',submit:'button',reset:'button',checkbox:'checkbox',radio:'radio'}[el.type] || 'textbox')})[el.tagName] || (el.isContentEditable ? 'textbox' : el.tagName.toLowerCase());
  // The accessible name in the order the browser computes it: the elements
  // aria-labelledby names, aria-label, a <label>, then the hint or alternative
  // text, the content (never for a field, whose content is its value), and title.
  const labelledBy = el => text((el.getAttribute('aria-labelledby') || '').split(/\s+/).map(id => id && document.getElementById(id)).filter(Boolean).map(node => node.textContent).join(' '));
  const labelOf = el => text(el.labels && el.labels.length ? [...el.labels].map(label => label.textContent).join(' ') : '');
  const holdsValue = el => ['textbox','searchbox','combobox','spinbutton','slider'].includes(roleOf(el));
  const nameOf = el => (labelledBy(el) || text(el.getAttribute('aria-label')) || labelOf(el) || text(el.getAttribute('placeholder')) || text(el.getAttribute('aria-placeholder')) || text(el.getAttribute('alt')) || (holdsValue(el) ? '' : text(el.textContent)) || (el.tagName==='INPUT' && ['button','submit','reset'].includes(el.type) ? el.value : '') || text(el.getAttribute('title')) || '').slice(0, 120);
  const candidates=[...document.querySelectorAll('button,a,input,select,textarea,[role],[onclick],[tabindex],[contenteditable]:not([contenteditable="false"])')];
  return {
    truncated:candidates.length>2000,
    title:document.title, page_focused:document.hasFocus(),
    viewport:{width:innerWidth,height:innerHeight,device_pixel_ratio:devicePixelRatio,scale:visualViewport?.scale || 1,scroll_x:scrollX,scroll_y:scrollY},
    elements:candidates.slice(0,2000).map(el => {
      const r=el.getBoundingClientRect(), style=getComputedStyle(el);
      const visible=r.width>1 && r.height>1 && r.bottom>0 && r.right>0 && r.left<innerWidth && r.top<innerHeight && style.display!=='none' && style.visibility!=='hidden' && style.opacity!=='0';
      const x=r.x+r.width/2, y=r.y+r.height/2, hit=document.elementFromPoint(x,y);
      const actionable=['BUTTON','A','INPUT','SELECT','TEXTAREA'].includes(el.tagName) || ['button','link','checkbox','radio','textbox','combobox','menuitem','slider','tab','switch'].includes(roleOf(el)) || el.hasAttribute('onclick');
      const center_hit=visible && !!hit && (hit===el || el.contains(hit));
      const enabled=!el.disabled && el.getAttribute('aria-disabled')!=='true';
      return {role:roleOf(el),name:nameOf(el),bounds:{x:r.x,y:r.y,w:r.width,h:r.height},visible,enabled,
        occluded:visible ? !center_hit : null,center_hit,clickable:actionable && visible && enabled && center_hit && document.hasFocus(),
        center_hit_target:hit ? {role:roleOf(hit),name:nameOf(hit)} : null};
    })
  };
})())
