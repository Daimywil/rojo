//! Mapping between Roblox instance names and asset file names.
//!
//! Roblox allows any string as an instance name, including characters that are
//! invalid in file names and siblings that share a name. Asset files need a
//! unique, valid name for every instance, so names are encoded:
//!
//! - Characters that are invalid on common filesystems, control characters,
//!   `%`, and `~` are percent-encoded byte by byte (`:` becomes `%3A`).
//! - A leading `.` or space and a trailing `.` or space are percent-encoded,
//!   since they make hidden files or get stripped by Windows.
//! - Names reserved by Windows (`CON`, `nul.txt`, ...) get their first
//!   character percent-encoded.
//! - Siblings whose names collide (ignoring case, since Windows and macOS
//!   filesystems usually do too) get a `~2`, `~3`, ... suffix in sibling
//!   order. `~` is always encoded in names, so the suffix is unambiguous.
//!
//! The Studio plugin implements the exact same encoding in
//! `plugin/src/AssetSync/names.lua`. Both are tested against the same cases,
//! so any change here must be mirrored there.

use std::{cmp::Ordering, collections::BTreeMap, ops::Bound};

/// The extension of files that store a single asset.
pub const UNIT_EXTENSION: &str = ".rbxm";

// The plugin is what decides the file name of every instance. The items below
// marked `cfg(test)` mirror its logic so that the cases shared with
// `names.spec.lua` pin both implementations down.

/// Longest encoded name that can be used for a directory. Folders with longer
/// names are stored as a single file instead, whose name can be truncated
/// safely because the instance name is stored inside the file.
#[cfg(test)]
const MAX_DIRECTORY_NAME_LEN: usize = 200;

/// Encoded unit names are truncated to this many bytes, leaving room for a
/// dedup suffix and the extension within the usual 255 byte limit.
#[cfg(test)]
const MAX_UNIT_STEM_LEN: usize = 200;

const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
    "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6",
    "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³",
];

fn must_escape(c: char) -> bool {
    matches!(
        c,
        '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '%' | '~'
    ) || c.is_control()
}

fn push_escaped(output: &mut String, c: char) {
    let mut buffer = [0; 4];
    for byte in c.encode_utf8(&mut buffer).bytes() {
        output.push_str(&format!("%{byte:02X}"));
    }
}

/// Windows refuses to create files named after devices, with or without an
/// extension, and it ignores trailing spaces when checking.
fn is_reserved(encoded: &str) -> bool {
    let base = encoded
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ');
    RESERVED_NAMES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(base))
}

/// Percent-encodes an instance name so that it is a valid file name on every
/// common filesystem. This is injective: different names never encode to the
/// same string, and [`decode_name`] reverses it.
///
/// The empty string encodes to the empty string, which is not a valid file
/// name. Callers have to handle empty names themselves.
pub fn encode_name(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut output = String::with_capacity(name.len());

    for (index, &c) in chars.iter().enumerate() {
        let is_first = index == 0;
        let is_last = index == chars.len() - 1;

        let escape = must_escape(c)
            || (is_first && (c == '.' || c == ' '))
            || (is_last && (c == '.' || c == ' '));

        if escape {
            push_escaped(&mut output, c);
        } else {
            output.push(c);
        }
    }

    if is_reserved(&output) {
        let first = output.chars().next().unwrap();
        let mut escaped = String::new();
        push_escaped(&mut escaped, first);
        output.replace_range(..first.len_utf8(), &escaped);
    }

    output
}

/// Reverses [`encode_name`]. Returns `None` if the given string could not have
/// been produced by it.
pub fn decode_name(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = encoded.get(index + 1..index + 3)?;
                output.push(u8::from_str_radix(hex, 16).ok()?);
                index += 3;
            }
            b'~' => return None,
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }

    let decoded = String::from_utf8(output).ok()?;

    // Reject anything that isn't in the canonical encoded form, like
    // lowercase hex or needlessly escaped characters, so that every file name
    // maps to exactly one instance name and back.
    if encode_name(&decoded) == encoded {
        Some(decoded)
    } else {
        None
    }
}

/// Splits a dedup suffix (`~2`) off of a file name, if there is one.
fn split_dedup_suffix(name: &str) -> (&str, Option<u32>) {
    if let Some((base, suffix)) = name.rsplit_once('~') {
        if !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(number) = suffix.parse::<u32>() {
                if number >= 2 && suffix == number.to_string() {
                    return (base, Some(number));
                }
            }
        }
    }

    (name, None)
}

