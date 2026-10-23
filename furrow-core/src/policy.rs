//! Small, auditable capture-policy parser.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

pub const POLICY_FILE: &str = ".furrowpolicy";
pub const POLICY_FILE_BYTES: &[u8] = b".furrowpolicy";
const MAX_POLICY_BYTES: u64 = 64 * 1024;
const MAX_RULES: usize = 1_024;

/// Metadata-only accounting of untracked files omitted by Git's ignore rules.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IgnoredSummary {
    pub files: u64,
    pub bytes: u64,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CapturePolicy {
    excluded: Vec<Vec<u8>>,
    gitignore: bool,
    ignored: BTreeSet<Vec<u8>>,
    pub omitted: Option<IgnoredSummary>,
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
        let mut ignored = self.ignored.clone();
        ignored.extend(other.ignored.iter().cloned());
        Self {
            excluded,
            gitignore: self.gitignore || other.gitignore,
            ignored,
            omitted: self.omitted.clone(),
        }
    }

    pub fn excludes_bytes(&self, relative: &[u8]) -> bool {
        self.ignored.contains(relative)
            || relative
                .iter()
                .enumerate()
                .any(|(index, byte)| *byte == b'/' && self.ignored.contains(&relative[..index]))
            || self.excluded.iter().any(|rule| {
                relative == rule.as_slice()
                    || (relative.starts_with(rule) && relative.get(rule.len()) == Some(&b'/'))
            })
    }

    /// Git itself supplies its ignore semantics, including tracked exceptions and global rules.
    /// Only directory listings and file metadata are read; ignored contents are never opened.
    pub fn resolve_gitignore(mut self, root: &Path) -> anyhow::Result<Self> {
        if !self.gitignore {
            return Ok(self);
        }
        let probe = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "--is-inside-work-tree"])
            .output()
            .context("run Git for gitignore policy")?;
        if !probe.status.success() || probe.stdout != b"true\n" {
            return Ok(self);
        }
        let list = |directories: bool| -> anyhow::Result<Vec<Vec<u8>>> {
            let mut command = Command::new("git");
            command.arg("-C").arg(root).args([
                "ls-files",
                "-z",
                "--others",
                "--ignored",
                "--exclude-standard",
            ]);
            if directories {
                command.arg("--directory");
            }
            let output = command.output()?;
            anyhow::ensure!(
                output.status.success(),
                "Git could not evaluate gitignore policy"
            );
            Ok(output
                .stdout
                .split(|b| *b == 0)
                .filter(|p| !p.is_empty())
                .map(|p| p.strip_suffix(b"/").unwrap_or(p).to_vec())
                .collect())
        };
        let mut summary = IgnoredSummary::default();
        for path in list(false)? {
            if self.excludes_bytes(&path) {
                continue;
            }
            let absolute = crate::repository::safe_join(root, &path)?;
            let metadata = match fs::symlink_metadata(&absolute) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if metadata.is_dir() {
                self.count_ignored_directory(root, &absolute, &mut summary)?;
            } else {
                summary.files += 1;
                summary.bytes += metadata.len();
            }
            self.ignored.insert(path);
        }
        for path in list(true)? {
            // Git collapses only wholly ignored directories, preserving force-added children.
            let absolute = crate::repository::safe_join(root, &path)?;
            if fs::symlink_metadata(absolute).is_ok_and(|m| m.is_dir()) {
                self.ignored.insert(path);
            }
        }
        // Reporting collapses top-level paths independently of capture pruning, so a
        // directory with tracked exceptions still has one concise omission label.
        let collapsed: BTreeSet<Vec<u8>> = self
            .ignored
            .iter()
            .map(|p| p.split(|byte| *byte == b'/').next().unwrap_or(p).to_vec())
            .collect();
        summary.paths = collapsed
            .into_iter()
            .take(20)
            .map(|p| {
                let directory = crate::repository::safe_join(root, &p)
                    .is_ok_and(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()));
                let mut display = String::from_utf8_lossy(&p).into_owned();
                if directory {
                    display.push('/');
                }
                display
            })
            .collect();
        self.omitted = Some(summary);
        Ok(self)
    }

    /// Git reports ignored nested repositories as directories rather than listing their files.
    /// Their omission totals still count file metadata without opening any contents.
    fn count_ignored_directory(
        &self,
        root: &Path,
        directory: &Path,
        summary: &mut IgnoredSummary,
    ) -> anyhow::Result<()> {
        let mut pending = vec![directory.to_owned()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(directory)? {
                let path = entry?.path();
                if self.excludes_bytes(path.strip_prefix(root)?.as_os_str().as_encoded_bytes()) {
                    continue;
                }
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                if metadata.is_dir() {
                    pending.push(path);
                } else {
                    summary.files += 1;
                    summary.bytes += metadata.len();
                }
            }
        }
        Ok(())
    }

    pub fn uses_gitignore(&self) -> bool {
        self.gitignore
    }

    /// Removing or replacing an ancestor would also destroy protected ignored children.
    pub fn contains_ignored(&self, relative: &[u8]) -> bool {
        let mut prefix = relative.to_vec();
        prefix.push(b'/');
        self.ignored
            .range(prefix.clone()..)
            .next()
            .is_some_and(|p| p.starts_with(&prefix))
    }

    pub fn excludes_path(&self, root: &Path, path: &Path) -> bool {
        path.strip_prefix(root)
            .ok()
            .is_some_and(|relative| self.excludes_bytes(relative.as_os_str().as_bytes()))
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
}
