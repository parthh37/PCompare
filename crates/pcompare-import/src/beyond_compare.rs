//! Beyond Compare 3, 4 and 5 sessions: `BCSessions.xml`, or a `.bcpkg`
//! settings export (a zip that contains it).
//!
//! ```xml
//! <BCSessions Version="2" MinVersion="2">          <!-- BC5; "1" = BC4, none = BC3 -->
//!   <TSessionFolder>                                <!-- the root -->
//!     <Items>
//!       <TSessionFolder>                            <!-- unnamed: Auto-saved -->
//!         <Items>
//!           <TDirCompareSession Value="new &lt;--> old">
//!             <Specs><Left Value="C:\new"/><Right Value="C:\old"/></Specs>
//!             <Filters><NameFilter Value="-.git\;-*.tmp"/></Filters>
//!             <Rules><UseContentComparison Value="True"/></Rules>
//!           </TDirCompareSession>
//!         </Items>
//!       </TSessionFolder>
//!       <TDirCompareSession>…</TDirCompareSession>   <!-- no paths: the type's defaults -->
//!     </Items>
//!   </TSessionFolder>
//! </BCSessions>
//! ```
//!
//! Every value sits in a `Value` attribute. The session type is the element
//! name (Delphi class names); BC3 percent-encodes `<`/`>` in session names.

use crate::{format_err, zip_entry, ImportedSession, Parsed, Result, Skipped};
use roxmltree::Node;
use std::path::PathBuf;

/// BC session class → PCompare session kind.
fn kind_for(tag: &str) -> Option<&'static str> {
    Some(match tag {
        "TTextCompareSession" => "text",
        "TTextMergeSession" => "merge",
        "TDirCompareSession" => "folder",
        "TDirSyncSession" => "sync",
        "TDirMergeSession" => "folderMerge",
        "THexCompareSession" => "hex",
        "TPixCompareSession" | "TPictureCompareSession" => "picture",
        "TVerCompareSession" | "TVersionCompareSession" => "version",
        "TDataCompareSession" | "TTableCompareSession" => "table",
        "TMp3CompareSession" => "mp3",
        "TRegCompareSession" | "TRegistryCompareSession" => "registry",
        _ => return None,
    })
}

/// `Item <Name Value="x"/>` → `x`.
fn value<'a>(node: Node<'a, '_>, path: &[&str]) -> Option<&'a str> {
    let mut n = node;
    for name in path {
        n = n.children().find(|c| c.has_tag_name(*name))?;
    }
    n.attribute("Value")
}

fn is_true(node: Node, path: &[&str]) -> Option<bool> {
    value(node, path).map(|v| v.eq_ignore_ascii_case("true"))
}

/// BC3 writes `a %3C--%3E b`; decode `%XX` escapes.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The side of a session: `Specs/Left`, or `Left` straight under the session
/// (seen in sessions nested inside a folder session).
fn side(session: Node, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|n| value(session, &["Specs", n]).or_else(|| value(session, &[n])))
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
}

struct Walk {
    bc3: bool,
    out: Parsed,
}

impl Walk {
    /// `group` is the folder path so far; `auto` marks the Auto-saved folder.
    fn folder(&mut self, folder: Node, group: &[String], auto: bool, depth: usize) {
        let Some(items) = folder.children().find(|c| c.has_tag_name("Items")) else {
            return;
        };
        for item in items.children().filter(Node::is_element) {
            let tag = item.tag_name().name();
            if tag == "TSessionFolder" {
                match item.attribute("Value") {
                    Some(name) => {
                        let mut g = group.to_vec();
                        g.push(self.name(name));
                        self.folder(item, &g, auto, depth + 1);
                    }
                    // The unnamed folder inside the root holds BC's auto-saved sessions.
                    None => self.folder(item, group, auto || depth == 0, depth + 1),
                }
            } else if tag.ends_with("Session") {
                self.session(item, group, auto);
            }
        }
    }

    fn name(&self, raw: &str) -> String {
        if self.bc3 {
            percent_decode(raw)
        } else {
            raw.to_owned()
        }
    }

