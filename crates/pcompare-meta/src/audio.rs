//! Audio tags, embedded pictures and stream properties via `lofty`
//! (MP3, FLAC, M4A/MP4, Ogg Vorbis/Opus/Speex, WAV, AIFF, APE, WavPack, MPC, AAC).
//!
//! Tags are read twice: once generically, which maps format-specific keys
//! to readable names, and once through the concrete file type to recover
//! the items lofty cannot map (custom ID3v2 frames, unknown Vorbis fields…),
//! which are listed under their raw keys.

use crate::property::{Property, PropertyCompareResult};
use crate::{format_bytes, group_thousands};
use base64::Engine;
use lofty::config::ParseOptions;
use lofty::file::{AudioFile, FileType, TaggedFile, TaggedFileExt};
use lofty::id3::v2::{Frame, Id3v2Tag, Id3v2Version};
use lofty::picture::{Picture, PictureInformation, PictureType};
use lofty::probe::Probe;
use lofty::tag::{ItemKey, ItemValue, SplitTag, Tag, TagType};
use pcompare_core::{CoreError, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

pub const GROUP_TAGS: &str = "Tags";
pub const GROUP_AUDIO: &str = "Audio";
pub const GROUP_PICTURES: &str = "Pictures";
pub const GROUP_FILE: &str = "File";

/// Long text (lyrics, descriptions) is cut to this many characters.
const MAX_TEXT: usize = 300;
/// Pictures up to this size are sent to the UI as data URLs for thumbnails.
const MAX_THUMBNAIL_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PictureInfo {
    pub kind: String,
    pub mime: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub size: u64,
    /// `data:` URL of the original image, when small enough to display.
    pub data_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioSummary {
    pub format: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub duration_ms: u64,
    pub bitrate_kbps: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct AudioInfo {
    pub summary: AudioSummary,
    pub properties: Vec<Property>,
    pub pictures: Vec<PictureInfo>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioCompareResult {
    #[serde(flatten)]
    pub compare: PropertyCompareResult,
    pub left: AudioSummary,
    pub right: AudioSummary,
    pub left_pictures: Vec<PictureInfo>,
    pub right_pictures: Vec<PictureInfo>,
}

pub fn compare_audio(left: &Path, right: &Path) -> Result<AudioCompareResult> {
    let l = read_audio(left)?;
    let r = read_audio(right)?;
    Ok(AudioCompareResult {
        compare: PropertyCompareResult::new(&l.properties, &r.properties),
        left: l.summary,
        right: r.summary,
        left_pictures: l.pictures,
        right_pictures: r.pictures,
    })
}

fn lofty_err(path: &Path, e: impl std::fmt::Display) -> CoreError {
    CoreError::Other(format!("{}: cannot read audio file ({e})", path.display()))
}

pub fn read_audio(path: &Path) -> Result<AudioInfo> {
    let size = std::fs::metadata(path)
        .map_err(|e| CoreError::io(path, e))?
        .len();
    let tagged: TaggedFile = Probe::open(path)
        .map_err(|e| lofty_err(path, e))?
        .guess_file_type()
        .map_err(|e| CoreError::io(path, e))?
        .read()
        .map_err(|e| lofty_err(path, e))?;
    // The raw pass is best-effort: the generic read already succeeded, so a
    // failure here only loses the unmapped items.
    let details = read_details(path, tagged.file_type()).unwrap_or_default();

    let mut props = Vec::new();
    let mut pictures = Vec::new();

    // The format's primary tag is grouped as plain "Tags" so files of
    // different formats (ID3v2 vs Vorbis Comments) still line up.
    let primary = tagged
        .primary_tag()
        .or_else(|| tagged.first_tag())
        .map(Tag::tag_type);
    let mut tags: Vec<&Tag> = tagged.tags().iter().collect();
    tags.sort_by_key(|t| Some(t.tag_type()) != primary);
    for tag in &tags {
        let group = if Some(tag.tag_type()) == primary {
            GROUP_TAGS.to_owned()
        } else {
            format!("{GROUP_TAGS} ({})", tag_label(tag.tag_type(), None))
        };
        tag_properties(tag, &group, &details, &mut props);
        pictures.extend(tag.pictures().iter().map(picture_info));
    }
    if tags.is_empty() {
        props.push(Property::new(GROUP_TAGS, "Format", "No tags"));
    }
    for (i, pic) in pictures.iter().enumerate() {
        let mut desc = vec![pic.mime.clone().unwrap_or_else(|| "unknown type".into())];
        if let (Some(w), Some(h)) = (pic.width, pic.height) {
            desc.push(format!("{w}×{h}"));
        }
        desc.push(format_bytes(pic.size));
        let all = tags.iter().flat_map(|t| t.pictures());
        if let Some(p) = all.clone().nth(i) {
            desc.push(format!("hash {:016x}", fnv1a(p.data())));
        }
        props.push(Property::new(GROUP_PICTURES, &pic.kind, desc.join(", ")));
    }

    let p = tagged.properties();
    let format = file_type_label(tagged.file_type());
    let mut add = |name: &str, value: String| props.push(Property::new(GROUP_AUDIO, name, value));
    add("Format", format.clone());
    add("Duration", format_duration(p.duration()));
    if let Some(b) = p.audio_bitrate() {
        add("Audio bitrate", format!("{b} kbps"));
    }
    if let Some(b) = p.overall_bitrate() {
        add("Overall bitrate", format!("{b} kbps"));
    }
    if let Some(r) = p.sample_rate() {
        add(
            "Sample rate",
            format!("{} Hz", group_thousands(u64::from(r))),
        );
    }
    if let Some(c) = p.channels() {
        add(
            "Channels",
            match c {
                1 => "1 (mono)".into(),
                2 => "2 (stereo)".into(),
                n => n.to_string(),
            },
        );
    }
    if let Some(d) = p.bit_depth() {
        add("Bit depth", format!("{d}-bit"));
    }
    props.push(Property::new(GROUP_FILE, "Size", format_bytes(size)));

    let primary_tag = tags.first().filter(|t| Some(t.tag_type()) == primary);
    let text = |k: ItemKey| primary_tag.and_then(|t| t.get_string(k)).map(str::to_owned);
    let summary = AudioSummary {
        format,
        title: text(ItemKey::TrackTitle),
        artist: text(ItemKey::TrackArtist),
        duration_ms: p.duration().as_millis() as u64,
        bitrate_kbps: p.audio_bitrate().or(p.overall_bitrate()),
    };
    Ok(AudioInfo {
        summary,
        properties: props,
        pictures,
    })
}

// ---------- generic tags ----------

/// Display name and sort rank for well-known keys; unknown keys sort after.
fn key_label(key: ItemKey) -> (String, usize) {
    const KNOWN: &[(ItemKey, &str)] = &[
        (ItemKey::TrackTitle, "Title"),
        (ItemKey::TrackSubtitle, "Subtitle"),
        (ItemKey::TrackArtist, "Artist"),
        (ItemKey::TrackArtists, "Artists"),
        (ItemKey::AlbumTitle, "Album"),
        (ItemKey::AlbumArtist, "Album Artist"),
        (ItemKey::Year, "Year"),
        (ItemKey::RecordingDate, "Date"),
        (ItemKey::ReleaseDate, "Release Date"),
        (ItemKey::OriginalReleaseDate, "Original Release Date"),
        (ItemKey::TrackNumber, "Track"),
        (ItemKey::TrackTotal, "Track Total"),
        (ItemKey::DiscNumber, "Disc"),
        (ItemKey::DiscTotal, "Disc Total"),
        (ItemKey::Genre, "Genre"),
        (ItemKey::Composer, "Composer"),
        (ItemKey::Conductor, "Conductor"),
        (ItemKey::Lyricist, "Lyricist"),
        (ItemKey::Comment, "Comment"),
        (ItemKey::Bpm, "BPM"),
        (ItemKey::IntegerBpm, "BPM (integer)"),
        (ItemKey::InitialKey, "Key"),
        (ItemKey::Label, "Label"),
        (ItemKey::Publisher, "Publisher"),
        (ItemKey::CopyrightMessage, "Copyright"),
        (ItemKey::Isrc, "ISRC"),
        (ItemKey::FlagCompilation, "Compilation"),
        (ItemKey::EncodedBy, "Encoded By"),
        (ItemKey::EncoderSoftware, "Encoder"),
        (ItemKey::EncoderSettings, "Encoder Settings"),
        (ItemKey::Lyrics, "Lyrics"),
        (ItemKey::UnsyncLyrics, "Lyrics (unsynced)"),
    ];
    if let Some(i) = KNOWN.iter().position(|(k, _)| *k == key) {
        return (KNOWN[i].1.to_owned(), i);
    }
    // Fall back to splitting the variant name: `ReplayGainTrackGain` → `Replay Gain Track Gain`.
    let raw = format!("{key:?}");
    let mut out = String::with_capacity(raw.len() + 4);
    for (i, c) in raw.chars().enumerate() {
        if i > 0 && c.is_ascii_uppercase() {
            out.push(' ');
        }
        out.push(c);
    }
    (out, KNOWN.len())
}

fn clip(s: &str) -> String {
    let s = s.trim_end_matches('\0');
    match s.char_indices().nth(MAX_TEXT) {
        Some((i, _)) => format!("{}… ({} characters)", &s[..i], s.chars().count()),
        None => s.to_owned(),
    }
}

fn item_text(value: &ItemValue) -> String {
    match value {
        ItemValue::Text(t) | ItemValue::Locator(t) => clip(t),
        ItemValue::Binary(b) => format!("<{}>", format_bytes(b.len() as u64)),
    }
}

fn tag_properties(tag: &Tag, group: &str, details: &Details, props: &mut Vec<Property>) {
    let version = (tag.tag_type() == TagType::Id3v2)
        .then_some(details.id3v2_version)
        .flatten();
    props.push(Property::new(
        group,
        "Format",
        tag_label(tag.tag_type(), version),
    ));

    // Multiple values for one name are joined, e.g. several artists.
    let mut items: BTreeMap<(usize, String), Vec<String>> = BTreeMap::new();
    for item in tag.items() {
        let (mut name, rank) = key_label(item.key());
        if !item.description().is_empty() {
            name = format!("{name} ({})", item.description());
        }
        items
            .entry((rank, name))
            .or_default()
            .push(item_text(item.value()));
    }
    // Present "Track 3/12" rather than two separate rows.
    let take = |items: &mut BTreeMap<(usize, String), Vec<String>>, k: ItemKey| {
        let (name, rank) = key_label(k);
        items.remove(&(rank, name)).map(|v| v.join("; "))
    };
    for (num, total) in [
        (ItemKey::TrackNumber, ItemKey::TrackTotal),
        (ItemKey::DiscNumber, ItemKey::DiscTotal),
    ] {
        let n = take(&mut items, num);
        let t = take(&mut items, total);
        let combined = match (n, t) {
            (Some(n), Some(t)) => Some(format!("{n}/{t}")),
            (Some(n), None) => Some(n),
            (None, Some(t)) => Some(format!("?/{t}")),
            (None, None) => None,
        };
        if let Some(v) = combined {
            let (name, rank) = key_label(num);
            items.insert((rank, name), vec![v]);
        }
    }
    for ((_, name), values) in items {
        props.push(Property::new(group, name, values.join("; ")));
    }
    if tag.tag_type() == TagType::VorbisComments {
        if let Some(vendor) = &details.vorbis_vendor {
            props.push(Property::new(group, "Vendor", vendor));
        }
    }
    for (_, key, value) in details
        .extras
        .iter()
        .filter(|(t, _, _)| *t == tag.tag_type())
    {
        props.push(Property::new(group, key, value));
    }
}

fn tag_label(t: TagType, id3v2: Option<Id3v2Version>) -> String {
    match t {
        TagType::Id3v2 => match id3v2 {
            Some(Id3v2Version::V2) => "ID3v2.2".into(),
            Some(Id3v2Version::V3) => "ID3v2.3".into(),
            Some(Id3v2Version::V4) => "ID3v2.4".into(),
            None => "ID3v2".into(),
        },
        TagType::Id3v1 => "ID3v1".into(),
        TagType::Ape => "APEv2".into(),
        TagType::VorbisComments => "Vorbis Comments".into(),
        TagType::Mp4Ilst => "MP4 (iTunes)".into(),
        TagType::RiffInfo => "RIFF INFO".into(),
        TagType::AiffText => "AIFF text".into(),
        other => format!("{other:?}"),
    }
}

fn file_type_label(t: FileType) -> String {
    match t {
        FileType::Aac => "AAC (ADTS)".into(),
        FileType::Aiff => "AIFF".into(),
        FileType::Ape => "Monkey's Audio".into(),
        FileType::Flac => "FLAC".into(),
        FileType::Mpeg => "MPEG audio (MP3)".into(),
        FileType::Mp4 => "MPEG-4 audio".into(),
        FileType::Mpc => "Musepack".into(),
        FileType::Opus => "Opus".into(),
        FileType::Vorbis => "Ogg Vorbis".into(),
        FileType::Speex => "Speex".into(),
        FileType::Wav => "WAV".into(),
        FileType::WavPack => "WavPack".into(),
        other => format!("{other:?}"),
    }
}

fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    let (h, m, s, ms) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{ms:03}")
    } else {
        format!("{m}:{s:02}.{ms:03}")
    }
}

// ---------- pictures ----------

fn picture_kind(t: PictureType) -> String {
    match t {
        PictureType::Other => "Other".into(),
        PictureType::Icon => "File icon".into(),
        PictureType::OtherIcon => "Other icon".into(),
        PictureType::CoverFront => "Front cover".into(),
        PictureType::CoverBack => "Back cover".into(),
        PictureType::Leaflet => "Leaflet".into(),
        PictureType::Media => "Media".into(),
        PictureType::LeadArtist => "Lead artist".into(),
        PictureType::Artist => "Artist".into(),
        PictureType::Conductor => "Conductor".into(),
        PictureType::Band => "Band".into(),
        PictureType::Composer => "Composer".into(),
        PictureType::Lyricist => "Lyricist".into(),
        PictureType::RecordingLocation => "Recording location".into(),
        PictureType::DuringRecording => "During recording".into(),
        PictureType::DuringPerformance => "During performance".into(),
        PictureType::ScreenCapture => "Screen capture".into(),
        PictureType::BrightFish => "Bright fish".into(),
        PictureType::Illustration => "Illustration".into(),
        PictureType::BandLogo => "Band logo".into(),
        PictureType::PublisherLogo => "Publisher logo".into(),
        other => format!("{other:?}"),
    }
}

fn picture_info(p: &Picture) -> PictureInfo {
    let mime = p.mime_type().map(|m| m.as_str().to_owned());
    // Only PNG and JPEG headers are understood; anything else reports 0×0.
    let dims = PictureInformation::from_picture(p)
        .ok()
        .filter(|i| i.width > 0 && i.height > 0);
    let displayable = mime.as_deref().is_some_and(|m| {
        matches!(
            m,
            "image/jpeg" | "image/jpg" | "image/png" | "image/gif" | "image/bmp" | "image/webp"
        )
    });
    let data_url = (displayable && p.data().len() <= MAX_THUMBNAIL_BYTES).then(|| {
        format!(
            "data:{};base64,{}",
            mime.as_deref().unwrap_or_default(),
            base64::engine::general_purpose::STANDARD.encode(p.data())
        )
    });
    PictureInfo {
        kind: picture_kind(p.pic_type()),
        mime,
        width: dims.map(|d| d.width),
        height: dims.map(|d| d.height),
        size: p.data().len() as u64,
        data_url,
    }
}

/// Cheap content fingerprint so two same-sized covers can still differ.
fn fnv1a(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

// ---------- format-specific pass ----------

#[derive(Default)]
struct Details {
    id3v2_version: Option<Id3v2Version>,
    vorbis_vendor: Option<String>,
    /// Items lofty's generic tag drops, as `(tag, raw key, value)`.
    extras: Vec<(TagType, String, String)>,
}

impl Details {
    fn id3v2(&mut self, tag: Option<&Id3v2Tag>) {
        let Some(tag) = tag else { return };
        self.id3v2_version = Some(tag.original_version());
        let (remainder, _) = tag.clone().split_tag();
        for frame in remainder.iter() {
            let (key, value) = frame_entry(frame);
            self.extras.push((TagType::Id3v2, key, value));
        }
    }

    fn vorbis(&mut self, tag: Option<&lofty::ogg::tag::VorbisComments>) {
        let Some(tag) = tag else { return };
        if !tag.vendor().is_empty() {
            self.vorbis_vendor = Some(tag.vendor().to_owned());
        }
        let (remainder, _) = tag.clone().split_tag();
        for (k, v) in remainder.items() {
            self.extras
                .push((TagType::VorbisComments, k.to_owned(), clip(v)));
        }
    }

    fn ape(&mut self, tag: Option<&lofty::ape::ApeTag>) {
        let Some(tag) = tag else { return };
        let (remainder, _) = tag.clone().split_tag();
        for item in &*remainder {
            self.extras
                .push((TagType::Ape, item.key().to_owned(), item_text(item.value())));
        }
    }

    fn ilst(&mut self, tag: Option<&lofty::mp4::Ilst>) {
        use lofty::mp4::{AtomData, AtomIdent};
        let Some(tag) = tag else { return };
        let (remainder, _) = tag.clone().split_tag();
        for atom in &*remainder {
            let key = match atom.ident() {
                // FOURCCs are Latin-1 (`©nam`).
                AtomIdent::Fourcc(b) => b.iter().map(|&c| c as char).collect(),
                AtomIdent::Freeform { mean, name } => format!("----:{mean}:{name}"),
            };
            let values: Vec<String> = atom
                .data()
                .map(|d| match d {
                    AtomData::UTF8(s) | AtomData::UTF16(s) => clip(s.as_str()),
                    AtomData::SignedInteger(n) => n.to_string(),
                    AtomData::UnsignedInteger(n) => n.to_string(),
                    AtomData::Bool(b) => b.to_string(),
                    AtomData::Picture(p) => {
                        format!("<picture, {}>", format_bytes(p.data().len() as u64))
                    }
                    _ => "<data>".to_owned(),
                })
                .collect();
            self.extras.push((TagType::Mp4Ilst, key, values.join("; ")));
        }
    }
}

fn frame_entry(frame: &Frame<'_>) -> (String, String) {
    let id = frame.id_str().to_owned();
    match frame {
        Frame::Text(f) => (id, clip(&f.value)),
        Frame::UserText(f) => (format!("{id}:{}", f.description), clip(&f.content)),
        Frame::Url(f) => (id, f.url().to_owned()),
        Frame::UserUrl(f) => (format!("{id}:{}", f.description), f.content.to_string()),
        Frame::Comment(f) => (format!("{id}:{}", f.description), clip(&f.content)),
        Frame::UnsynchronizedText(f) => (format!("{id}:{}", f.description), clip(&f.content)),
        Frame::KeyValue(f) => {
            let pairs: Vec<String> = f
                .key_value_pairs
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            (id, clip(&pairs.join("; ")))
        }
        Frame::Private(f) => (
            format!("{id}:{}", f.owner),
            format!("<{}>", format_bytes(f.private_data.len() as u64)),
        ),
        Frame::UniqueFileIdentifier(f) => {
            let ident = std::str::from_utf8(&f.identifier)
                .ok()
                .filter(|s| !s.chars().any(char::is_control))
                .map_or_else(
                    || format!("<{}>", format_bytes(f.identifier.len() as u64)),
                    str::to_owned,
                );
            (format!("{id}:{}", f.owner), ident)
        }
        Frame::Timestamp(f) => (id, f.timestamp.to_string()),
        Frame::Binary(f) => (id, format!("<{}>", format_bytes(f.data.len() as u64))),
        _ => (id, "<present>".to_owned()),
    }
}

fn read_details(path: &Path, file_type: FileType) -> Option<Details> {
    use lofty::{aac, ape, flac, iff, mp4, mpeg, musepack, ogg, wavpack};

    let options = ParseOptions::new()
        .read_properties(false)
        .read_cover_art(false);
    let mut file = std::fs::File::open(path).ok()?;
    let mut d = Details::default();
    match file_type {
        FileType::Mpeg => {
            let f = mpeg::MpegFile::read_from(&mut file, options).ok()?;
            d.id3v2(f.id3v2());
            d.ape(f.ape());
        }
        FileType::Flac => {
            let f = flac::FlacFile::read_from(&mut file, options).ok()?;
            d.vorbis(f.vorbis_comments());
            d.id3v2(f.id3v2());
        }
        FileType::Opus => d.vorbis(Some(
            ogg::OpusFile::read_from(&mut file, options)
                .ok()?
                .vorbis_comments(),
        )),
        FileType::Vorbis => d.vorbis(Some(
            ogg::VorbisFile::read_from(&mut file, options)
                .ok()?
                .vorbis_comments(),
        )),
        FileType::Speex => d.vorbis(Some(
            ogg::SpeexFile::read_from(&mut file, options)
                .ok()?
                .vorbis_comments(),
        )),
        FileType::Wav => d.id3v2(
            iff::wav::WavFile::read_from(&mut file, options)
                .ok()?
                .id3v2(),
        ),
        FileType::Aiff => d.id3v2(
            iff::aiff::AiffFile::read_from(&mut file, options)
                .ok()?
                .id3v2(),
        ),
        FileType::Aac => d.id3v2(aac::AacFile::read_from(&mut file, options).ok()?.id3v2()),
        FileType::Ape => {
            let f = ape::ApeFile::read_from(&mut file, options).ok()?;
            d.ape(f.ape());
            d.id3v2(f.id3v2());
        }
        FileType::WavPack => d.ape(
            wavpack::WavPackFile::read_from(&mut file, options)
                .ok()?
                .ape(),
        ),
        FileType::Mpc => {
            let f = musepack::MpcFile::read_from(&mut file, options).ok()?;
            d.ape(f.ape());
            d.id3v2(f.id3v2());
        }
        FileType::Mp4 => d.ilst(mp4::Mp4File::read_from(&mut file, options).ok()?.ilst()),
        _ => {}
    }
    Some(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::property::DiffStatus;
    use lofty::config::WriteOptions;
    use lofty::id3::v2::PrivateFrame;
    use lofty::picture::MimeType;
    use lofty::tag::TagExt;

    /// A 1×1 transparent PNG.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    /// 40 silent MPEG-1 Layer III frames (128 kbps, 44.1 kHz, stereo) ≈ 1 s.
    fn mp3_bytes() -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..40 {
            out.extend_from_slice(&[0xff, 0xfb, 0x90, 0x00]);
            out.extend(std::iter::repeat_n(0u8, 417 - 4));
        }
        out
    }

    /// 0.1 s of 16-bit stereo silence at 44.1 kHz.
    fn wav_bytes() -> Vec<u8> {
        let data_len: u32 = 44_100 * 4 / 10;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&2u16.to_le_bytes()); // channels
        out.extend_from_slice(&44_100u32.to_le_bytes());
        out.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.extend(std::iter::repeat_n(0u8, data_len as usize));
        out
    }

    fn write_mp3(
        dir: &Path,
        name: &str,
        title: &str,
        artist: &str,
        with_extras: bool,
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, mp3_bytes()).unwrap();
        let mut generic = Tag::new(TagType::Id3v2);
        generic.insert_text(ItemKey::TrackTitle, title.into());
        generic.insert_text(ItemKey::TrackArtist, artist.into());
        generic.insert_text(ItemKey::TrackNumber, "3".into());
        generic.insert_text(ItemKey::TrackTotal, "12".into());
        generic.push_picture(
            Picture::unchecked(PNG.to_vec())
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Png)
                .build(),
        );
        let mut tag = Id3v2Tag::from(generic);
        if with_extras {
            tag.insert(Frame::Private(PrivateFrame::new(
                "com.example",
                vec![1u8, 2, 3],
            )));
            tag.insert_user_text("MY_CUSTOM_FIELD".into(), "custom value".into());
        }
        tag.save_to_path(&path, WriteOptions::default()).unwrap();
        path
    }

    fn get<'a>(info: &'a AudioInfo, group: &str, name: &str) -> Option<&'a str> {
        info.properties
            .iter()
            .find(|p| p.group == group && p.name == name)
            .map(|p| p.value.as_str())
    }

    #[test]
    fn reads_mp3_tags_and_extras() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_mp3(dir.path(), "a.mp3", "Song", "Band", true);
        let info = read_audio(&path).unwrap();

        assert_eq!(info.summary.format, "MPEG audio (MP3)");
        assert_eq!(info.summary.title.as_deref(), Some("Song"));
        assert_eq!(get(&info, GROUP_TAGS, "Format"), Some("ID3v2.4"));
        assert_eq!(get(&info, GROUP_TAGS, "Title"), Some("Song"));
        assert_eq!(get(&info, GROUP_TAGS, "Artist"), Some("Band"));
        assert_eq!(get(&info, GROUP_TAGS, "Track"), Some("3/12"));
        assert_eq!(
            get(&info, GROUP_TAGS, "PRIV:com.example"),
            Some("<3 bytes>")
        );
        assert_eq!(
            get(&info, GROUP_TAGS, "TXXX:MY_CUSTOM_FIELD"),
            Some("custom value")
        );
        assert_eq!(get(&info, GROUP_AUDIO, "Sample rate"), Some("44,100 Hz"));
        assert_eq!(get(&info, GROUP_AUDIO, "Channels"), Some("2 (stereo)"));

        assert_eq!(info.pictures.len(), 1);
        let pic = &info.pictures[0];
        assert_eq!(pic.kind, "Front cover");
        assert_eq!((pic.width, pic.height), (Some(1), Some(1)));
        assert!(pic
            .data_url
            .as_deref()
            .unwrap()
            .starts_with("data:image/png;base64,"));
        let expected = format!("image/png, 1×1, {} bytes, hash ", PNG.len());
        assert!(get(&info, GROUP_PICTURES, "Front cover")
            .unwrap()
            .starts_with(&expected));
    }

    #[test]
    fn compares_mp3s() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_mp3(dir.path(), "a.mp3", "Song", "Band", true);
        let b = write_mp3(dir.path(), "b.mp3", "Song (Remix)", "Band", false);
        let res = compare_audio(&a, &b).unwrap();
        let row = |n: &str| res.compare.rows.iter().find(|r| r.name == n).unwrap();
        assert_eq!(row("Title").status, DiffStatus::Different);
        assert_eq!(row("Artist").status, DiffStatus::Same);
        assert_eq!(row("PRIV:com.example").status, DiffStatus::LeftOnly);
        assert_eq!(row("Front cover").status, DiffStatus::Same);
        assert_eq!(row("Duration").status, DiffStatus::Same);
        assert_eq!(res.left_pictures.len(), 1);
    }

    #[test]
    fn reads_wav_with_riff_info() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        std::fs::write(&path, wav_bytes()).unwrap();
        let mut tag = Tag::new(TagType::RiffInfo);
        tag.insert_text(ItemKey::TrackTitle, "Wave".into());
        tag.save_to_path(&path, WriteOptions::default()).unwrap();

        let info = read_audio(&path).unwrap();
        assert_eq!(info.summary.format, "WAV");
        assert_eq!(get(&info, GROUP_TAGS, "Format"), Some("RIFF INFO"));
        assert_eq!(get(&info, GROUP_TAGS, "Title"), Some("Wave"));
        assert_eq!(get(&info, GROUP_AUDIO, "Bit depth"), Some("16-bit"));
        assert_eq!(get(&info, GROUP_AUDIO, "Duration"), Some("0:00.100"));
        assert!(info.pictures.is_empty());
    }

    #[test]
    fn untagged_and_invalid_files() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("plain.wav");
        std::fs::write(&wav, wav_bytes()).unwrap();
        let info = read_audio(&wav).unwrap();
        assert_eq!(get(&info, GROUP_TAGS, "Format"), Some("No tags"));

        let junk = dir.path().join("junk.mp3");
        std::fs::write(&junk, b"definitely not audio").unwrap();
        assert!(read_audio(&junk).is_err());
        assert!(read_audio(&dir.path().join("missing.mp3")).is_err());
    }

    #[test]
    fn helpers() {
        assert_eq!(format_duration(Duration::from_millis(225_250)), "3:45.250");
        assert_eq!(
            format_duration(Duration::from_millis(3_725_000)),
            "1:02:05.000"
        );
        assert_eq!(
            key_label(ItemKey::ReplayGainTrackGain).0,
            "Replay Gain Track Gain"
        );
        let long = "x".repeat(MAX_TEXT + 5);
        assert!(clip(&long).ends_with(&format!("… ({} characters)", MAX_TEXT + 5)));
    }
}
