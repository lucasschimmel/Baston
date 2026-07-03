//! RPF2 packfile writer — the `resource.rpf` archive the FiveM client
//! downloads per resource.
//!
//! Byte-for-byte port of FXServer's `fi::PackfileBuilder`
//! (`components/citizen-server-impl/src/ResourceFilesComponent.cpp`):
//!
//! ```text
//! header : u32 magic 0x32465052 ("RPF2") | u32 tocSize | u32 numEntries
//!          | u32 0 | u32 0 | pad to 2048
//! toc    : 16-byte entries — root first, then per-directory child blocks in
//!          DFS pre-order, children sorted by name:
//!            dir : [u32 nameOff][u32 childCount][u32 firstChildIdx|0x80000000][u32 childCount]
//!            file: [u32 nameOff][u32 length][u32 dataOffset][u32 length]
//! names  : root is "/\0" (offset base); entry nameOff is relative to it
//! (pad to 2048; tocSize = offset here)
//! data   : file contents, each padded to 2048; dataOffset is absolute
//! ```

use std::collections::BTreeMap;

const RPF2_MAGIC: u32 = 0x3246_5052;
const ALIGNMENT: usize = 2048;
const DIRECTORY_FLAG: u32 = 0x8000_0000;

/// A file to pack: archive path (forward slashes) + contents.
pub struct PackfileInput {
    pub path: String,
    pub data: Vec<u8>,
}

#[derive(Default)]
struct DirNode {
    dirs: BTreeMap<String, DirNode>,
    files: BTreeMap<String, Vec<u8>>,
}

impl DirNode {
    fn insert(&mut self, path: &str, data: Vec<u8>) {
        match path.split_once('/') {
            Some((dir, rest)) if !rest.is_empty() => {
                self.dirs
                    .entry(dir.to_owned())
                    .or_default()
                    .insert(rest, data);
            }
            _ => {
                self.files.insert(path.to_owned(), data);
            }
        }
    }
}

/// Flattened TOC entry under construction.
enum TocEntry {
    Dir {
        name: String,
        child_count: u32,
        first_child: u32,
    },
    File {
        name: String,
        data: Vec<u8>,
    },
}

fn pad_to(buf: &mut Vec<u8>, alignment: usize) {
    let rem = buf.len() % alignment;
    if rem != 0 {
        buf.resize(buf.len() + alignment - rem, 0);
    }
}

/// Flatten the tree exactly like `Entry::Write` + `Entry::WriteSubEntries`:
/// root, then each directory's (name-sorted) children as one block, blocks in
/// DFS pre-order.
fn flatten(root: DirNode) -> Vec<TocEntry> {
    let mut entries = vec![TocEntry::Dir {
        name: String::new(),
        child_count: 0,
        first_child: 0,
    }];
    // Queue of (entry index, node) whose child block is pending. C++ recurses
    // "write my children, then each child's children" — DFS pre-order over
    // directories with the block itself contiguous.
    let mut pending: Vec<(usize, DirNode)> = vec![(0, root)];
    while let Some((dir_index, node)) = pending.pop() {
        let first_child = entries.len() as u32;
        // BTreeMap iterates name-sorted, matching the C++ std::sort by name;
        // dirs and files share one sorted namespace.
        let mut children: Vec<(String, Option<DirNode>, Option<Vec<u8>>)> = Vec::new();
        for (name, dir) in node.dirs {
            children.push((name, Some(dir), None));
        }
        for (name, data) in node.files {
            children.push((name, None, Some(data)));
        }
        children.sort_by(|a, b| a.0.cmp(&b.0));

        let child_count = children.len() as u32;
        if let TocEntry::Dir {
            child_count: count,
            first_child: first,
            ..
        } = &mut entries[dir_index]
        {
            *count = child_count;
            *first = first_child;
        }

        let mut sub_dirs = Vec::new();
        for (name, dir, data) in children {
            let index = entries.len();
            match (dir, data) {
                (Some(node), _) => {
                    entries.push(TocEntry::Dir {
                        name,
                        child_count: 0,
                        first_child: 0,
                    });
                    sub_dirs.push((index, node));
                }
                (None, Some(data)) => entries.push(TocEntry::File { name, data }),
                (None, None) => unreachable!("child is either dir or file"),
            }
        }
        // DFS pre-order: first (sorted) subdirectory's block comes next.
        pending.extend(sub_dirs.into_iter().rev());
    }
    entries
}

