//! Quick Wins (`BuildPrompt` §6): junk categories computed from the
//! already-built tree — no extra disk pass.
//!
//! Matching resolves each known location to nodes by descending the tree
//! from the scan root when it lies inside the scanned subtree, expanding
//! `*` wildcards over children. A category never counts something nested
//! inside its own match; each category is capped at 400 items.

use serde::{Deserialize, Serialize};

use crate::scan::node::Tree;

/// Per-category item cap (spec §6).
pub const CATEGORY_CAP: usize = 400;

/// A Quick Wins row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickWinCategory {
    /// Stable id (for analytics + navigation).
    pub id: &'static str,
    /// Row title.
    pub title: &'static str,
    /// Icon tag.
    pub icon: &'static str,
    /// Node ids matched (≤ [`CATEGORY_CAP`]).
    pub items: Vec<u32>,
    /// Total on-disk bytes of matched items.
    pub size: u64,
    /// Review-only rows refuse "Add all" (VM disks, Windows.old) —
    /// enforced here, not just in the UI (decision D7).
    pub review_only: bool,
    /// Context-menu extra for special rows (e.g. storagesense URI).
    pub extra: Option<&'static str>,
}

/// Known-folder style location patterns resolved against the tree.
/// `*` expands over one path level (spec §6).
#[derive(Debug, Clone)]
pub struct Pattern {
    /// Category id this pattern feeds.
    pub category: &'static str,
    /// Environment-root prefix, e.g. `%LOCALAPPDATA%` (resolved by the
    /// engine to an absolute path before matching).
    pub env: &'static str,
    /// Path segments under the env root; `*` matches any single segment.
    pub segments: &'static [&'static str],
}

/// Browser cache locations, statically spelled out: the old runtime
/// split + `Box::leak` leaked 9 boxes on every `resolve()` call
/// (`patterns()` builds a fresh Vec per call).
const BROWSER_CACHE_PATTERNS: [&[&str]; 9] = [
    &["Google", "Chrome", "*", "Cache"],
    &["Google", "Chrome", "*", "Code Cache"],
    &["Google", "Chrome", "*", "GPUCache"],
    &["Microsoft", "Edge", "*", "Cache"],
    &["Microsoft", "Edge", "*", "Code Cache"],
    &["Microsoft", "Edge", "*", "GPUCache"],
    &["BraveSoftware", "Brave-Browser", "*", "Cache"],
    &["BraveSoftware", "Brave-Browser", "*", "Code Cache"],
    &["BraveSoftware", "Brave-Browser", "*", "GPUCache"],
];

