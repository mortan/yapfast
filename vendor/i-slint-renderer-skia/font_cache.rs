// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use clru::CLruCache;
use i_slint_common::sharedfontique::HashedBlob;
use i_slint_core::textlayout::sharedparley::{fontique, parley};
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;

const FONT_CACHE_CAPACITY: NonZeroUsize = NonZeroUsize::new(64).unwrap();

pub struct FontCache {
    font_mgr: skia_safe::FontMgr,
    // Use HashedBlob in key to keep strong reference to font data blob,
    // preventing eviction from fontique's shared cache (see commit 30a03cf).
    // The u64 is a hash of variation settings (0 for base typefaces).
    fonts: CLruCache<(HashedBlob, u32, u64), Option<skia_safe::Typeface>>,
}

impl Default for FontCache {
    fn default() -> Self {
        Self { font_mgr: skia_safe::FontMgr::new(), fonts: CLruCache::new(FONT_CACHE_CAPACITY) }
    }
}

impl FontCache {
    pub fn font_with_variations(
        &mut self,
        font: &parley::FontData,
        synthesis: &fontique::Synthesis,
    ) -> Option<skia_safe::Typeface> {
        let variation_settings = synthesis.variation_settings();

        let mut variations_hash = 0u64;
        if !variation_settings.is_empty() {
            let mut hasher = DefaultHasher::new();
            for &(tag, value) in variation_settings {
                tag.to_be_bytes().hash(&mut hasher);
                value.to_bits().hash(&mut hasher);
            }
            variations_hash = hasher.finish();
        }

        let key = (font.data.clone().into(), font.index, variations_hash);

        if let Some(cached) = self.fonts.get(&key) {
            return cached.clone();
        }

        let mut typeface = self.load_typeface_internal(font);

        if !variation_settings.is_empty() {
            typeface = typeface.and_then(|base| {
                let coords: Vec<skia_safe::font_arguments::variation_position::Coordinate> =
                    variation_settings
                        .iter()
                        .map(|&(tag, value)| {
                            skia_safe::font_arguments::variation_position::Coordinate {
                                axis: skia_safe::FourByteTag::new(u32::from_be_bytes(
                                    tag.to_be_bytes(),
                                )),
                                value,
                            }
                        })
                        .collect();
                let position =
                    skia_safe::font_arguments::VariationPosition { coordinates: &coords };
                let args = skia_safe::FontArguments::new().set_variation_design_position(position);
                base.clone_with_arguments(&args).or(Some(base))
            });
        }

        self.fonts.put(key, typeface.clone());
        typeface
    }

    fn load_typeface_internal(&self, font: &parley::FontData) -> Option<skia_safe::Typeface> {
        // Yapfast: Skia reads the font where fontique keeps it, rather than a copy.
        // Fontique maps system fonts from disk, so the copy put each whole file
        // on the heap, 10 MiB for Noto Color Emoji, once more for each variation.
        let bytes = keep_for_skia(&font.data);
        // SAFETY: `keep_for_skia` keeps the blob, and so its bytes, alive and in
        // place until the process ends, so no typeface or Skia cache outlives them.
        let data = unsafe { skia_safe::Data::new_bytes(bytes) };
        let typeface = self
            .font_mgr
            .new_from_data(data, if font.index > 0 { Some(font.index as _) } else { None });

        // Due to  https://issues.skia.org/issues/310510989, fonts from true type collections
        // with an index > 0 fail to load on macOS. As a workaround, we manually extract the font from the
        // collection and load it as a single font.
        #[cfg(target_vendor = "apple")]
        if font.index > 0
            && typeface.is_none()
            && let Some(typeface) = read_fonts::CollectionRef::new(font.data.as_ref())
                .ok()
                .and_then(|ttc| ttc.get(font.index).ok())
                .and_then(|ttf| {
                    // Yapfast: the system's own face, and the copy only when it lists none.
                    system_face(&self.font_mgr, &ttf).or_else(|| {
                        let new_ttf =
                            write_fonts::FontBuilder::new().copy_missing_tables(ttf).build();
                        self.font_mgr.new_from_data(skia_safe::Data::new_copy(&new_ttf), None)
                    })
                })
        {
            return Some(typeface);
        }

        typeface
    }
}

/// Yapfast: the face of a font collection as macOS's font manager has it, found by its family,
/// PostScript name and glyph count, and taken only if its `head` table, with the font's revision,
/// modification time and checksum, is the same: another version of the font installed elsewhere
/// has other glyph ids for the same name and count. The system reads it from the collection's
/// file, where the face rebuilt as a font of its own sat on the heap for the whole session, tens
/// of megabytes for a CJK font. `None` for a face the font manager doesn't list, such as a hidden
/// system font.
#[cfg(target_vendor = "apple")]
fn system_face(
    font_mgr: &skia_safe::FontMgr,
    face: &read_fonts::FontRef,
) -> Option<skia_safe::Typeface> {
    use read_fonts::{
        TableProvider,
        types::{NameId, Tag},
    };
    // The `head` table is 54 bytes long.
    let head = face.table_data(Tag::new(b"head"))?.as_bytes();
    let name = face.name().ok()?;
    let data = name.string_data();
    let names = |id: NameId| {
        name.name_record()
            .iter()
            .filter(move |record| record.name_id() == id)
            .filter_map(move |record| record.string(data).ok())
            .map(|string| string.to_string())
    };
    let post_script_name = names(NameId::POSTSCRIPT_NAME).next()?;
    let glyphs = usize::from(face.maxp().ok()?.num_glyphs());
    names(NameId::TYPOGRAPHIC_FAMILY_NAME).chain(names(NameId::FAMILY_NAME)).find_map(|family| {
        let mut set = font_mgr.match_family(&family);
        (0..set.count()).filter_map(|index| set.new_typeface(index)).find(|typeface| {
            typeface.post_script_name().as_deref() == Some(post_script_name.as_str())
                && typeface.count_glyphs() == glyphs
                && {
                    let mut system_head = [0; 54];
                    let read = typeface
                        .get_table_data(u32::from_be_bytes(*b"head"), &mut system_head);
                    system_head.get(..read) == Some(head)
                }
        })
    })
}

/// Yapfast: every font blob Skia reads without a copy, by blob id, never released.
/// They are the few font files drawn with; fontique maps system fonts from disk
/// and bundled ones are in the executable, so keeping them takes no heap.
static KEPT_FOR_SKIA: std::sync::Mutex<std::collections::BTreeMap<u64, fontique::Blob<u8>>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

fn keep_for_skia(blob: &fontique::Blob<u8>) -> &'static [u8] {
    let mut kept = KEPT_FOR_SKIA.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let blob = kept.entry(blob.id()).or_insert_with(|| blob.clone());
    let bytes: &[u8] = blob.as_ref();
    // SAFETY: the blob stays in KEPT_FOR_SKIA, which nothing removes from, and
    // a blob's data is shared behind an Arc, so its bytes neither move nor go.
    unsafe { std::slice::from_raw_parts(bytes.as_ptr(), bytes.len()) }
}

thread_local! {
    pub static FONT_CACHE: RefCell<FontCache> = RefCell::new(Default::default())
}