    fn session(&mut self, node: Node, group: &[String], auto: bool) {
        let tag = node.tag_name().name();
        let (left, right) = (side(node, &["Left"]), side(node, &["Right"]));
        // A session type with no name and no paths holds that type's defaults.
        let Some(raw_name) = node.attribute("Value") else {
            return;
        };
        let name = self.name(raw_name);
        let Some(kind) = kind_for(tag) else {
            let what = tag.trim_start_matches('T').trim_end_matches("Session");
            self.out.skipped.push(Skipped {
                name,
                reason: format!("Beyond Compare \"{what}\" sessions have no PCompare equivalent"),
            });
            return;
        };
        if left.is_none() && right.is_none() {
            self.out.skipped.push(Skipped {
                name,
                reason: "no paths saved".into(),
            });
            return;
        }
        let mut s = ImportedSession::new(
            name,
            kind,
            left.unwrap_or_default(),
            right.unwrap_or_default(),
        );
        s.auto_saved = auto;
        if !group.is_empty() {
            s.group = Some(group.join("/"));
        }
        if matches!(kind, "merge" | "folderMerge") {
            s.base = side(node, &["Center", "Base", "Ancestor"]);
            s.output = side(node, &["Output"]);
        }
        if let Some(filter) = value(node, &["Filters", "NameFilter"]) {
            apply_name_filter(&mut s, filter);
        } else if kind == "sync" {
            // BC syncs everything unless filtered; don't let PCompare's
            // default exclusions (.git, node_modules) silently skip files.
            s.set("exclude", "");
        }
        if matches!(kind, "folder" | "folderMerge") {
            apply_rules(&mut s, node);
        }
        if kind == "sync" {
            s.notes.push(
                "Sync direction isn't carried over; check the mode before running it.".into(),
            );
        }
        if is_true(node, &["NoEdit", "Left"]) == Some(true)
            || is_true(node, &["NoEdit", "Right"]) == Some(true)
        {
            s.notes
                .push("Read-only sides aren't kept in saved sessions.".into());
        }
        if kind == "table" && value(node, &["Keys"]).is_some() {
            s.notes.push("Key columns aren't carried over.".into());
        }
        self.out.sessions.push(s);
    }
}

/// BC folder rules → PCompare's comparison criteria.
fn apply_rules(s: &mut ImportedSession, node: Node) {
    if let Some(content) = is_true(node, &["Rules", "UseContentComparison"]) {
        s.set("content", if content { "rules" } else { "off" });
    }
    if let Some(ts) = is_true(node, &["Rules", "UseTimestampComparison"]) {
        s.set("compareTimestamp", ts);
    }
    if let Some(case) = is_true(node, &["Rules", "UseCaseSensitiveComparison"]) {
        s.set("caseSensitiveNames", case);
    }
    if let Some(follow) = is_true(node, &["Rules", "FollowSymLinks"]) {
        s.set("followLinks", follow);
    }
}

/// `*.cpp;-*.tmp;-.git\` → include files, exclude files, exclude folders
/// (`-` excludes, a trailing `\` or `/` names a folder).
fn apply_name_filter(s: &mut ImportedSession, filter: &str) {
    let (mut inc, mut exc_files, mut exc_dirs) = (Vec::new(), Vec::new(), Vec::new());
    for part in filter.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (exclude, mask) = match part.strip_prefix('-') {
            Some(m) => (true, m),
            None => (false, part.strip_prefix('+').unwrap_or(part)),
        };
        let is_dir = mask.ends_with(['\\', '/']);
        let mask = mask.trim_end_matches(['\\', '/']).to_owned();
        match (exclude, is_dir) {
            (true, true) => exc_dirs.push(mask),
            (true, false) => exc_files.push(mask),
            (false, false) if mask != "*" && mask != "*.*" => inc.push(mask),
            (false, true) => s.notes.push(format!(
                "Folder include filter `{mask}` isn't carried over."
            )),
            _ => {}
        }
    }
    s.set("includeFiles", inc.join(", "));
    s.set("excludeFiles", exc_files.join(", "));
    s.set("excludeFolders", exc_dirs.join(", "));
    if s.kind == "sync" {
        // The sync viewer has one exclusion list for names and folders.
        let all: Vec<String> = exc_files.into_iter().chain(exc_dirs).collect();
        s.set("exclude", all.join(", "));
    }
}