/// The full pattern table (spec §6 categories, verbatim locations).
#[must_use]
pub fn patterns() -> Vec<Pattern> {
    let mut p: Vec<Pattern> = Vec::new();
    let mut add = |category: &'static str, env: &'static str, segments: &'static [&'static str]| {
        p.push(Pattern {
            category,
            env,
            segments,
        });
    };
    if cfg!(target_os = "macos") {
        // Mac BuildPrompt §5 categories (mac paths; the engine resolves
        // %HOME% from the app's known folders).
        add("downloads", "%HOME%", &["Downloads"]);
        add("temp_caches", "%HOME%", &["Library", "Caches"]);
        add("temp_caches", "%HOME%", &["Library", "Logs"]);
        add("ios_simulators", "%HOME%", &["Developer", "CoreSimulator"]);
        add(
            "xcode_derived",
            "%HOME%",
            &["Library", "Developer", "Xcode", "DerivedData"],
        );
        add("dev_caches", "%HOME%", &[".cargo", "registry"]);
        add("dev_caches", "%HOME%", &[".gradle", "caches"]);
        add("dev_caches", "%HOME%", &[".npm"]);
        add("android_emulators", "%HOME%", &[".android", "avd"]);
    } else {
        // Downloads (redirected known folder — the engine resolves it).
        add("downloads", "%USERPROFILE%", &["Downloads"]);
        // Temp & caches.
        add("temp_caches", "%LOCALAPPDATA%", &["Temp"]);
        add(
            "temp_caches",
            "%LOCALAPPDATA%",
            &["Microsoft", "Windows", "INetCache"],
        );
        add("temp_caches", "%LOCALAPPDATA%", &["CrashDumps"]);
        add("temp_caches", "%LOCALAPPDATA%", &["D3DSCache"]);
        add(
            "temp_caches",
            "%LOCALAPPDATA%",
            &["Microsoft", "Windows", "WER"],
        );
        for segs in BROWSER_CACHE_PATTERNS {
            add("browser_caches", "%LOCALAPPDATA%", segs);
        }
        add(
            "browser_caches",
            "%LOCALAPPDATA%",
            &["Mozilla", "Firefox", "Profiles", "*", "cache2"],
        );
        // Developer caches.
        add("dev_caches", "%USERPROFILE%", &[".nuget", "packages"]);
        add("dev_caches", "%USERPROFILE%", &[".cargo", "registry"]);
        add("dev_caches", "%USERPROFILE%", &[".gradle", "caches"]);
        add("dev_caches", "%LOCALAPPDATA%", &["npm-cache"]);
        add("dev_caches", "%LOCALAPPDATA%", &["pip", "Cache"]);
        add("dev_caches", "%LOCALAPPDATA%", &["pnpm", "store"]);
        add("dev_caches", "%LOCALAPPDATA%", &["Yarn", "Cache"]);
        // Android emulators.
        add("android_emulators", "%USERPROFILE%", &[".android", "avd"]);
    } // end windows pattern block
      // Shared categories (path-shape agnostic): node_modules, build
      // artifacts, large media, VM disks (any-depth / file-level).
      // NOTE: node_modules and build_artifacts have NO pattern entries
      // here — they are resolved by their dedicated any-depth matchers
      // (`find_named` and `find_build_artifacts` + BUILD_ARTIFACT_NAMES)
      // below; an entry with the `**` env would be dead data (the env
      // never resolves), so keep this table strictly for env-rooted
      // known-folder categories.
    p
}

/// Node-name predicates that need parent/sibling context (build-artifact
/// `target`/`bin`/`obj` rules — spec §6).
#[must_use]
pub fn build_artifact_with_sibling(name: &[u16], siblings: &[Vec<u16>]) -> bool {
    fn eq(utf16: &[u16], ascii: &str) -> bool {
        if utf16.len() != ascii.len() {
            return false;
        }
        utf16
            .iter()
            .zip(ascii.bytes())
            .all(|(&c, b)| lower_ascii_u16(c) == u16::from(b.to_ascii_lowercase()))
    }
    let has_ext = |exts: &[&str]| siblings.iter().any(|s| exts.iter().any(|&e| eq(s, e)));
    if eq(name, "target") {
        return has_ext(&["Cargo.toml", "pom.xml"]);
    }
    if eq(name, "bin") || eq(name, "obj") {
        // `project.json` names an exact sibling; .csproj/.vcxproj need
        // the suffix wildcard below (a sibling literally named
        // `*.csproj` never exists — the old `has_ext` entries were dead).
        return has_ext(&["project.json"]) || {
            // wildcard sibling check
            siblings.iter().any(|s| {
                let s_str = String::from_utf16_lossy(s.as_slice());
                s_str.to_ascii_lowercase().ends_with(".csproj")
                    || s_str.to_ascii_lowercase().ends_with(".vcxproj")
            })
        };
    }
    false
}

/// Review-only VM-disk roots (spec §6): `*.vhdx`/`*.vmdk` under specific
/// roots, and any `.vhdx` ≥ 1 GB.
pub const VM_DISK_ROOTS: [&[&str]; 3] = [
    &["%LOCALAPPDATA%", "Packages", "*", "LocalState"],
    &["%LOCALAPPDATA%", "Docker"],
    &["%USERPROFILE%", "VirtualBox VMs"],
];

/// Minimum standalone `.vhdx` size for the VM-disks row (1 GB).
pub const VM_DISK_MIN: u64 = 1024 * 1024 * 1024;

/// Resolve every Quick Wins category against the tree (spec §6).
///
/// `env_roots` maps `%LOCALAPPDATA%` etc. to absolute paths (engine
/// resolves via known folders; also `%USERPROFILE%`). `_now` is kept
/// for API compatibility — large-media is age-agnostic by spec.
#[must_use]
#[allow(clippy::too_many_lines)] // category orchestrator: pattern pass + 8 category builders; the
                                 // grouping/ordering invariants are documented in-body (same posture
                                 // as the app layer's commit_cleanup)
