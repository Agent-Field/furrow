//! Small, auditable capture-policy parser.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

pub const POLICY_FILE: &str = ".furrowpolicy";
pub const POLICY_FILE_BYTES: &[u8] = b".furrowpolicy";
const MAX_POLICY_BYTES: u64 = 64 * 1024;
const MAX_RULES: usize = 1_024;
const MAX_OMISSION_GROUPS: usize = 20;

/// Collapsed omitted names discovered without enumerating ignored directories.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IgnoredSummary {
    pub paths: Vec<String>,
    #[serde(default)]
    pub more: usize,
}

#[derive(Debug, Clone, Default)]
pub struct CapturePolicy {
    excluded: Vec<Vec<u8>>,
    gitignore: bool,
    matcher: Option<RefCell<crate::gitignore::Rules>>,
    omitted: RefCell<BTreeMap<Vec<u8>, bool>>,
    full_capture: Cell<bool>,
    virtual_subtree: Option<Vec<u8>>,
}

impl CapturePolicy {
    pub fn load(root: &Path) -> anyhow::Result<Self> {
        let path = root.join(POLICY_FILE);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            ".furrowpolicy must be a regular file"
        );
        anyhow::ensure!(
            metadata.len() <= MAX_POLICY_BYTES,
            ".furrowpolicy exceeds 64 KiB"
        );
        let contents = fs::read_to_string(&path).context(".furrowpolicy must be valid UTF-8")?;
        let mut excluded = Vec::new();
        let mut gitignore = false;
        for (index, raw) in contents.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == "gitignore" {
                gitignore = true;
                continue;
            }
            let value = line.strip_prefix("exclude ").with_context(|| {
                format!(
                    ".furrowpolicy line {} must use `exclude <relative-subtree>` or `gitignore`",
                    index + 1
                )
            })?;
            let value = value.trim().trim_end_matches('/');
            let bytes = value.as_bytes();
            validate_rule(bytes)
                .with_context(|| format!("invalid .furrowpolicy line {}", index + 1))?;
            excluded.push(bytes.to_vec());
            anyhow::ensure!(
                excluded.len() <= MAX_RULES,
                ".furrowpolicy exceeds 1024 rules"
            );
        }
        excluded.sort();
        excluded.dedup();
        Ok(Self {
            excluded,
            gitignore,
            ..Self::default()
        })
    }

    pub fn from_rules(rules: &[String]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            rules.len() <= MAX_RULES,
            "snapshot policy exceeds 1024 rules"
        );
        let mut excluded = Vec::with_capacity(rules.len());
        for rule in rules {
            validate_rule(rule.as_bytes())?;
            excluded.push(rule.as_bytes().to_vec());
        }
        excluded.sort();
        excluded.dedup();
        Ok(Self {
            excluded,
            ..Self::default()
        })
    }

    pub fn union(&self, other: &Self) -> Self {
        let mut excluded = self.excluded.clone();
        excluded.extend(other.excluded.iter().cloned());
        excluded.sort();
        excluded.dedup();
        Self {
            excluded,
            gitignore: self.gitignore || other.gitignore,
            matcher: self.matcher.clone().or_else(|| other.matcher.clone()),
            omitted: self.omitted.clone(),
            full_capture: self.full_capture.clone(),
            virtual_subtree: self
                .virtual_subtree
                .clone()
                .or_else(|| other.virtual_subtree.clone()),
        }
    }

    pub fn excludes_bytes(&self, relative: &[u8]) -> bool {
        if self.excluded.iter().any(|rule| {
            relative == rule.as_slice()
                || (relative.starts_with(rule) && relative.get(rule.len()) == Some(&b'/'))
        }) {
            return true;
        }
        if self.is_virtual(relative) {
            return false;
        }
        let Some(matcher) = &self.matcher else {
            return false;
        };
        let mut matcher = matcher.borrow_mut();
        let ignored = matcher.excludes(relative);
        if ignored || matcher.ignored_directory(relative) {
            self.note_omission(matcher.root(), relative);
        }
        ignored
    }

    /// Resolving policy reads control files only; capture discovers omitted names as it walks.
    pub fn resolve_gitignore(mut self, root: &Path) -> anyhow::Result<Self> {
        if self.gitignore {
            self.matcher = crate::gitignore::Rules::discover(root)?.map(RefCell::new);
        }
        Ok(self)
    }

    fn note_omission(&self, root: &Path, relative: &[u8]) {
        let first = relative.split(|b| *b == b'/').next().unwrap_or(relative);
        let path = crate::gitignore::bytes_path(first);
        let directory = fs::symlink_metadata(root.join(path)).is_ok_and(|m| m.is_dir());
        self.omitted.borrow_mut().insert(first.to_vec(), directory);
    }

    /// A composed subtree is outside the project's physical Git namespace; explicit exclusions still apply.
    pub fn with_virtual_subtree(mut self, prefix: &[u8]) -> Self {
        self.virtual_subtree = Some(prefix.to_vec());
        self
    }

    fn is_virtual(&self, relative: &[u8]) -> bool {
        self.virtual_subtree.as_ref().is_some_and(|prefix| {
            relative == prefix
                || (relative.starts_with(prefix) && relative.get(prefix.len()) == Some(&b'/'))
        })
    }

    pub fn ignored_summary(&self) -> Option<IgnoredSummary> {
        self.matcher.as_ref()?;
        let omitted = self.omitted.borrow();
        Some(IgnoredSummary {
            paths: omitted
                .iter()
                .take(MAX_OMISSION_GROUPS)
                .map(|(name, directory)| {
                    let name = String::from_utf8_lossy(name);
                    if *directory {
                        format!("{name}/")
                    } else {
                        name.into_owned()
                    }
                })
                .collect(),
            more: omitted.len().saturating_sub(MAX_OMISSION_GROUPS),
        })
    }

    pub fn begin_full_capture(&self) {
        self.full_capture.set(true);
        self.omitted.borrow_mut().clear();
    }

    pub fn did_full_capture(&self) -> bool {
        self.full_capture.get()
    }

    /// Ignored directories with tracked exceptions expose only the next indexed path components.
    pub fn tracked_children(&self, relative: &[u8]) -> Option<Vec<std::ffi::OsString>> {
        if self.is_virtual(relative) {
            return None;
        }
        self.matcher
            .as_ref()?
            .borrow_mut()
            .tracked_children(relative)
    }

    /// An ancestor cannot be replaced if doing so would destroy an ignored descendant.
    pub fn contains_ignored(&self, relative: &[u8]) -> bool {
        if self.is_virtual(relative) {
            return false;
        }
        let Some(matcher) = &self.matcher else {
            return false;
        };
        matcher.borrow_mut().contains_ignored(relative)
    }

    pub fn excludes_path(&self, root: &Path, path: &Path) -> bool {
        path.strip_prefix(root)
            .ok()
            .is_some_and(|relative| self.excludes_bytes(&crate::platform::path_bytes(relative)))
    }

    pub fn rules(&self) -> impl Iterator<Item = &[u8]> {
        self.excluded.iter().map(Vec::as_slice)
    }

    pub fn rule_strings(&self) -> Vec<String> {
        self.excluded
            .iter()
            .map(|rule| String::from_utf8(rule.clone()).expect("policy rules are UTF-8"))
            .collect()
    }
}