/// Build a `resource.rpf` archive from the given files.
pub fn build_rpf(files: Vec<PackfileInput>) -> Vec<u8> {
    let mut root = DirNode::default();
    for file in files {
        let path = file.path.replace('\\', "/");
        let path = path.trim_start_matches('/');
        if path.is_empty() {
            continue;
        }
        root.insert(path, file.data);
    }
    let entries = flatten(root);

    let mut buf = Vec::new();
    buf.extend_from_slice(&RPF2_MAGIC.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // tocSize (patched)
    buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    pad_to(&mut buf, ALIGNMENT);

    // TOC records; remember where each entry's patchable fields live.
    let toc_start = buf.len();
    for entry in &entries {
        match entry {
            TocEntry::Dir {
                child_count,
                first_child,
                ..
            } => {
                buf.extend_from_slice(&0u32.to_le_bytes()); // nameOff (patched)
                buf.extend_from_slice(&child_count.to_le_bytes());
                buf.extend_from_slice(&(first_child | DIRECTORY_FLAG).to_le_bytes());
                buf.extend_from_slice(&child_count.to_le_bytes());
            }
            TocEntry::File { .. } => {
                buf.extend_from_slice(&0u32.to_le_bytes()); // nameOff (patched)
                buf.extend_from_slice(&0u32.to_le_bytes()); // length (patched)
                buf.extend_from_slice(&0u32.to_le_bytes()); // dataOff (patched)
                buf.extend_from_slice(&0u32.to_le_bytes()); // length2 (patched)
            }
        }
    }

    // Names: root "/\0" first; offsets relative to the names base.
    let names_base = buf.len();
    buf.push(b'/');
    buf.push(0);
    for (i, entry) in entries.iter().enumerate() {
        let record = toc_start + i * 16;
        let name = match entry {
            TocEntry::Dir { name, .. } => name,
            TocEntry::File { name, .. } => name,
        };
        let name_off = if i == 0 {
            0
        } else {
            (buf.len() - names_base) as u32
        };
        buf[record..record + 4].copy_from_slice(&name_off.to_le_bytes());
        if i != 0 {
            buf.extend_from_slice(name.as_bytes());
            buf.push(0);
        }
    }

    pad_to(&mut buf, ALIGNMENT);
    let toc_size = buf.len() as u32;
    buf[4..8].copy_from_slice(&toc_size.to_le_bytes());

    // File data, 2048-aligned, absolute offsets.
    for (i, entry) in entries.iter().enumerate() {
        if let TocEntry::File { data, .. } = entry {
            let record = toc_start + i * 16;
            let offset = buf.len() as u32;
            let len = data.len() as u32;
            buf[record + 4..record + 8].copy_from_slice(&len.to_le_bytes());
            buf[record + 8..record + 12].copy_from_slice(&offset.to_le_bytes());
            buf[record + 12..record + 16].copy_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(data);
            pad_to(&mut buf, ALIGNMENT);
        }
    }

    buf
}

/// Generate the `fxmanifest.lua` the FiveM client reads from the packfile,
/// from BASTON's `manifest.json` (FXServer always packs the manifest file —
/// `GetFilesForSet` adds `fxmanifest.lua`).
pub fn generate_fxmanifest(manifest: &baston_protocol::ResourceManifest) -> String {
    fn lua_list(items: &[String]) -> String {
        items
            .iter()
            .map(|i| format!("    '{}'", i.replace('\\', "/")))
            .collect::<Vec<_>>()
            .join(",\n")
    }

    let mut out = String::from("-- generated by BASTON from manifest.json\n");
    out.push_str("fx_version 'cerulean'\ngame 'gta5'\n\n");
    if !manifest.client_scripts.is_empty() {
        out.push_str(&format!(
            "client_scripts {{\n{}\n}}\n",
            lua_list(&manifest.client_scripts)
        ));
    }
    if !manifest.files.is_empty() {
        out.push_str(&format!("files {{\n{}\n}}\n", lua_list(&manifest.files)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(buf: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
    }

    #[test]
    fn header_and_alignment() {
        let rpf = build_rpf(vec![PackfileInput {
            path: "fxmanifest.lua".into(),
            data: b"fx_version 'cerulean'".to_vec(),
        }]);
        assert_eq!(u32_at(&rpf, 0), RPF2_MAGIC);
        let toc_size = u32_at(&rpf, 4) as usize;
        assert_eq!(toc_size % ALIGNMENT, 0);
        assert_eq!(u32_at(&rpf, 8), 2, "root + 1 file");
        assert_eq!(rpf.len() % ALIGNMENT, 0);
    }

    #[test]
    fn nested_structure_and_offsets() {
        let rpf = build_rpf(vec![
            PackfileInput {
                path: "dist/client/index.js".into(),
                data: b"console.log('hi')".to_vec(),
            },
            PackfileInput {
                path: "fxmanifest.lua".into(),
                data: b"fx_version 'cerulean'".to_vec(),
            },
        ]);
        // entries: 0 root(dir), block(root): 1 dist(dir), 2 fxmanifest.lua(file),
        // block(dist): 3 client(dir), block(client): 4 index.js(file)
        assert_eq!(u32_at(&rpf, 8), 5);
        let toc = ALIGNMENT;

        // root: 2 children starting at entry 1, dir flag set
        assert_eq!(u32_at(&rpf, toc + 4), 2);
        assert_eq!(u32_at(&rpf, toc + 8), 1 | DIRECTORY_FLAG);

        // entry 1 = "dist" (sorted before fxmanifest.lua): dir with 1 child at index 3
        assert_eq!(u32_at(&rpf, toc + 16 + 4), 1);
        assert_eq!(u32_at(&rpf, toc + 16 + 8), 3 | DIRECTORY_FLAG);

        // entry 2 = fxmanifest.lua: file, correct length and readable data
        let len = u32_at(&rpf, toc + 32 + 4) as usize;
        let off = u32_at(&rpf, toc + 32 + 8) as usize;
        assert_eq!(len, "fx_version 'cerulean'".len());
        assert_eq!(&rpf[off..off + len], b"fx_version 'cerulean'");
        assert_eq!(off % ALIGNMENT, 0);

        // entry 4 = index.js under dist/client
        let len = u32_at(&rpf, toc + 64 + 4) as usize;
        let off = u32_at(&rpf, toc + 64 + 8) as usize;
        assert_eq!(&rpf[off..off + len], b"console.log('hi')");
    }

    #[test]
    fn name_table_matches_entries() {
        let rpf = build_rpf(vec![PackfileInput {
            path: "a.js".into(),
            data: vec![1],
        }]);
        let toc = ALIGNMENT;
        let names_base = toc + 2 * 16;
        // root name is "/"
        assert_eq!(u32_at(&rpf, toc), 0);
        assert_eq!(rpf[names_base], b'/');
        // file name offset points at "a.js\0"
        let name_off = u32_at(&rpf, toc + 16) as usize;
        assert_eq!(
            &rpf[names_base + name_off..names_base + name_off + 5],
            b"a.js\0"
        );
    }

    #[test]
    fn fxmanifest_generation() {
        let manifest = baston_protocol::ResourceManifest {
            name: "axiom-core".into(),
            version: None,
            dependencies: vec![],
            server_scripts: vec!["dist/server/index.js".into()],
            client_scripts: vec!["dist/client/index.js".into()],
            files: vec!["dist/client/index.js".into()],
        };
        let lua = generate_fxmanifest(&manifest);
        assert!(lua.contains("fx_version 'cerulean'"));
        assert!(lua.contains("client_scripts"));
        assert!(lua.contains("'dist/client/index.js'"));
        assert!(
            !lua.contains("server_scripts"),
            "server scripts stay server-side"
        );
    }
}