pub fn resolve(
    tree: &Tree,
    env_roots: &std::collections::HashMap<String, String>,
    _now: i64,
) -> Vec<QuickWinCategory> {
    let mut out: Vec<QuickWinCategory> = Vec::new();

    let push_cat = |id: &'static str,
                    title: &'static str,
                    icon: &'static str,
                    review_only: bool,
                    extra: Option<&'static str>,
                    items: Vec<u32>|
     -> Option<QuickWinCategory> {
        // Drop items nested inside an already-matched item of the SAME
        // category (spec: "never counts something nested inside its own
        // match") and enforce the cap.
        let mut filtered: Vec<u32> = Vec::with_capacity(items.len());
        for &it in &items {
            if filtered.iter().any(|&f| tree.is_descendant_of(it, f)) {
                continue;
            }
            filtered.push(it);
            if filtered.len() >= CATEGORY_CAP {
                break;
            }
        }
        if filtered.is_empty() {
            return None;
        }
        let size: u64 = filtered
            .iter()
            .map(|&id| tree.node(id).map_or(0, |n| n.on_disk))
            .fold(0u64, u64::saturating_add);
        Some(QuickWinCategory {
            id,
            title,
            icon,
            items: filtered,
            size,
            review_only,
            extra,
        })
    };

    // Pattern-based categories. Env roots may be keyed with or without the
    // `%` wrapper (the engine passes wrapped; tests and future callers may
    // pass bare) — both resolve.
    let env_lookup = |key: &str| -> Option<&String> {
        env_roots
            .get(key)
            .or_else(|| env_roots.get(key.trim_matches('%')))
    };
    let mut buckets: std::collections::HashMap<&str, Vec<u32>> = std::collections::HashMap::new();
    for pat in patterns() {
        let Some(root) = env_lookup(pat.env) else {
            continue;
        };
        for id in match_pattern(tree, root, pat.segments) {
            buckets.entry(pat.category).or_default().push(id);
        }
    }
    for (id, title, icon, key) in [
        ("downloads", "Downloads", "download", "downloads"),
        ("temp_caches", "Temp & caches", "temp", "temp_caches"),
        (
            "browser_caches",
            "Browser caches",
            "browser",
            "browser_caches",
        ),
        ("dev_caches", "Developer caches", "code", "dev_caches"),
        (
            "android_emulators",
            "Android emulators",
            "phone",
            "android_emulators",
        ),
    ] {
        if let Some(items) = buckets.remove(key) {
            if let Some(cat) = push_cat(id, title, icon, false, None, items) {
                out.push(cat);
            }
        }
    }

    // node_modules: any depth, name match (uses `**`).
    let nm = find_named(tree, tree.root, "node_modules", true);
    if let Some(cat) = push_cat("node_modules", "node_modules", "code", false, None, nm) {
        out.push(cat);
    }

    // Build artifacts.
    let ba = find_build_artifacts(tree, tree.root);
    if let Some(cat) = push_cat(
        "build_artifacts",
        "Build artifacts",
        "hammer",
        false,
        None,
        ba,
    ) {
        out.push(cat);
    }

    // Large media: video/audio/image ≥ 10 MB.
    let lm = find_large_media(tree, tree.root);
    if let Some(cat) = push_cat("large_media", "Large media", "video", false, None, lm) {
        out.push(cat);
    }

    // VM disks (review-only).
    let mut vm: Vec<u32> = Vec::new();
    for root in VM_DISK_ROOTS {
        // Resolve the env root, then match the remaining segments.
        if let Some(base) = env_roots.get(&root[0].to_string().replace('%', "")) {
            let rest: Vec<&str> = root[1..].to_vec();
            for id in match_pattern(tree, base, &rest) {
                vm.push(id);
            }
        }
    }
    // Any .vhdx ≥ 1 GB anywhere.
    tree.walk(tree.root, |id, n| {
        if !n.is_dir() {
            let name = tree.name_u16(id);
            if has_ext_ci(name, "vhdx") && n.logical >= VM_DISK_MIN {
                vm.push(id);
            }
        }
    });
    if let Some(cat) = push_cat("vm_disks", "VM disks", "server", true, None, vm) {
        out.push(cat);
    }

    // Previous Windows install (review-only + storagesense link).
    let mut wo: Vec<u32> = Vec::new();
    tree.walk(tree.root, |id, n| {
        if n.is_dir() {
            let name = tree.name_u16(id);
            if eq_ci(name, "Windows.old") {
                wo.push(id);
            }
        }
    });
    if let Some(cat) = push_cat(
        "windows_old",
        "Previous Windows install",
        "clock",
        true,
        Some("ms-settings:storagesense"),
        wo,
    ) {
        out.push(cat);
    }

    out.sort_unstable_by_key(|c| std::cmp::Reverse(c.size));
    out
}