fn validate_rule(rule: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(!rule.is_empty(), "exclude path cannot be empty");
    anyhow::ensure!(!rule.starts_with(b"/"), "exclude path must be relative");
    anyhow::ensure!(!rule.contains(&0), "exclude path contains NUL");
    anyhow::ensure!(
        rule.split(|byte| *byte == b'/')
            .all(|part| !part.is_empty() && part != b"." && part != b".."),
        "exclude path contains an unsafe component"
    );
    let first = rule.split(|byte| *byte == b'/').next().unwrap_or_default();
    anyhow::ensure!(
        first != b".git" && first != b".furrow" && rule != POLICY_FILE_BYTES,
        "Git and furrow control state cannot be excluded"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_literal_subtrees_and_rejects_unsafe_rules() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(
            temporary.path().join(POLICY_FILE),
            b"# local policy\nexclude node_modules/\nexclude packages/app/cache\n",
        )
        .unwrap();
        let policy = CapturePolicy::load(temporary.path()).unwrap();
        assert!(policy.excludes_bytes(b"node_modules/pkg/index.js"));
        assert!(policy.excludes_bytes(b"packages/app/cache"));
        assert!(!policy.excludes_bytes(b"packages/app/cacheable"));

        fs::write(temporary.path().join(POLICY_FILE), b"exclude ../outside\n").unwrap();
        assert!(CapturePolicy::load(temporary.path()).is_err());
        fs::write(temporary.path().join(POLICY_FILE), b"exclude .git\n").unwrap();
        assert!(CapturePolicy::load(temporary.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn g4_distinct_omitted_names_remain_distinct_when_display_is_lossy() {
        // G4 counts omitted groups by filesystem name, even when several names have the same display text.
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success());
        fs::write(root.join(POLICY_FILE), "gitignore\n").unwrap();
        fs::write(root.join(".gitignore"), "bad*\n").unwrap();
        let policy = CapturePolicy::load(root)
            .unwrap()
            .resolve_gitignore(root)
            .unwrap();
        for suffix in 128..154 {
            let mut name = b"bad".to_vec();
            name.push(suffix);
            fs::write(
                crate::repository::safe_join(root, &name).unwrap(),
                "ignored",
            )
            .unwrap();
            assert!(policy.excludes_bytes(&name));
        }
        let report = policy.ignored_summary().unwrap();
        assert_eq!(report.paths.len(), 20);
        assert_eq!(report.more, 6);
    }
}
