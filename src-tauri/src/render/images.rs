//! The images conversation blocks show (§3.7: memory only, never persisted or
//! logged). A block carries an [`ImageRef`]; the base64 bytes stay in this
//! store and the WebView reads them with `content_image`.
//!
//! - Only raster types ([`crate::contract::content::IMAGE_MEDIA_TYPES`]): an
//!   SVG could carry script.
//! - The data must be standard base64 and at most [`IMAGE_MAX_BASE64`]; anything
//!   else is not stored and the block shows no image for it.
//! - The store is bounded ([`STORE_MAX_BYTES`]): the least recently stored
//!   image goes first; a block whose image left reads `NOT_FOUND`.
//! - The same image (media type and bytes) is one entry ([`ImageId::derive`]).

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::Value;

use crate::contract::content::{image_media_type, is_base64, ImageRef};
use crate::contract::ids::ImageId;

/// One image's base64 ≤ 16 MiB.
pub const IMAGE_MAX_BASE64: usize = 16 * 1024 * 1024;
/// Everything the store holds ≤ 256 MiB of base64.
pub const STORE_MAX_BYTES: usize = 256 * 1024 * 1024;

struct Stored {
    media_type: &'static str,
    data: Arc<str>,
}

#[derive(Default)]
struct Inner {
    map: HashMap<ImageId, Stored>,
    order: VecDeque<ImageId>,
    bytes: usize,
}

/// See the module docs.
pub struct ImageStore {
    inner: Mutex<Inner>,
    max_bytes: usize,
}

impl ImageStore {
    pub fn new(max_bytes: usize) -> ImageStore {
        ImageStore { inner: Mutex::new(Inner::default()), max_bytes }
    }

    /// Keep an image and name it, or `None` when its type or data is not one
    /// a block may show.
    pub fn put(&self, media_type_raw: &str, data: &str) -> Option<ImageRef> {
        let media_type = image_media_type(media_type_raw)?;
        if data.len() > IMAGE_MAX_BASE64.min(self.max_bytes) || !is_base64(data) {
            return None;
        }
        let id = ImageId::derive(media_type, data);
        let mut inner = self.inner.lock();
        if inner.map.contains_key(&id) {
            // Seen again: it is the newest now.
            inner.order.retain(|o| o != &id);
            inner.order.push_back(id.clone());
        } else {
            while inner.bytes + data.len() > self.max_bytes {
                let Some(old) = inner.order.pop_front() else { break };
                if let Some(s) = inner.map.remove(&old) {
                    inner.bytes -= s.data.len();
                }
            }
            inner.bytes += data.len();
            inner.map.insert(id.clone(), Stored { media_type, data: Arc::from(data) });
            inner.order.push_back(id.clone());
        }
        Some(ImageRef { id, media_type: media_type.to_owned() })
    }

    /// The media type and base64 data of a stored image.
    pub fn get(&self, id: &ImageId) -> Option<(&'static str, Arc<str>)> {
        self.inner.lock().map.get(id).map(|s| (s.media_type, s.data.clone()))
    }
}

/// The process's store (one per core; every session view shares it).
pub fn global() -> &'static ImageStore {
    static STORE: OnceLock<ImageStore> = OnceLock::new();
    STORE.get_or_init(|| ImageStore::new(STORE_MAX_BYTES))
}

/// `{"type":"image","source":{"type":"base64","media_type":…,"data":…}}` —
/// borrowed so a large result is not copied before it is checked.
#[derive(Deserialize)]
struct PartIn<'a> {
    #[serde(rename = "type", borrow, default)]
    ty: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    source: Option<SourceIn<'a>>,
}

#[derive(Deserialize)]
struct SourceIn<'a> {
    #[serde(rename = "type", borrow, default)]
    ty: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    media_type: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    data: Option<Cow<'a, str>>,
}

fn keep(part: &PartIn<'_>, store: &ImageStore) -> Option<ImageRef> {
    if part.ty.as_deref() != Some("image") {
        return None;
    }
    let source = part.source.as_ref()?;
    if source.ty.as_deref() != Some("base64") {
        return None;
    }
    store.put(source.media_type.as_deref()?, source.data.as_deref()?)
}

/// The images of one content item (an `image` part), stored.
pub fn item_image(item: &Value, store: &ImageStore) -> Option<ImageRef> {
    let part = PartIn::deserialize(item).ok()?;
    keep(&part, store)
}

/// The images of a content array (a `tool_result.content`, a user message),
/// stored, in order. A string or anything that is not an array has none.
pub fn raw_images(raw: &RawValue, store: &ImageStore) -> Vec<ImageRef> {
    if !raw.get().trim_start().starts_with('[') || !raw.get().contains("\"image\"") {
        return Vec::new();
    }
    let Ok(parts) = serde_json::from_str::<Vec<PartIn<'_>>>(raw.get()) else { return Vec::new() };
    parts.iter().filter_map(|p| keep(p, store)).collect()
}

/// [`raw_images`] over a parsed value.
pub fn value_images(content: &Value, store: &ImageStore) -> Vec<ImageRef> {
    content.as_array().map(|items| items.iter().filter_map(|i| item_image(i, store)).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PNG: &str = "iVBORw0KGgo=";

    #[test]
    fn only_raster_types_and_base64_are_kept() {
        let s = ImageStore::new(1024);
        let r = s.put("image/PNG", PNG).expect("png");
        assert_eq!(r.media_type, "image/png");
        assert_eq!(s.get(&r.id).map(|(t, d)| (t, d.to_string())), Some(("image/png", PNG.to_string())));
        assert_eq!(s.put("image/jpg", PNG).map(|r| r.media_type), Some("image/jpeg".into()));
        assert!(s.put("image/svg+xml", PNG).is_none(), "an SVG could carry script");
        assert!(s.put("image/png", "not base64!").is_none());
        assert!(s.put("image/png", "ab=c").is_none(), "padding only at the end");
        assert!(s.put("image/png", "").is_none());
    }

    #[test]
    fn the_same_image_is_one_entry_and_the_oldest_goes_first() {
        let s = ImageStore::new(24);
        let a = s.put("image/png", "AAAAAAAA").unwrap();
        assert_eq!(s.put("image/png", "AAAAAAAA").unwrap().id, a.id, "same bytes, same id");
        let b = s.put("image/png", "BBBBBBBB").unwrap();
        let c = s.put("image/png", "CCCCCCCC").unwrap();
        let d = s.put("image/png", "DDDDDDDD").unwrap();
        assert!(s.get(&a.id).is_none(), "over the cap: the oldest left");
        assert!(s.get(&b.id).is_some() && s.get(&c.id).is_some() && s.get(&d.id).is_some());
        assert!(s.put("image/png", &"A".repeat(28)).is_none(), "larger than the whole store");
    }

    #[test]
    fn content_arrays_yield_their_image_parts_in_order() {
        let s = ImageStore::new(1024);
        let content = json!([
            {"type":"text","text":"a screenshot"},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}},
            {"type":"image","source":{"type":"url","url":"https://example.com/x.png"}},
            {"type":"image","source":{"type":"base64","media_type":"image/webp","data":"UklGRg=="}}
        ]);
        let raw = RawValue::from_string(content.to_string()).unwrap();
        let from_raw: Vec<String> = raw_images(&raw, &s).into_iter().map(|r| r.media_type).collect();
        assert_eq!(from_raw, ["image/png", "image/webp"], "never a URL: nothing is fetched");
        assert_eq!(value_images(&content, &s).len(), 2);
        assert!(raw_images(&RawValue::from_string("\"just text\"".into()).unwrap(), &s).is_empty());
    }
}
