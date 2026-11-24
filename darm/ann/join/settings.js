// Connection settings shared by the join page and the phone app (same
// origin). Defaults come from the site's config.json, which the Pages
// workflow fills from the DARM_MONITOR_NODES variable: that list is the
// pivot every new device starts from. A user can override it in Settings.
//
// Tokens are kept for this tab only unless the user asks to remember them on
// this device. They are secrets for the user's own archive, so nothing here
// ever puts them in a URL or a generated file.

const KEY = 'darm.settings.v1';

export const emptySettings = () => ({ archive: '', readToken: '', enrollToken: '', remember: false });

export function loadSettings(local, session) {
  for (const store of [local, session]) {
    try {
      const raw = store?.getItem(KEY);
      if (raw) return { ...emptySettings(), ...JSON.parse(raw) };
    } catch {
      /* storage blocked or corrupt: fall through to defaults */
    }
  }
  return emptySettings();
}

export function saveSettings(settings, local, session) {
  const value = JSON.stringify({ ...emptySettings(), ...settings });
  try {
    local?.removeItem(KEY);
    session?.removeItem(KEY);
    (settings.remember ? local : session)?.setItem(KEY, value);
  } catch {
    /* storage blocked: settings last for this page view only */
  }
}

export function clearSettings(local, session) {
  try {
    local?.removeItem(KEY);
    session?.removeItem(KEY);
  } catch {
    /* nothing to clear */
  }
}

/** Archives to try, in order: the user's own first, then the site's pivot list. */
export function candidateArchives(settings, config, onChain = []) {
  const list = [];
  if (settings.archive) list.push(settings.archive);
  for (const url of config?.nodes ?? []) if (!list.includes(url)) list.push(url);
  if (config?.archive && !list.includes(config.archive)) list.push(config.archive);
  for (const entry of onChain) if (!list.some((c) => (typeof c === 'string' ? c : c.url) === entry.url)) list.push(entry);
  return list;
}
