/**
 * Images attached in the composer (contract §4.2.3 `turn_send.images`):
 * pasted, dropped or picked. Each is read here into standard base64; one that
 * is too large or of a type the agent does not take is redrawn on a canvas,
 * its long edge at most `MAX_EDGE`, as JPEG (PNG when it has to stay
 * lossless-looking and still fits). Conversation sessions only.
 */

import type { OutgoingImage } from '../lib/gen/OutgoingImage';

/** The types `turn_send` takes (the core's `IMAGE_MEDIA_TYPES`). */
export const SEND_TYPES: readonly string[] = ['image/png', 'image/jpeg', 'image/gif', 'image/webp'];
/** Images one message may carry (`MAX_SEND_IMAGES`). */
export const MAX_IMAGES = 5;
/** One image's base64 ≤ 5 MiB (`MAX_SEND_IMAGE_BASE64`). */
export const MAX_BASE64 = 5 * 1024 * 1024;
/** Files up to this size of a sent type go as they are. */
const AS_IS_BYTES = 1536 * 1024;
/** The long edge a redrawn image keeps. */
const MAX_EDGE = 2000;

export interface Attachment extends OutgoingImage {
  /** Local key for the list. */
  key: string;
  /** For the thumbnail. */
  url: string;
}

let next = 0;

/** Standard base64 of a blob's bytes. */
async function base64Of(blob: Blob): Promise<string> {
  const bytes = new Uint8Array(await blob.arrayBuffer());
  let binary = '';
  for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(binary);
}

function fromDataUrl(url: string): { mediaType: string; data: string } | null {
  const m = /^data:([^;,]+);base64,(.*)$/s.exec(url);
  return m ? { mediaType: m[1]!.toLowerCase(), data: m[2]! } : null;
}

async function redraw(blob: Blob): Promise<{ mediaType: string; data: string } | null> {
  if (typeof createImageBitmap !== 'function' || typeof document === 'undefined') return null;
  const bitmap = await createImageBitmap(blob);
  try {
    for (const edge of [MAX_EDGE, 1600, 1200]) {
      const scale = Math.min(1, edge / Math.max(bitmap.width, bitmap.height));
      const canvas = document.createElement('canvas');
      canvas.width = Math.max(1, Math.round(bitmap.width * scale));
      canvas.height = Math.max(1, Math.round(bitmap.height * scale));
      const g = canvas.getContext('2d');
      if (!g) return null;
      g.drawImage(bitmap, 0, 0, canvas.width, canvas.height);
      for (const type of blob.type === 'image/png' ? ['image/png', 'image/jpeg'] : ['image/jpeg']) {
        const parsed = fromDataUrl(canvas.toDataURL(type, 0.9));
        if (parsed && parsed.mediaType === type && parsed.data.length <= MAX_BASE64) return parsed;
      }
    }
    return null;
  } finally {
    bitmap.close();
  }
}

/** One file as an attachment, or null when it is not an image that can be sent. */
export async function attachmentOf(file: Blob): Promise<Attachment | null> {
  if (!file.type.startsWith('image/')) return null;
  let image: { mediaType: string; data: string } | null = null;
  if (SEND_TYPES.includes(file.type) && file.size <= AS_IS_BYTES) {
    image = { mediaType: file.type, data: await base64Of(file) };
  }
  if (!image || !SEND_TYPES.includes(image.mediaType) || image.data.length > MAX_BASE64) {
    image = await redraw(file).catch(() => null);
  }
  if (!image) return null;
  return { ...image, key: `a${++next}`, url: `data:${image.mediaType};base64,${image.data}` };
}

/** The image files of a paste or a drop. */
export function imageFiles(list: DataTransfer | null): File[] {
  if (!list) return [];
  const files: File[] = [];
  for (const item of Array.from(list.items ?? [])) {
    if (item.kind === 'file' && item.type.startsWith('image/')) {
      const f = item.getAsFile();
      if (f) files.push(f);
    }
  }
  if (files.length === 0) for (const f of Array.from(list.files ?? [])) if (f.type.startsWith('image/')) files.push(f);
  return files;
}