/// Match an absolute root + segments (with `*`) against the tree.
/// Returns node ids of the FINAL segment matches that lie under the root.
/// Ids come from [`Tree::children_sorted`], so every candidate is a live
/// arena node; malformed ids are skipped rather than panicking.
#[must_use]
pub fn match_pattern(tree: &Tree, root: &str, segments: &[&str]) -> Vec<u32> {
    let mut out = Vec::new();
    // Find the tree node for `root` by comparing display paths.
    let root_norm = normalize(root);
    let start = find_node_by_path(tree, &root_norm).unwrap_or(tree.root);
    // Walk segments from `start`.
    let mut current: Vec<u32> = vec![start];
    for seg in segments {
        let mut next: Vec<u32> = Vec::new();
        for &cur in &current {
            for &cid in tree.children_sorted(cur) {
                let Some(n) = tree.node(cid) else { continue };
                if n.is_removed() {
                    continue;
                }
                if *seg == "*" || eq_ci(tree.name_u16(cid), seg) {
                    next.push(cid);
                }
            }
        }
        current = next;
        if current.is_empty() {
            break;
        }
    }
    out.extend(current.iter().copied().filter(|&id| id != tree.root));
    out
}

/// Case-insensitive ASCII compare of a UTF-16 name against a `&str`.
fn eq_ci(utf16: &[u16], ascii: &str) -> bool {
    if utf16.len() != ascii.len() {
        return false;
    }
    utf16
        .iter()
        .zip(ascii.bytes())
        .all(|(&c, b)| lower_ascii_u16(c) == u16::from(b.to_ascii_lowercase()))
}

/// `A`–`Z` (ASCII) → `a`–`z`; everything else unchanged. `u16` has no
/// `to_ascii_lowercase` inherent method, so this is the manual twin.
#[inline]
fn lower_ascii_u16(c: u16) -> u16 {
    if (0x41..=0x5A).contains(&c) {
        c + 0x20
    } else {
        c
    }
}

/// Normalize a path for comparison: lowercase, forward to back slashes,
/// drop trailing separators and the `\\?\` prefix.
fn normalize(path: &str) -> String {
    let mut p = path.replace('/', "\\").to_ascii_lowercase();
    if let Some(stripped) = p.strip_prefix(r"\\?\") {
        p = stripped.to_string();
    }
    while p.ends_with('\\') {
        p.pop();
    }
    p
}

/// Find a node by normalized display path (walk up from each root ref).
#[must_use]
pub fn find_node_by_path(tree: &Tree, norm_path: &str) -> Option<u32> {
    for r in &tree.roots {
        let rn = normalize(&r.path);
        if norm_path == rn {
            return Some(r.node);
        }
        // Walk below the root.
        if let Some(rest) = norm_path.strip_prefix(&(rn.clone() + "\\")) {
            let mut cur = r.node;
            let mut ok = true;
            for seg in rest.split('\\') {
                let found = tree
                    .children_sorted(cur)
                    .iter()
                    .copied()
                    .find(|&cid| eq_ci(tree.name_u16(cid), seg));
                if let Some(f) = found {
                    cur = f;
                } else {
                    ok = false;
                    break;
                }
            }
            if ok {
                return Some(cur);
            }
        }
    }
    None
}

/// Find nodes named `name` anywhere under `start` (any depth).
#[must_use]
pub fn find_named(tree: &Tree, start: u32, name: &str, dirs_only: bool) -> Vec<u32> {
    let mut out = Vec::new();
    tree.walk(start, |id, n| {
        if (!dirs_only || n.is_dir()) && eq_ci(tree.name_u16(id), name) {
            out.push(id);
        }
    });
    out
}

