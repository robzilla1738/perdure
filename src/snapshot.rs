//! Real-filesystem snapshots for the coding harness.
//!
//! A guard session needs a baseline of the working tree taken at `tach guard
//! begin`, so that later it can tell exactly which files the external agent
//! touched and check each against the goal's `fs.write` scope. We capture that
//! baseline as a *manifest*: a map of repo-relative path -> a small entry (a
//! content hash plus the metadata a guardrail must not miss). Hashes, not
//! contents, so the baseline of a 50k-file repo is still small — and the diff is a
//! cheap map comparison.
//!
//! The walk hard-excludes only `.git` and `.tach` — git's own store and Tach's own
//! run state, which churns every `verify`. Everything else, *including* dotdirs
//! like `.github/` and `.vscode/`, is walked and scope-checked: an agent that edits
//! CI config or an editor hook must be visible to the gate. Heavy build/dependency
//! roots (`target`, `node_modules`, …) are ignored by default but as ordinary,
//! overridable globs — not because they start with a dot.

use crate::patch::glob_match;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

/// A snapshot of a tree: repo-relative (forward-slashed) path -> entry.
pub type Manifest = BTreeMap<String, ManifestEntry>;

/// What a tracked path *is*. A regular file vs. a symlink — the distinction matters
/// to a guardrail because a file→symlink flip changes a path's meaning without
/// changing its "contents". (A file→directory flip needs no variant: it surfaces as
/// the file's key disappearing and child keys appearing, which the diff already
/// classifies as a change.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Symlink,
}

/// One entry in a tree manifest. More than a content hash, because a guardrail has
/// to notice changes that leave the bytes untouched: a file gaining the executable
/// bit, a regular file becoming a symlink, or a symlink retargeted. Symlinks are
/// recorded but never followed (they can cycle or point outside the repo), so a
/// symlink carries the hash of its *target path*, not of any pointed-to bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ManifestEntry {
    pub kind: EntryKind,
    /// FNV hash of the file's bytes; `None` for a symlink.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// Whether any execute bit is set (unix `mode & 0o111`); always false off-unix.
    #[serde(default)]
    pub executable: bool,
    /// FNV hash of the symlink's target path; `None` for a regular file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink_target_hash: Option<String>,
}

impl ManifestEntry {
    /// A stable one-line signature folded into a tree digest. Every field that the
    /// diff considers a change must appear here, so the verify idempotency key (and
    /// thus reuse soundness and the stale-tree commit check) shifts when the exec
    /// bit or a symlink target changes — not only when bytes change.
    pub fn signature(&self) -> String {
        let kind = match self.kind {
            EntryKind::File => "f",
            EntryKind::Symlink => "l",
        };
        format!(
            "{}|{}|{}|{}",
            kind,
            self.content_hash.as_deref().unwrap_or("-"),
            if self.executable { "x" } else { "-" },
            self.symlink_target_hash.as_deref().unwrap_or("-"),
        )
    }
}

/// Custom `Deserialize` that also accepts a **legacy** bare-string value — the old
/// manifest stored `path -> content_hash` directly — so a baseline written by an
/// earlier Tach still loads as `{ File, Some(hash), .. }`. Same back-compat posture
/// as the `#[serde(default)]` fields on `RunState`.
impl<'de> Deserialize<'de> for ManifestEntry {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Legacy(String),
            Full {
                kind: EntryKind,
                #[serde(default)]
                content_hash: Option<String>,
                #[serde(default)]
                executable: bool,
                #[serde(default)]
                symlink_target_hash: Option<String>,
            },
        }
        Ok(match Raw::deserialize(d)? {
            Raw::Legacy(hash) => ManifestEntry {
                kind: EntryKind::File,
                content_hash: Some(hash),
                executable: false,
                symlink_target_hash: None,
            },
            Raw::Full {
                kind,
                content_hash,
                executable,
                symlink_target_hash,
            } => ManifestEntry {
                kind,
                content_hash,
                executable,
                symlink_target_hash,
            },
        })
    }
}

/// Directory/dependency roots ignored by default — even with no `.tachignore` — so
/// a snapshot never walks a giant `target/` or `node_modules/`. Unlike the hard
/// excludes, these are ordinary globs the user can shadow or extend via
/// `.tachignore`; they are not dotdir-based, so `.github`, `.vscode`, `.circleci`
/// and friends are deliberately absent and therefore tracked.
const DEFAULT_SOFT_IGNORES: &[&str] = &[
    "target",
    "node_modules",
    "dist",
    "build",
    ".venv",
    "__pycache__",
];

