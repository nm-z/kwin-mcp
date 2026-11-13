(() => {
  // browser_eval runs in a world of its own, which sees the page's DOM but not
  // its scripts. In it what browser_read never returns reads empty too:
  // cookies, storage, and the value of a password field (#959).
  const hide = (target, name, value) => {
    try { Object.defineProperty(target, name, {get: () => value, set() {}, configurable: false}); } catch (error) {}
  };
  hide(Document.prototype, 'cookie', '');
  for (const name of ['localStorage', 'sessionStorage', 'indexedDB', 'caches', 'cookieStore']) {
    hide(globalThis, name, undefined);
    hide(Window.prototype, name, undefined);
  }
  // A password field, also one a "show password" switch turned into a text field.
  const secret = el => el.type === 'password' || /passw|pwd/i.test([el.getAttribute('autocomplete'), el.name, el.id].join(' '));
  const value = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value');
  try {
    Object.defineProperty(HTMLInputElement.prototype, 'value', {
      get() { return secret(this) ? '' : value.get.call(this); },
      set(text) { value.set.call(this, text); },
      configurable: false,
    });
  } catch (error) {}
  return true;
})()