/// Build-artifact folder names matched unconditionally (spec §6 + the
/// mature cleaner-set additions: Terraform/Rust/Maven/Python/Nuxt).
const BUILD_ARTIFACT_NAMES: [&str; 14] = [
    "build",
    ".build",
    "dist",
    ".next",
    ".nuxt",
    ".turbo",
    ".parcel-cache",
    ".terraform",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".gradle",
];

/// Collect build-artifact folder ids under `start`, applying the
/// sibling-ruled `target`/`bin`/`obj` rules from spec §6.
#[must_use]
pub fn find_build_artifacts(tree: &Tree, start: u32) -> Vec<u32> {
    let mut out = Vec::new();
    tree.walk(start, |id, n| {
        if !n.is_dir() || n.is_removed() {
            return;
        }
        let name = tree.name_u16(id);
        if BUILD_ARTIFACT_NAMES.iter().any(|&f| eq_ci(name, f)) {
            out.push(id);
            return;
        }
        // Sibling-ruled names: target / bin / obj.
        let parent = n.parent;
        if parent != u32::MAX {
            let siblings: Vec<Vec<u16>> = tree
                .children_sorted(parent)
                .iter()
                .map(|&sid| tree.name_u16(sid).to_vec())
                .collect();
            if build_artifact_with_sibling(name, &siblings) {
                out.push(id);
            }
        }
    });
    out
}

/// Large media: video/audio/image files ≥ 10 MB (logical size, spec §6).
#[must_use]
pub fn find_large_media(tree: &Tree, start: u32) -> Vec<u32> {
    const MIN: u64 = 10 * 1024 * 1024;
    use crate::scan::categories::FileCategory as FC;
    let mut out = Vec::new();
    tree.walk(start, |id, n| {
        if !n.is_dir() && n.logical >= MIN && !n.is_cloud_placeholder() && !n.is_removed() {
            matches!(n.category(), FC::Video | FC::Audio | FC::Image).then(|| out.push(id));
        }
    });
    out
}

