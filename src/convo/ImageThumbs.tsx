/**
 * The images a conversation block carries: a message's attachments, a tool
 * result's screenshot. The block names each image (`ImageRef`); its bytes stay
 * in the core and are read once per view with `content_image`, then shown as
 * a `data:` URL (the only image source the CSP allows). A thumbnail opens to
 * its full width in place; an image the core no longer holds says so.
 */

import { useEffect, useState } from 'react';

import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import type { ImageId } from '../lib/gen/ImageId';
import type { ImageRef } from '../lib/gen/ImageRef';

/** Data URLs read so far (null = the core no longer holds it), newest last. */
const cache = new Map<ImageId, Promise<string | null>>();
const CACHE_MAX = 64;

function dataUrlOf(id: ImageId): Promise<string | null> {
  const hit = cache.get(id);
  if (hit) return hit;
  const read = commands.content_image({ imageId: id }).then(
    (res) => `data:${res.mediaType};base64,${res.data}`,
    () => null,
  );
  cache.set(id, read);
  while (cache.size > CACHE_MAX) {
    const oldest = cache.keys().next().value;
    if (oldest === undefined) break;
    cache.delete(oldest);
  }
  return read;
}

/** Forget every read image (tests). */
export function resetImageCacheForTests() {
  cache.clear();
}

function Thumb({ image, index }: { image: ImageRef; index: number }) {
  const { t } = useI18n();
  const [url, setUrl] = useState<string | null | undefined>(undefined);
  const [open, setOpen] = useState(false);
  useEffect(() => {
    let live = true;
    void dataUrlOf(image.id).then((u) => {
      if (live) setUrl(u);
    });
    return () => {
      live = false;
    };
  }, [image.id]);
  const label = t('convo.image', { index: index + 1 });
  if (url === null) return <span className="g-thumb g-thumb-gone">{t('convo.imageGone')}</span>;
  return (
    <button
      type="button"
      className="g-thumb"
      data-open={open ? 'true' : undefined}
      aria-expanded={open}
      aria-label={label}
      disabled={url === undefined}
      onClick={() => setOpen((o) => !o)}
    >
      {url ? <img src={url} alt={label} /> : null}
    </button>
  );
}

export function ImageThumbs({ images }: { images: readonly ImageRef[] }) {
  if (images.length === 0) return null;
  return (
    <div className="g-thumbs">
      {images.map((image, i) => (
        <Thumb key={image.id} image={image} index={i} />
      ))}
    </div>
  );
}
