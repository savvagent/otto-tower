// @vitest-environment jsdom
/**
 * The public front page, as a signed-out visitor sees it.
 *
 * The `claude mcp add` example names the server, and that name is what every
 * tool is called under (`mcp__<name>__*`). It must be `savvagent-tower`, the name
 * the docs, the client skills and the server's own instructions use; a page
 * that teaches `factory` gives a reader tools no skill refers to.
 */

import { mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('$app/navigation', () => ({
  goto: vi.fn(() => Promise.resolve())
}));

import { page } from '$app/state';
import { session } from '$lib/session.svelte';
import Page from './+page.svelte';

let container: HTMLElement;
let urlDescriptor: PropertyDescriptor | undefined;

beforeEach(() => {
  container = document.createElement('div');
  document.body.appendChild(container);
  urlDescriptor = Object.getOwnPropertyDescriptor(page, 'url');
  Object.defineProperty(page, 'url', {
    configurable: true,
    get: () => new URL('http://localhost/')
  });
  session.info = undefined;
  session.ready = true;
});

afterEach(() => {
  container.remove();
  if (urlDescriptor) Object.defineProperty(page, 'url', urlDescriptor);
  session.ready = false;
});

describe('the front page', () => {
  it('names the MCP server savvagent-tower in its claude mcp add example', () => {
    const instance = mount(Page, { target: container });
    const command = [...container.querySelectorAll('code')]
      .map((code) => code.textContent ?? '')
      .find((text) => text.includes('claude mcp add'));
    expect(command).toBeDefined();
    expect(command).toMatch(/^claude mcp add --transport http savvagent-tower https:\/\/\S+\/mcp$/);
    expect(command).not.toMatch(/\bfactory\b/);
    unmount(instance);
  });
});