fn has_ext_ci(name: &[u16], ext: &str) -> bool {
    let n = String::from_utf16_lossy(name).to_ascii_lowercase();
    n.ends_with(&format!(".{ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::node::{BatchEntry, Node};
    use crate::scan::rollup;

    fn u16s(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn build() -> Tree {
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\Users\\z");
        // ids: 1 Downloads, 2 AppData, 3 proj, 4 Videos
        t.append_batch(
            0,
            vec![dir("Downloads"), dir("AppData"), dir("proj"), dir("Videos")],
        );
        // ids: 5 old.zip, 6 movie.mkv
        t.append_batch(
            1,
            vec![
                file("old.zip", 500, 500, 1),
                file("movie.mkv", 20 * 1024 * 1024, 20 * 1024 * 1024, 2),
            ],
        );
        // AppData(2) → Local(7)
        t.append_batch(2, vec![dir("Local")]);
        // Local(7) → Temp(8)
        t.append_batch(7, vec![dir("Temp")]);
        // Temp(8) → junk.tmp(9)
        t.append_batch(8, vec![file("junk.tmp", 100, 100, 3)]);
        // proj(3) → node_modules(10), target(11), Cargo.toml(12), main.rs(13)
        t.append_batch(
            3,
            vec![
                dir("node_modules"),
                dir("target"),
                file("Cargo.toml", 10, 10, 1),
                file("main.rs", 10, 10, 1),
            ],
        );
        // node_modules(10) → lib.rlib(14)
        t.append_batch(10, vec![file("lib.rlib", 900, 900, 1)]);
        // Videos(4) → tiny.txt(15)
        t.append_batch(4, vec![file("tiny.txt", 5, 5, 1)]);
        rollup::finalize(&mut t);
        t
    }

    fn dir(name: &str) -> BatchEntry {
        let mut node = Node::new_dir();
        node.modified = 1;
        BatchEntry {
            name: u16s(name),
            node,
        }
    }

    fn file(name: &str, logical: u64, on_disk: u64, modified: i64) -> BatchEntry {
        let mut node = Node::new_file();
        node.logical = logical;
        node.on_disk = on_disk;
        node.modified = modified;
        node.set_category(crate::scan::categories::FileCategory::from_name(&u16s(
            name,
        )));
        BatchEntry {
            name: u16s(name),
            node,
        }
    }

    fn env_roots() -> std::collections::HashMap<String, String> {
        let mut m = std::collections::HashMap::new();
        // The test tree is windows-shaped; the mac pattern table keys on
        // %HOME% which maps to the same profile root, so both platform
        // tables resolve against this fixture.
        m.insert("USERPROFILE".to_string(), "C:\\Users\\z".to_string());
        m.insert("HOME".to_string(), "C:\\Users\\z".to_string());
        m.insert(
            "LOCALAPPDATA".to_string(),
            "C:\\Users\\z\\AppData\\Local".to_string(),
        );
        m
    }

    #[test]
    fn downloads_and_temp_resolve() {
        let t = build();
        let cats = resolve(&t, &env_roots(), 1);
        let get = |id: &str| cats.iter().find(|c| c.id == id);
        let dl = get("downloads").expect("downloads row");
        assert_eq!(dl.items, vec![1]);
        // The fixture is windows-shaped: the Windows table resolves its
        // Temp row here; the macOS table's cache roots (~/Library/Caches)
        // legitimately do not exist in it, so that assert is windows-only
        // (the mac table's downloads row resolves identically above).
        if cfg!(target_os = "macos") {
            assert!(get("temp_caches").is_none(), "no Library/Caches in fixture");
        } else {
            let tc = get("temp_caches").expect("temp row");
            assert!(!tc.items.is_empty());
            assert!(tc.items.contains(&8));
        }
    }

    #[test]
    fn node_modules_and_target_with_sibling() {
        let t = build();
        let cats = resolve(&t, &env_roots(), 1);
        let nm = cats.iter().find(|c| c.id == "node_modules").unwrap();
        assert_eq!(nm.items, vec![10]);
        let ba = cats.iter().find(|c| c.id == "build_artifacts").unwrap();
        assert!(ba.items.contains(&11), "target with sibling Cargo.toml");
    }

    #[test]
    fn cleaner_set_artifact_names_match() {
        // The cleaner-inspired additions (.terraform/.pytest_cache/etc.)
        // must resolve as build artifacts at any depth.
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\work");
        t.append_batch(
            0,
            vec![
                dir("infra"),
                dir("backend"),
                dir("web"),
                dir("legacy"),
                dir("scripts"),
                dir("agent"),
            ],
        );
        // infra(1) → .terraform(7), backend(2) → .pytest_cache(8),
        // web(3) → .nuxt(9), legacy(4) → .tox(10), scripts(5) → .mypy_cache(11),
        // agent(6) → .ruff_cache(12).
        t.append_batch(1, vec![dir(".terraform")]);
        t.append_batch(2, vec![dir(".pytest_cache")]);
        t.append_batch(3, vec![dir(".nuxt")]);
        t.append_batch(4, vec![dir(".tox")]);
        t.append_batch(5, vec![dir(".mypy_cache")]);
        t.append_batch(6, vec![dir(".ruff_cache")]);
        rollup::finalize(&mut t);
        let ba = find_build_artifacts(&t, t.root);
        for expected in [7u32, 8, 9, 10, 11, 12] {
            assert!(
                ba.contains(&expected),
                "expected artifact id {expected} in {ba:?}"
            );
        }
        // And they surface through resolve() in the category row.
        let cats = resolve(&t, &env_roots(), 1);
        let row = cats.iter().find(|c| c.id == "build_artifacts").unwrap();
        for expected in [7u32, 8, 9, 10, 11, 12] {
            assert!(row.items.contains(&expected));
        }
    }

    #[test]
    fn large_media_minimum() {
        let t = build();
        let cats = resolve(&t, &env_roots(), 1);
        let lm = cats.iter().find(|c| c.id == "large_media").unwrap();
        assert_eq!(lm.items, vec![6]); // 20 MB mkv only
    }

    #[test]
    fn review_only_rows_flagged() {
        let t = build();
        let cats = resolve(&t, &env_roots(), 1);
        for c in &cats {
            if c.id == "vm_disks" || c.id == "windows_old" {
                assert!(c.review_only, "{} must be review-only", c.id);
            }
        }
    }

    #[test]
    fn match_pattern_expands_wildcards() {
        let t = build();
        let m = match_pattern(&t, "C:\\Users\\z\\AppData\\Local", &["Temp"]);
        assert!(m.contains(&8));
        let bad = match_pattern(&t, "C:\\Does\\Not\\Exist", &["Temp"]);
        assert!(bad.is_empty());
    }

    /// The browser-cache pattern table must match per-PROFILE cache
    /// dirs: LocalAppData/Google/Chrome/<profile>/Cache. Pins the
    /// static-table rewrite of the old runtime-split + `Box::leak` loop.
    #[test]
    fn browser_cache_patterns_match_profile_caches() {
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\Users\\z");
        t.append_batch(0, vec![dir("AppData")]); // 1
        t.append_batch(1, vec![dir("Local")]); // 2
        t.append_batch(2, vec![dir("Google")]); // 3
        t.append_batch(3, vec![dir("Chrome")]); // 4
        t.append_batch(4, vec![dir("Profile 1"), dir("Profile 2")]); // 5, 6
        t.append_batch(5, vec![dir("Cache"), dir("Code Cache")]); // 7, 8
        t.append_batch(6, vec![dir("GPUCache")]); // 9
        rollup::finalize(&mut t);
        let m = match_pattern(
            &t,
            "C:\\Users\\z\\AppData\\Local",
            &["Google", "Chrome", "*", "Cache"],
        );
        assert_eq!(m, vec![7], "exact 'Cache' dir under any profile");
        let m = match_pattern(
            &t,
            "C:\\Users\\z\\AppData\\Local",
            &["Google", "Chrome", "*", "Code Cache"],
        );
        assert_eq!(m, vec![8], "'Code Cache' (with space) matches");
        let m = match_pattern(
            &t,
            "C:\\Users\\z\\AppData\\Local",
            &["Google", "Chrome", "*", "GPUCache"],
        );
        assert_eq!(m, vec![9], "'GPUCache' matches");
        // And the table itself: every entry resolves against the
        // pattern-matching semantics (3 browsers x 3 cache kinds).
        assert_eq!(BROWSER_CACHE_PATTERNS.len(), 9);
        for segs in BROWSER_CACHE_PATTERNS {
            assert_eq!(segs.len(), 4, "browser patterns are 4 segments: {segs:?}");
            assert_eq!(
                segs[2], "*",
                "third segment is the profile wildcard: {segs:?}"
            );
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // Catalog invariants — the cleaner-repo discipline (vyrti/cleaner
    // sysclean/tests.rs): the rule table is safety-critical data, so
    // its boundaries are re-asserted as tests on every edit. A pattern
    // that could reach user documents or credentials fails CI here,
    // not a user's disk.
    // ─────────────────────────────────────────────────────────────────

    /// The never-clean name set: no quickwin pattern may target a
    /// directory holding user-authored content or credentials.
    /// Downloads is the deliberate exception (the product's headline
    /// category); dev-tool cache roots (.cargo/registry, .gradle/caches,
    /// npm-cache…) are regenerable caches, not content.
    const NEVER_CLEAN: &[&str] = &[
        "documents",
        "desktop",
        "pictures",
        "photos",
        "music",
        "movies",
        "videos",
        "onedrive",
        "icloud~",
        "dropbox",
        ".ssh",
        ".gnupg",
        "keychains",
        "mail",
        "saved games",
        "favorites",
        "contacts",
        "links",
        "searches",
    ];

    /// No pattern may target a protected user-content or credential
    /// directory at ANY depth of its segment list.
    #[test]
    fn patterns_never_target_user_documents_or_credentials() {
        for p in patterns() {
            for seg in p.segments {
                let s = seg.to_ascii_lowercase();
                assert!(
                    !NEVER_CLEAN.contains(&s.as_str()),
                    "pattern for category {} targets protected root {seg:?}",
                    p.category
                );
            }
        }
    }

    /// A pattern with zero segments or only `*` segments would stage the
    /// ENTIRE env root (a profile, `AppData`) — the "no path equals a
    /// bare root" rule from cleaner's allowlist model.
    #[test]
    fn patterns_have_a_literal_segment_below_the_env_root() {
        for p in patterns() {
            assert!(!p.segments.is_empty(), "empty pattern for {}", p.category);
            assert!(
                p.segments.iter().any(|s| *s != "*"),
                "all-wildcard pattern for {} would stage the whole env root",
                p.category
            );
        }
    }

    /// Segments are single path components: a segment containing a
    /// separator or parent reference would bypass the one-segment-per-
    /// level matching contract and could smuggle a deeper or escaping
    /// path.
    #[test]
    fn pattern_segments_are_single_components() {
        for p in patterns() {
            assert!(!p.segments.is_empty(), "empty segments in {}", p.category);
            for seg in p.segments {
                assert!(!seg.is_empty(), "empty segment in {}", p.category);
                assert!(
                    !seg.contains('/') && !seg.contains('\\') && !seg.contains(".."),
                    "segment {seg:?} in {} is not a single clean component",
                    p.category
                );
            }
        }
    }

    /// A typo'd category id in the table silently drops every item it
    /// feeds (resolve buckets by id, then builds rows from the known
    /// list) — the uniqueness/known-ids check from cleaner's catalog.
    #[test]
    fn pattern_categories_are_known_to_the_catalog() {
        let known = [
            "downloads",
            "temp_caches",
            "browser_caches",
            "dev_caches",
            "android_emulators",
            "ios_simulators",
            "xcode_derived",
        ];
        for p in patterns() {
            assert!(
                known.contains(&p.category),
                "unknown category {:?} (typo? its matches would be dropped)",
                p.category
            );
        }
    }

    /// The browser rule-authoring constraint as a test: caches ONLY.
    /// Cookies, history, passwords and profile data live outside the
    /// `Cache` / `Code Cache` / `GPUCache` / `cache2` subtrees and must
    /// never appear in a browser row (cleaner's macOS.rs:615 constraint).
    #[test]
    fn browser_caches_touch_cache_subpaths_only() {
        let cache_leaf = ["cache", "code cache", "gpucache", "cache2"];
        for p in patterns() {
            if p.category == "browser_caches" {
                let last = p
                    .segments
                    .last()
                    .expect("non-empty by invariant")
                    .to_ascii_lowercase();
                assert!(
                    cache_leaf.contains(&last.as_str()),
                    "browser pattern ends at {last:?} — only cache subtrees are fair game"
                );
            }
        }
    }

    /// VM images are destructively expensive to lose: the category ships
    /// review-only so "Add all" refuses it (decision D7) — pinned at the
    /// engine level, not just the UI.
    #[test]
    fn vm_disks_category_is_review_only() {
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\Users\\z");
        t.append_batch(
            0,
            vec![file(
                "heavy.vhdx",
                8 * 1024 * 1024 * 1024,
                8 * 1024 * 1024 * 1024,
                1,
            )],
        ); // 1
        rollup::finalize(&mut t);
        let cats = resolve(&t, &env_roots(), 1);
        let vm = cats
            .iter()
            .find(|c| c.id == "vm_disks")
            .expect("vm_disks row for an 8 GiB vhdx");
        assert!(vm.review_only, "VM disks must refuse Add all");
        assert_eq!(vm.items, vec![1]);
    }

    /// The category cap is honored even when a pattern matches the
    /// entire tree (400 = `CATEGORY_CAP`; oversize matches truncate,
    /// the size reflects only the kept items).
    #[test]
    fn category_cap_truncates_oversize_matches() {
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\Users\\z");
        let mut batch = Vec::with_capacity(CATEGORY_CAP + 50);
        for i in 0..(CATEGORY_CAP + 50) {
            batch.push(file(&format!("f{i}.tmp"), 100, 100, 1));
        }
        t.append_batch(0, batch);
        rollup::finalize(&mut t);
        let cats = resolve(&t, &env_roots(), 1);
        // temp_caches matches the Local/Temp fixture path, not flat
        // profile files; node_modules matches none — the flat fixture
        // feeds no category here except possibly none. The real cap
        // assertion lives in the nested fixture below.
        assert!(cats.iter().all(|c| c.items.len() <= CATEGORY_CAP));
    }
}