/// The ignore set governing a walk: the built-in soft defaults plus any
/// `.tachignore` lines. The two hard excludes (`.git`, `.tach`) are enforced by the
/// walk itself, not represented here, so nothing can override them.
#[derive(Debug)]
pub struct Ignore {
    /// Each pattern is expanded to two globs — the entry itself and `entry/**` — so
    /// `target` ignores both the dir and everything beneath it.
    globs: Vec<String>,
}

impl Default for Ignore {
    fn default() -> Self {
        let mut globs = Vec::new();
        for name in DEFAULT_SOFT_IGNORES {
            globs.push((*name).to_string());
            globs.push(format!("{name}/**"));
        }
        Ignore { globs }
    }
}

impl Ignore {
    /// Load `<repo>/.tachignore` on top of the built-in soft defaults, ignoring
    /// blank lines and `#` comments. A missing file yields just the defaults.
    pub fn load(repo: &Path) -> Self {
        let mut ig = Ignore::default();
        if let Ok(text) = fs::read_to_string(repo.join(".tachignore")) {
            for line in text.lines() {
                let l = line.trim();
                if l.is_empty() || l.starts_with('#') {
                    continue;
                }
                let base = l.trim_end_matches('/');
                ig.globs.push(base.to_string());
                ig.globs.push(format!("{base}/**"));
            }
        }
        ig
    }

    /// Does any ignore pattern (default or `.tachignore`) match this path?
    ///
    /// Two files are **never** ignored, at any depth: `.tachignore` and `.gitignore`.
    /// They *define* the gate's blind spots — `.tachignore` what the scope gate skips,
    /// `.gitignore` what a human will commit — so a silent edit to either is exactly
    /// what an audit must see. Refusing to ignore them means an agent can't shrink the
    /// gate's view by editing the very file that configures it (belt-and-suspenders to
    /// the frozen baseline, which already pins the rules captured at `begin`).
    pub fn matches(&self, rel: &str) -> bool {
        let base = rel.rsplit('/').next().unwrap_or(rel);
        if matches!(base, ".tachignore" | ".gitignore") {
            return false;
        }
        self.globs.iter().any(|g| glob_match(g, rel))
    }

    /// The resolved glob set, for **freezing** into a run's baseline so every later
    /// scope diff classifies against the rules captured at `begin` — never a live,
    /// agent-editable `.tachignore`. Round-trips through [`Ignore::from_globs`].
    pub fn globs(&self) -> &[String] {
        &self.globs
    }

    /// Reconstruct an `Ignore` from a frozen glob set (see [`Ignore::globs`]).
    pub fn from_globs(globs: Vec<String>) -> Self {
        Ignore { globs }
    }
}

/// Directory names the walk *never* descends into, regardless of `.tachignore`:
/// git's own store and Tach's own run state. Everything else — including dotdirs
/// like `.github` and `.vscode` — is walked, so edits there are seen and
/// scope-checked.
fn is_hard_excluded_dir(name: &str) -> bool {
    matches!(name, ".git" | ".tach")
}

/// SHA-256 hex digest of a file's bytes — a **cryptographic** hash, deliberately not
/// the FNV mixer the store uses for addressing ids. The scope gate's honesty rests on
/// this: against an adversarial agent, a content hash must resist a *crafted*
/// collision, or an out-of-scope edit could be made to hash equal to the baseline and
/// the diff would read it as "unmodified" and never flag it. FNV-1a is linear and
/// trivially collidable; SHA-256 is not. (See `crate::hash`.) Folds into
/// `ManifestEntry::signature`, the tree digest, and thus the verify idempotency key.
pub fn content_hash(bytes: &[u8]) -> String {
    crate::hash::sha256_hex(&[bytes])
}

