/**
 * Channels for the three streaming commands (`store_subscribe`,
 * `convo_subscribe`, `pty_attach`). Against Tauri this is the real `Channel`;
 * in showcase mode there is no runtime to register a callback with, so a plain
 * object with the same `onmessage` is handed to the showcase backend.
 */

import { Channel } from '@tauri-apps/api/core';

import { showcaseMode } from './mode';

/** What a backend needs from a channel: somewhere to deliver messages. */
export interface ChannelLike<T> {
  onmessage: (message: T) => void;
}

class LocalChannel<T> implements ChannelLike<T> {
  readonly id = -1;
  constructor(public onmessage: (message: T) => void) {}
}

export function createChannel<T>(onmessage: (message: T) => void): Channel<T> {
  if (showcaseMode()) return new LocalChannel<T>(onmessage) as unknown as Channel<T>;
  return new Channel<T>(onmessage);
}

/** Stop delivering to `channel` (the old subscription after a resubscribe). */
export function disposeChannel<T>(channel: Channel<T> | ChannelLike<T>): void {
  channel.onmessage = () => {};
  const cleanup = (channel as { cleanupCallback?: () => void }).cleanupCallback;
  if (typeof cleanup === 'function' && !(channel instanceof LocalChannel)) {
    try {
      cleanup.call(channel);
    } catch {
      // The runtime is gone (window closing): nothing left to unregister.
    }
  }
}