pub(crate) fn parse(root: Node) -> Result<Parsed> {
    let label = match root.attribute("Version") {
        None => "Beyond Compare 3",
        Some("1") => "Beyond Compare 4",
        Some("2") => "Beyond Compare 5",
        Some(_) => "Beyond Compare",
    };
    let mut walk = Walk {
        bc3: root.attribute("Version").is_none(),
        out: Parsed {
            label: label.into(),
            ..Default::default()
        },
    };
    for top in root.children().filter(|c| c.has_tag_name("TSessionFolder")) {
        walk.folder(top, &[], false, 0);
    }
    Ok(walk.out)
}

/// A `.bcpkg` from Tools › Export Settings: a zip with `BCSessions.xml` inside.
pub(crate) fn parse_package(bytes: &[u8]) -> Result<Parsed> {
    let xml = zip_entry(bytes, "BCSessions.xml")?
        .ok_or_else(|| format_err("this Beyond Compare package has no sessions (export them with Tools › Export Settings › Sessions)"))?;
    let mut parsed = crate::parse_bytes(&xml, std::path::Path::new(""))?;
    parsed.label = format!("{} settings package", parsed.label);
    Ok(parsed)
}

/// Where each BC version keeps its sessions (newest first).
pub(crate) fn locations(appdata: Option<PathBuf>, home: Option<PathBuf>) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    if let Some(appdata) = appdata {
        for v in [5, 4, 3] {
            out.push((
                format!("Beyond Compare {v}"),
                appdata
                    .join("Scooter Software")
                    .join(format!("Beyond Compare {v}"))
                    .join("BCSessions.xml"),
            ));
        }
    }
    if let Some(home) = home {
        let support = home.join("Library").join("Application Support");
        out.push((
            "Beyond Compare 5".into(),
            support.join("Beyond Compare 5").join("BCSessions.xml"),
        ));
        out.push((
            "Beyond Compare 4".into(),
            support.join("Beyond Compare").join("BCSessions.xml"),
        ));
        out.push((
            "Beyond Compare 5".into(),
            home.join(".config")
                .join("bcompare5")
                .join("BCSessions.xml"),
        ));
        out.push((
            "Beyond Compare".into(),
            home.join(".config").join("bcompare").join("BCSessions.xml"),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::parse_bytes;
    use std::path::Path;

    /// Shaped like a real BC5 file (folders, auto-saved, defaults, several kinds).
    const BC5: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- Produced by Beyond Compare 5 from Scooter Software -->
<BCSessions Version="2" MinVersion="2">
	<TSessionFolder>
		<Items>
			<TSessionFolder>
				<Items>
					<TDirCompareSession Value="new &lt;--&gt; old">
						<LastModified Value="2026-06-10 14:06:41"/>
						<Specs>
							<Left Value="C:\GIT\bpd\new"/>
							<Right Value="C:\GIT\bpd\old"/>
						</Specs>
					</TDirCompareSession>
					<TPixCompareSession Value="Foto.png &lt;--&gt; Foto2.png">
						<Specs><Left Value="C:\a\Foto.png"/><Right Value="C:\a\Foto2.png"/></Specs>
					</TPixCompareSession>
				</Items>
			</TSessionFolder>
			<TSessionFolder Value="Work">
				<Items>
					<TDirCompareSession Value="Site">
						<Filters><NameFilter Value="*.cs;-*.tmp;-.git\;-bin\"/></Filters>
						<Rules><UseContentComparison Value="True"/><UseTimestampComparison Value="False"/></Rules>
						<Specs><Left Value="D:\site"/><Right Value="\\nas\site"/></Specs>
					</TDirCompareSession>
					<TTextMergeSession Value="Merge it">
						<Specs><Left Value="C:\m\mine.c"/><Right Value="C:\m\theirs.c"/><Center Value="C:\m\base.c"/><Output Value="C:\m\out.c"/></Specs>
					</TTextMergeSession>
					<TDirSyncSession Value="Backup">
						<Specs><Left Value="C:\docs"/><Right Value="E:\docs"/></Specs>
					</TDirSyncSession>
					<TFtpSession Value="Server">
						<Specs><Left Value="ftp://x"/></Specs>
					</TFtpSession>
				</Items>
			</TSessionFolder>
			<TDirCompareSession>
				<State><DisplayFilter Value="[showLeftAdded]"/></State>
			</TDirCompareSession>
		</Items>
	</TSessionFolder>
</BCSessions>"#;

    #[test]
    fn reads_bc5_sessions_folders_and_auto_saved() {
        let p = parse_bytes(BC5.as_bytes(), Path::new("BCSessions.xml")).unwrap();
        assert_eq!(p.label, "Beyond Compare 5");
        let names: Vec<_> = p
            .sessions
            .iter()
            .map(|s| (s.name.as_str(), s.kind, s.auto_saved, s.group.as_deref()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("new <--> old", "folder", true, None),
                ("Foto.png <--> Foto2.png", "picture", true, None),
                ("Site", "folder", false, Some("Work")),
                ("Merge it", "merge", false, Some("Work")),
                ("Backup", "sync", false, Some("Work")),
            ]
        );
        let site = &p.sessions[2];
        assert_eq!(
            (site.left.as_str(), site.right.as_str()),
            (r"D:\site", r"\\nas\site")
        );
        assert_eq!(site.settings["includeFiles"], "*.cs");
        assert_eq!(site.settings["excludeFiles"], "*.tmp");
        assert_eq!(site.settings["excludeFolders"], ".git, bin");
        assert_eq!(site.settings["content"], "rules");
        assert_eq!(site.settings["compareTimestamp"], false);
        let merge = &p.sessions[3];
        assert_eq!(merge.base.as_deref(), Some(r"C:\m\base.c"));
        assert_eq!(merge.output.as_deref(), Some(r"C:\m\out.c"));
        let sync = &p.sessions[4];
        assert_eq!(
            sync.settings["exclude"], "",
            "sync must not inherit PCompare's default exclusions"
        );
        assert!(sync.notes[0].contains("direction"));
        // The FTP session is listed as skipped; the unnamed defaults node is ignored.
        assert_eq!(p.skipped.len(), 1);
        assert!(p.skipped[0].reason.contains("\"Ftp\""));
    }

    #[test]
    fn reads_bc3_percent_encoded_names() {
        let xml = r#"<BCSessions><TSessionFolder><Items><TSessionFolder Value="Docs"><Items>
            <TTextCompareSession Value="original.docx %3C--%3E poc.docx">
              <Specs><Left Value="C:\o.docx"/><Right Value="C:\p.docx"/></Specs>
            </TTextCompareSession>
            <TDirCompareSession Value="lib">
              <Filters><NameFilter Value="-*.pyc"/></Filters>
              <Specs><Left Value="F:\a\lib"/><Right Value="F:\b\lib"/></Specs>
            </TDirCompareSession>
        </Items></TSessionFolder></Items></TSessionFolder></BCSessions>"#;
        let p = parse_bytes(xml.as_bytes(), Path::new("x")).unwrap();
        assert_eq!(p.label, "Beyond Compare 3");
        assert_eq!(p.sessions[0].name, "original.docx <--> poc.docx");
        assert_eq!(p.sessions[0].group.as_deref(), Some("Docs"));
        assert_eq!(p.sessions[1].settings["excludeFiles"], "*.pyc");
    }

    #[test]
    fn reads_sessions_from_a_bcpkg() {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            zip.start_file("BCSessions.xml", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(BC5.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        let p = parse_bytes(&buf.into_inner(), Path::new("settings.bcpkg")).unwrap();
        assert_eq!(p.label, "Beyond Compare 5 settings package");
        assert_eq!(p.sessions.len(), 5);
    }
}