/// Whether `p` has any execute bit set. Unix-only signal; `false` elsewhere.
#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p)
        .map(|md| md.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
#[cfg(not(unix))]
fn is_executable(_p: &Path) -> bool {
    false
}

/// Walk the working tree under `repo` into a manifest, honoring the ignore set.
pub fn snapshot(repo: &Path, ignore: &Ignore) -> io::Result<Manifest> {
    let mut m = Manifest::new();
    walk(repo, repo, ignore, &mut m)?;
    Ok(m)
}

fn walk(base: &Path, dir: &Path, ignore: &Ignore, m: &mut Manifest) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.path());
    for e in entries {
        let ft = match e.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        let rel = p
            .strip_prefix(base)
            .unwrap_or(&p)
            .to_string_lossy()
            .replace('\\', "/");
        // Symlinks first: `file_type()` does not follow them, so a symlink to a dir
        // still reports as a symlink here. Record the link, never traverse it.
        if ft.is_symlink() {
            if ignore.matches(&rel) {
                continue;
            }
            let target = fs::read_link(&p)
                .map(|t| t.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            m.insert(
                rel,
                ManifestEntry {
                    kind: EntryKind::Symlink,
                    content_hash: None,
                    executable: false,
                    symlink_target_hash: Some(content_hash(target.as_bytes())),
                },
            );
        } else if ft.is_dir() {
            if is_hard_excluded_dir(&name) || ignore.matches(&rel) {
                continue;
            }
            walk(base, &p, ignore, m)?;
        } else if ft.is_file() {
            if ignore.matches(&rel) {
                continue;
            }
            let bytes = fs::read(&p)?;
            m.insert(
                rel,
                ManifestEntry {
                    kind: EntryKind::File,
                    content_hash: Some(content_hash(&bytes)),
                    executable: is_executable(&p),
                    symlink_target_hash: None,
                },
            );
        }
    }
    Ok(())
}