/// Returns the instance name a directory with the given name represents.
pub fn folder_name_from_dir(dir_name: &str) -> Option<String> {
    let (base, _) = split_dedup_suffix(dir_name);
    let name = decode_name(base)?;

    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Whether a folder with the given name can be represented as a directory.
#[cfg(test)]
fn can_be_directory(name: &str) -> bool {
    !name.is_empty() && encode_name(name).len() <= MAX_DIRECTORY_NAME_LEN
}

/// Whether the given file name is one the asset store manages.
pub fn is_unit_file_name(file_name: &str) -> bool {
    !file_name.starts_with('.')
        && file_name.len() > UNIT_EXTENSION.len()
        && file_name.ends_with(UNIT_EXTENSION)
}

#[cfg(test)]
fn unit_stem(name: &str) -> String {
    let mut stem = encode_name(name);

    if stem.is_empty() {
        // Empty names can't be decoded from directory names, but units don't
        // need that: the instance's real name is stored inside the file.
        stem.push('_');
    }

    if stem.len() > MAX_UNIT_STEM_LEN {
        let mut end = MAX_UNIT_STEM_LEN;
        // Don't cut a character or an escape sequence in half. Escape
        // sequences are ASCII, so checking bytes is enough.
        while !stem.is_char_boundary(end) {
            end -= 1;
        }
        let tail_start = end.saturating_sub(2);
        if let Some(offset) = stem.as_bytes()[tail_start..end]
            .iter()
            .position(|&byte| byte == b'%')
        {
            end = tail_start + offset;
        }
        stem.truncate(end);
    }

    stem
}

/// Whether a single path component received from a client is safe to use on
/// disk. This is deliberately stricter than what filesystems allow, which also
/// rules out path traversal.
pub fn is_safe_component(component: &str) -> bool {
    !component.is_empty()
        && component.len() <= 255
        && !component.starts_with(['.', ' '])
        && !component.ends_with(['.', ' '])
        && !component
            .chars()
            .any(|c| must_escape(c) && c != '%' && c != '~')
        && !is_reserved(component)
}

/// Validates a path relative to an asset directory, as sent by a client. Every
/// directory component has to name a folder and the path has to end in a unit
/// file.
pub fn is_valid_unit_path(path: &str) -> bool {
    let mut components = path.split('/').peekable();

    while let Some(component) = components.next() {
        if !is_safe_component(component) {
            return false;
        }

        let is_last = components.peek().is_none();
        if is_last {
            return is_unit_file_name(component);
        } else if folder_name_from_dir(component).is_none() {
            return false;
        }
    }

    false
}

/// Validates a directory path relative to an asset directory, as sent by a
/// client.
pub fn is_valid_directory_path(path: &str) -> bool {
    path.split('/')
        .all(|component| is_safe_component(component) && folder_name_from_dir(component).is_some())
}

/// Kind of an entry that is being assigned a file name.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    /// A Folder stored as a directory.
    Directory,
    /// Any other instance, stored as a single file.
    Unit,
}

/// Assigns unique file names to a list of sibling entries, in sibling order.
#[cfg(test)]
#[derive(Debug, Default)]
struct SiblingNamer {
    used: std::collections::HashSet<Vec<u8>>,
}

#[cfg(test)]
impl SiblingNamer {
    fn new() -> Self {
        Self::default()
    }

    fn assign(&mut self, name: &str, kind: EntryKind) -> String {
        let stem = match kind {
            EntryKind::Directory => encode_name(name),
            EntryKind::Unit => unit_stem(name),
        };

        let with_suffix = |suffix: Option<u32>| {
            let suffix = suffix.map(|n| format!("~{n}")).unwrap_or_default();
            match kind {
                EntryKind::Directory => format!("{stem}{suffix}"),
                EntryKind::Unit => format!("{stem}{suffix}{UNIT_EXTENSION}"),
            }
        };

        let mut candidate = with_suffix(None);
        let mut number = 2;
        while !self.used.insert(collision_key(&candidate)) {
            candidate = with_suffix(Some(number));
            number += 1;
        }

        candidate
    }
}

/// Returns a key that is equal for any two file names that a case-insensitive
/// filesystem could consider the same file.
///
/// Folding the case of non-ASCII letters properly needs Unicode tables that
/// the plugin doesn't have, so every non-ASCII byte is treated as the same
/// character instead. That can only make names collide that wouldn't have,
/// which costs a `~2` suffix, never a file overwriting another.
fn collision_key(file_name: &str) -> Vec<u8> {
    file_name
        .bytes()
        .map(|byte| {
            if byte >= 0x80 {
                b'*'
            } else {
                byte.to_ascii_lowercase()
            }
        })
        .collect()
}

/// What decides where a single path component is sorted: the name without its
/// dedup suffix, ignoring case like dedup suffixes do, and the suffix's number.
fn sort_key(component: &str, is_file: bool) -> (Vec<u8>, u32) {
    let stem = if is_file {
        component.strip_suffix(UNIT_EXTENSION).unwrap_or(component)
    } else {
        component
    };
    let (base, number) = split_dedup_suffix(stem);
    (collision_key(base), number.unwrap_or(1))
}

