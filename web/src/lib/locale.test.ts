/**
 * Locale resolution, and the one property that keeps it from being a bug.
 *
 * `reconcile` reloads the document when the account's stored choice disagrees
 * with what is on screen. A reload triggered by a comparison is a loop unless
 * something makes the comparison false next time round — so that "something"
 * is `needsReload`, split out precisely so it can be asserted here without a
 * browser.
 */

import { describe, expect, it } from 'vitest';

import {
  LOCALE_NAMES,
  SUPPORTED,
  detect,
  isSupported,
  needsReload,
  readCached,
  writeCached,
  type Locale,
  type LocaleStore
} from './locale';

/** A `localStorage` that works. */
function fakeStore(initial: Record<string, string> = {}): LocaleStore {
  const map = new Map(Object.entries(initial));
  return {
    getItem: (k) => map.get(k) ?? null,
    setItem: (k, v) => void map.set(k, v),
    removeItem: (k) => void map.delete(k)
  };
}

/** A browser configured to block site data: touching it throws. */
const hostileStore: LocaleStore = {
  getItem() {
    throw new DOMException('denied');
  },
  setItem() {
    throw new DOMException('denied');
  },
  removeItem() {
    throw new DOMException('denied');
  }
};

describe('detect', () => {
  it('takes the first supported language the browser lists', () => {
    expect(detect(['de', 'en'])).toBe('de');
    expect(detect(['ja', 'ko', 'it'])).toBe('it');
  });

  it('matches a region-qualified tag to its language', () => {
    // A browser sending es-419 means Spanish. Handing it English because of the
    // region would be the opposite of what it asked for — and it is the same
    // rule tower_core::i18n::Locale::from_str applies on the server.
    expect(detect(['es-419'])).toBe('es');
    expect(detect(['de-CH', 'de'])).toBe('de');
    expect(detect(['hi_IN'])).toBe('hi');
    expect(detect(['PT-br', 'FR-ca'])).toBe('fr');
  });

  it('falls back to the base locale when nothing matches', () => {
    expect(detect(['ja', 'ko'])).toBe('en');
    expect(detect([])).toBe('en');
  });
});

describe('needsReload', () => {
  it('is false when the account has chosen nothing and nothing is cached', () => {
    // null is "never chose", which leaves browser detection in charge. Reading
    // it as a request to switch to English would pin every account that never
    // touched the picker to the base locale.
    expect(needsReload(null, 'de', undefined)).toBe(false);
  });

  it('is false while the server has not answered yet, even with a stale cache', () => {
    // `undefined` is what `session.me?.user.locale` reads before `/api/me`
    // resolves and when signed out — no information, not the account clearing
    // its choice. Treating it like `null` would reload on every boot with a
    // cached locale, before the server ever got a chance to say otherwise.
    expect(needsReload(undefined, 'de', undefined)).toBe(false);
    expect(needsReload(undefined, 'de', 'de')).toBe(false);
  });

  it('is true when the account cleared its choice but a stale cache remains', () => {
    // Server says "nothing chosen", but this boot is rendering `de` because a
    // choice made on another device — and since cleared — is still sitting in
    // the cache. Without this, clearing a locale server-side would never
    // actually take effect in a browser that had cached the old one.
    expect(needsReload(null, 'de', 'de')).toBe(true);
  });

  it('is false when the stored choice is already on screen', () => {
    expect(needsReload('de', 'de')).toBe(false);
  });

  it('is true when they disagree', () => {
    expect(needsReload('de', 'en')).toBe(true);
  });

  it('ignores a stored value that is not a locale we ship', () => {
    // A locale that was removed from the product, or a corrupted row. Falling
    // back beats reloading forever into a language that no longer exists.
    expect(needsReload('klingon', 'en')).toBe(false);
    expect(needsReload('', 'en')).toBe(false);
  });

  /**
   * The termination argument, asserted rather than asserted-in-a-comment.
   *
   * `reconcile` writes the stored value to the cache *before* reloading, so the
   * next boot resolves to exactly that value. Whatever the server said, one
   * round of that must leave nothing to do.
   */
  it('cannot ask for a second reload after the first', () => {
    for (const stored of SUPPORTED) {
      for (const rendering of SUPPORTED) {
        if (!needsReload(stored, rendering)) continue;

        const store = fakeStore();
        writeCached(stored, store);
        const nextBoot = readCached(store) ?? detect([]);

        expect(nextBoot).toBe(stored);
        expect(needsReload(stored, nextBoot)).toBe(false);
      }
    }
  });
});

describe('the cache', () => {
  it('round-trips a supported locale', () => {
    const store = fakeStore();
    writeCached('it', store);
    expect(readCached(store)).toBe('it');
  });

  it('clears on undefined, which is "match my browser"', () => {
    const store = fakeStore({ 'of.locale': 'it' });
    writeCached(undefined, store);
    expect(readCached(store)).toBeUndefined();
  });

  it('ignores a stored value that is not a locale we ship', () => {
    expect(readCached(fakeStore({ 'of.locale': 'klingon' }))).toBeUndefined();
  });

  /**
   * A browser set to block site data throws on the accessor itself. The console
   * has to degrade to detecting the language on every load, not to a blank page.
   */
  it('survives a storage that throws on every operation', () => {
    expect(() => writeCached('de', hostileStore)).not.toThrow();
    expect(readCached(hostileStore)).toBeUndefined();
  });

  it('survives having no storage at all', () => {
    expect(() => writeCached('de', undefined)).not.toThrow();
    expect(readCached(undefined)).toBeUndefined();
  });
});

describe('the shipped locale list', () => {
  it('is the six #42 asks for', () => {
    expect([...SUPPORTED].sort()).toEqual(['de', 'en', 'es', 'fr', 'hi', 'it']);
  });

  it('accepts exactly those and nothing else', () => {
    for (const locale of SUPPORTED) expect(isSupported(locale)).toBe(true);
    for (const other of ['klingon', 'pt', '', 'EN', null, undefined]) {
      expect(isSupported(other)).toBe(false);
    }
  });

  /**
   * Somebody who cannot read the current language has to be able to find their
   * own in the picker, so every name is written in the language it names.
   */
  it('names every locale in its own language', () => {
    for (const locale of SUPPORTED) {
      expect(LOCALE_NAMES[locale as Locale]).toBeTruthy();
    }
    expect(LOCALE_NAMES.de).toBe('Deutsch');
    expect(LOCALE_NAMES.hi).toBe('हिन्दी');
  });
});