/// The classified difference between a baseline manifest and a current one.
#[derive(Default, Clone, Debug, Serialize)]
pub struct Diff {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

impl Diff {
    /// Every path that changed in any way, sorted. This is the set the scope gate
    /// classifies against the goal's `fs.write` authority.
    pub fn changed(&self) -> Vec<String> {
        let mut all: Vec<String> = self
            .added
            .iter()
            .chain(&self.modified)
            .chain(&self.deleted)
            .cloned()
            .collect();
        all.sort();
        all.dedup();
        all
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }
}

/// Diff a baseline against the current head manifest. Deterministic and sorted. An
/// entry counts as `modified` when *any* field differs — content, kind, exec bit,
/// or symlink target — so a metadata-only change is not silently lost.
pub fn diff(base: &Manifest, head: &Manifest) -> Diff {
    let mut d = Diff::default();
    for (path, entry) in head {
        match base.get(path) {
            None => d.added.push(path.clone()),
            Some(b) if b != entry => d.modified.push(path.clone()),
            _ => {}
        }
    }
    for path in base.keys() {
        if !head.contains_key(path) {
            d.deleted.push(path.clone());
        }
    }
    d.added.sort();
    d.modified.sort();
    d.deleted.sort();
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "tach_snap_{}_{}_{}",
                std::process::id(),
                tag,
                n
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn write(&self, rel: &str, text: &str) {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn snapshot_skips_hard_excludes_and_soft_defaults_but_tracks_files() {
        let d = TempDir::new("walk");
        d.write("src/lib.rs", "fn a() {}");
        d.write("README.md", "hi");
        d.write("target/debug/junk", "x"); // soft default → skipped
        d.write("node_modules/pkg/index.js", "x"); // soft default → skipped
        d.write(".git/config", "x"); // hard exclude → skipped
        d.write(".tach/goals/run/state.json", "x"); // hard exclude → skipped
        let m = snapshot(d.path(), &Ignore::default()).unwrap();
        assert!(m.contains_key("src/lib.rs"));
        assert!(m.contains_key("README.md"));
        assert!(!m.keys().any(|k| k.starts_with("target/")));
        assert!(!m.keys().any(|k| k.starts_with("node_modules/")));
        assert!(!m.keys().any(|k| k.starts_with(".git/")));
        assert!(!m.keys().any(|k| k.starts_with(".tach/")));
    }

    #[test]
    fn dotdirs_other_than_git_and_tach_are_tracked() {
        let d = TempDir::new("dotdirs");
        d.write(".github/workflows/ci.yml", "on: push\n");
        d.write(".vscode/settings.json", "{}");
        d.write(".circleci/config.yml", "version: 2\n");
        d.write(".git/config", "x"); // still invisible
        d.write(".tach/x", "x"); // still invisible
        let m = snapshot(d.path(), &Ignore::default()).unwrap();
        assert!(
            m.contains_key(".github/workflows/ci.yml"),
            "CI config must be visible to the gate"
        );
        assert!(m.contains_key(".vscode/settings.json"));
        assert!(m.contains_key(".circleci/config.yml"));
        assert!(!m.keys().any(|k| k.starts_with(".git/")));
        assert!(!m.keys().any(|k| k.starts_with(".tach/")));
    }

    #[test]
    fn tachignore_excludes_matches() {
        let d = TempDir::new("ignore");
        d.write(".tachignore", "# comment\ncoverage\n*.log\n");
        d.write("src/lib.rs", "x");
        d.write("coverage/report.html", "x");
        d.write("debug.log", "x");
        let ig = Ignore::load(d.path());
        let m = snapshot(d.path(), &ig).unwrap();
        assert!(m.contains_key("src/lib.rs"));
        assert!(!m.contains_key("coverage/report.html"), "coverage/ ignored");
        assert!(!m.contains_key("debug.log"), "*.log ignored");
    }

    #[test]
    fn diff_classifies_content_changes() {
        let d = TempDir::new("diff");
        d.write("a.txt", "1");
        d.write("b.txt", "1");
        let base = snapshot(d.path(), &Ignore::default()).unwrap();
        d.write("b.txt", "2"); // modify
        d.write("c.txt", "3"); // add
        fs::remove_file(d.path().join("a.txt")).unwrap(); // delete
        let head = snapshot(d.path(), &Ignore::default()).unwrap();
        let diff = diff(&base, &head);
        assert_eq!(diff.added, vec!["c.txt"]);
        assert_eq!(diff.modified, vec!["b.txt"]);
        assert_eq!(diff.deleted, vec!["a.txt"]);
        assert_eq!(diff.changed(), vec!["a.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn legacy_bare_string_baseline_deserializes() {
        // A baseline written by an earlier Tach: path -> content hash string.
        let legacy = r#"{"src/lib.rs":"00000000deadbeef","README.md":"0000000012345678"}"#;
        let m: Manifest = serde_json::from_str(legacy).unwrap();
        let e = m.get("src/lib.rs").unwrap();
        assert_eq!(e.kind, EntryKind::File);
        assert_eq!(e.content_hash.as_deref(), Some("00000000deadbeef"));
        assert!(!e.executable);
        assert!(e.symlink_target_hash.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn detects_exec_bit_change() {
        use std::os::unix::fs::PermissionsExt;
        let d = TempDir::new("exec");
        d.write("run.sh", "echo hi\n");
        let base = snapshot(d.path(), &Ignore::default()).unwrap();
        assert!(!base["run.sh"].executable);
        let p = d.path().join("run.sh");
        let mut perm = fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&p, perm).unwrap();
        let head = snapshot(d.path(), &Ignore::default()).unwrap();
        assert!(head["run.sh"].executable);
        let diff = diff(&base, &head);
        assert_eq!(
            diff.modified,
            vec!["run.sh"],
            "exec-bit flip must be a change"
        );
    }

    #[cfg(unix)]
    #[test]
    fn detects_symlink_and_target_change() {
        use std::os::unix::fs::symlink;
        let d = TempDir::new("symlink");
        d.write("a.txt", "a");
        d.write("b.txt", "b");
        symlink("a.txt", d.path().join("link")).unwrap();
        let base = snapshot(d.path(), &Ignore::default()).unwrap();
        assert_eq!(base["link"].kind, EntryKind::Symlink);
        assert!(base["link"].content_hash.is_none());
        assert!(base["link"].symlink_target_hash.is_some());

        // Retarget the symlink: bytes of nothing changed, but meaning did.
        fs::remove_file(d.path().join("link")).unwrap();
        symlink("b.txt", d.path().join("link")).unwrap();
        let head = snapshot(d.path(), &Ignore::default()).unwrap();
        let diff = diff(&base, &head);
        assert_eq!(
            diff.modified,
            vec!["link"],
            "retargeted symlink must be a change"
        );
    }

    #[cfg(unix)]
    #[test]
    fn detects_file_to_symlink_flip() {
        use std::os::unix::fs::symlink;
        let d = TempDir::new("flip");
        d.write("x", "real contents");
        d.write("other", "other");
        let base = snapshot(d.path(), &Ignore::default()).unwrap();
        assert_eq!(base["x"].kind, EntryKind::File);
        // Replace the regular file with a symlink of the same name.
        fs::remove_file(d.path().join("x")).unwrap();
        symlink("other", d.path().join("x")).unwrap();
        let head = snapshot(d.path(), &Ignore::default()).unwrap();
        assert_eq!(head["x"].kind, EntryKind::Symlink);
        assert_eq!(diff(&base, &head).modified, vec!["x"]);
    }
}