/// Orders asset paths so that siblings that only differ by their dedup suffix
/// are in suffix order (`Tree`, `Tree~2`, ..., `Tree~10`). The plugin gives
/// siblings their suffixes in the order they're in, so instances created in
/// this order get the same file names again.
pub fn compare_paths(a: &str, b: &str) -> Ordering {
    let mut a_components = a.split('/').peekable();
    let mut b_components = b.split('/').peekable();

    loop {
        match (a_components.next(), b_components.next()) {
            (Some(a_component), Some(b_component)) => {
                let a_key = sort_key(a_component, a_components.peek().is_none());
                let b_key = sort_key(b_component, b_components.peek().is_none());
                let ordering = a_key.cmp(&b_key).then_with(|| a_component.cmp(b_component));

                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
}

/// Whether the asset path `path` is inside the directory `dir`. The empty path
/// is the asset directory itself.
pub fn is_under(path: &str, dir: &str) -> bool {
    dir.is_empty()
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Iterates over the keys of a map of asset paths that are the file `path`, or
/// are under the directory `path`. Keys that start with `path` are next to
/// each other in sort order, so this doesn't look at the rest of the map.
pub fn keys_at_or_under<'a, V>(
    map: &'a BTreeMap<String, V>,
    path: &'a str,
) -> impl Iterator<Item = &'a String> {
    map.range::<str, _>((Bound::Included(path), Bound::Unbounded))
        .map(|(key, _)| key)
        .take_while(move |key| key.starts_with(path))
        .filter(move |key| key.as_str() == path || is_under(key, path))
}

#[cfg(test)]
mod test {
    use super::*;

    // These cases are mirrored in plugin/src/AssetSync/names.spec.lua.
    const ENCODE_CASES: &[(&str, &str)] = &[
        ("Tree", "Tree"),
        ("Maps - Bounty", "Maps - Bounty"),
        ("a:b", "a%3Ab"),
        ("100%", "100%25"),
        ("a~b", "a%7Eb"),
        ("what?*", "what%3F%2A"),
        ("<tag>", "%3Ctag%3E"),
        ("back\\slash/fwd", "back%5Cslash%2Ffwd"),
        ("pipe|quote\"", "pipe%7Cquote%22"),
        (".hidden", "%2Ehidden"),
        ("trailing.", "trailing%2E"),
        (" padded ", "%20padded%20"),
        ("..", "%2E%2E"),
        (".", "%2E"),
        ("tab\there", "tab%09here"),
        ("CON", "%43ON"),
        ("con", "%63on"),
        ("nul.txt", "%6Eul.txt"),
        ("COM1", "%43OM1"),
        ("Lpt9", "%4Cpt9"),
        ("CONSOLE", "CONSOLE"),
        ("caf\u{e9}", "caf\u{e9}"),
        ("\u{1F332}", "\u{1F332}"),
        ("", ""),
    ];

    #[test]
    fn encode_cases() {
        for (name, encoded) in ENCODE_CASES {
            assert_eq!(&encode_name(name), encoded, "encoding {name:?}");
        }
    }

    #[test]
    fn decode_reverses_encode() {
        for (name, encoded) in ENCODE_CASES {
            assert_eq!(
                decode_name(encoded).as_deref(),
                Some(*name),
                "decoding {encoded:?}"
            );
        }
    }

    #[test]
    fn decode_rejects_non_canonical_names() {
        assert_eq!(decode_name("a%3ab"), None);
        assert_eq!(decode_name("%41"), None);
        assert_eq!(decode_name("bad%zz"), None);
        assert_eq!(decode_name("trunc%4"), None);
        assert_eq!(decode_name("a~2"), None);
        assert_eq!(decode_name("CON"), None);
    }

    #[test]
    fn folder_names_from_directories() {
        assert_eq!(
            folder_name_from_dir("Maps - Bounty").as_deref(),
            Some("Maps - Bounty")
        );
        assert_eq!(folder_name_from_dir("Spawns~2").as_deref(), Some("Spawns"));
        assert_eq!(folder_name_from_dir("a%3Ab~3").as_deref(), Some("a:b"));
        assert_eq!(folder_name_from_dir("v~1").as_deref(), None);
        assert_eq!(folder_name_from_dir("v~02").as_deref(), None);
        assert_eq!(folder_name_from_dir(""), None);
    }

    #[test]
    fn sibling_names_are_unique_ignoring_case() {
        let mut namer = SiblingNamer::new();
        let assigned: Vec<String> = [
            ("Part", EntryKind::Unit),
            ("Part", EntryKind::Unit),
            ("part", EntryKind::Unit),
            ("Part", EntryKind::Directory),
            ("Part.rbxm", EntryKind::Directory),
            ("", EntryKind::Unit),
            ("_", EntryKind::Unit),
            ("\u{c4}pfel", EntryKind::Unit),
            ("\u{e4}pfel", EntryKind::Unit),
            ("\u{d6}l", EntryKind::Unit),
        ]
        .iter()
        .map(|(name, kind)| namer.assign(name, *kind))
        .collect();

        assert_eq!(
            assigned,
            [
                "Part.rbxm",
                "Part~2.rbxm",
                "part~3.rbxm",
                "Part",
                "Part.rbxm~2",
                "_.rbxm",
                "_~2.rbxm",
                "\u{c4}pfel.rbxm",
                "\u{e4}pfel~2.rbxm",
                "\u{d6}l.rbxm",
            ]
        );
    }

    #[test]
    fn long_unit_names_are_truncated_safely() {
        let name = "%".repeat(100);
        let mut namer = SiblingNamer::new();
        let assigned = namer.assign(&name, EntryKind::Unit);

        assert!(assigned.len() <= MAX_UNIT_STEM_LEN + UNIT_EXTENSION.len());
        let stem = assigned.strip_suffix(UNIT_EXTENSION).unwrap();
        assert!(
            stem.len().is_multiple_of(3),
            "escape sequences must not be cut: {stem}"
        );

        assert!(!can_be_directory(&name));
        assert!(can_be_directory("Short"));
        assert!(!can_be_directory(""));
    }

    #[test]
    fn client_paths_are_validated() {
        for valid in [
            "Tree.rbxm",
            "Maps - Bounty/MarineFord/Boundary/Decoration.rbxm",
            "dir~2/Tree~3.rbxm",
            "a%3Ab/_.rbxm",
        ] {
            assert!(is_valid_unit_path(valid), "{valid} should be valid");
        }

        for invalid in [
            "",
            "../evil.rbxm",
            "a/../b.rbxm",
            "/abs.rbxm",
            "a\\b.rbxm",
            "C:/x.rbxm",
            "a/b",
            "CON.rbxm",
            ".folder.rbxm",
            "dir./x.rbxm",
            "bad~1/x.rbxm",
            "a//b.rbxm",
        ] {
            assert!(!is_valid_unit_path(invalid), "{invalid} should be invalid");
        }

        assert!(is_valid_directory_path("Maps - Bounty/MarineFord"));
        assert!(!is_valid_directory_path("Maps/../x"));
        assert!(!is_valid_directory_path(""));
    }

    #[test]
    fn paths_are_sorted_in_dedup_order() {
        let mut paths = vec![
            "Tree~10.rbxm",
            "Tree~2.rbxm",
            "tree~3.rbxm",
            "Tree.rbxm",
            "Spawns~10/A.rbxm",
            "Spawns~2/A.rbxm",
            "Spawns/B.rbxm",
            "Spawns/A.rbxm",
            "Apple.rbxm",
        ];
        paths.sort_by(|a, b| compare_paths(a, b));

        assert_eq!(
            paths,
            [
                "Apple.rbxm",
                "Spawns/A.rbxm",
                "Spawns/B.rbxm",
                "Spawns~2/A.rbxm",
                "Spawns~10/A.rbxm",
                "Tree.rbxm",
                "Tree~2.rbxm",
                "tree~3.rbxm",
                "Tree~10.rbxm",
            ]
        );
    }

    #[test]
    fn ranges_only_contain_the_given_path() {
        let map: BTreeMap<String, ()> = [
            "A.rbxm",
            "A/x.rbxm",
            "A/y/z.rbxm",
            "AB/x.rbxm",
            "A~2/x.rbxm",
            "B.rbxm",
        ]
        .into_iter()
        .map(|path| (path.to_owned(), ()))
        .collect();

        fn under<'a>(map: &'a BTreeMap<String, ()>, path: &'a str) -> Vec<&'a str> {
            keys_at_or_under(map, path).map(String::as_str).collect()
        }

        assert_eq!(under(&map, "A"), ["A/x.rbxm", "A/y/z.rbxm"]);
        assert_eq!(under(&map, "A.rbxm"), ["A.rbxm"]);
        assert_eq!(under(&map, "A/y"), ["A/y/z.rbxm"]);
        assert_eq!(under(&map, "C"), Vec::<&str>::new());
        assert_eq!(under(&map, "").len(), map.len());
    }

    #[test]
    fn unit_file_names() {
        assert!(is_unit_file_name("Tree.rbxm"));
        assert!(is_unit_file_name("_~2.rbxm"));
        assert!(!is_unit_file_name(".rbxm"));
        assert!(!is_unit_file_name(".Tree.rbxm.rojo-tmp"));
        assert!(!is_unit_file_name("Tree.rbxmx"));
        assert!(!is_unit_file_name("README.md"));
    }
}
