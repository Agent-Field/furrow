//! In-process Git rules with lazy ancestor loading and identity-keyed process caches.

mod sparse;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    size: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    inode: (u64, u64, i64, i64),
    #[cfg(windows)]
    file: Arc<same_file::Handle>,
}

fn identity(path: &Path) -> Option<Identity> {
    let m = fs::metadata(path).ok()?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Some(Identity {
        size: m.len(),
        modified: m.modified().ok(),
        created: m.created().ok(),
        #[cfg(unix)]
        inode: (m.dev(), m.ino(), m.ctime(), m.ctime_nsec()),
        #[cfg(windows)]
        file: Arc::new(same_file::Handle::from_path(path).ok()?),
    })
}

#[derive(Debug, Default)]
struct Tracked {
    paths: BTreeSet<Vec<u8>>,
    folded: std::collections::BTreeMap<Vec<u8>, Vec<u8>>,
    gitlinks: BTreeSet<Vec<u8>>,
    folded_gitlinks: BTreeSet<Vec<u8>>,
    sparse: Option<sparse::Sparse>,
}

impl Tracked {
    fn contains(&self, path: &[u8], insensitive: bool) -> bool {
        (if insensitive {
            self.folded.contains_key(&path.to_ascii_lowercase())
        } else {
            self.paths.contains(path)
        }) || self
            .sparse
            .as_ref()
            .is_some_and(|s| s.is_file(path, insensitive))
    }

    fn is_gitlink(&self, path: &[u8], insensitive: bool) -> bool {
        (if insensitive {
            self.folded_gitlinks.contains(&path.to_ascii_lowercase())
        } else {
            self.gitlinks.contains(path)
        }) || self
            .sparse
            .as_ref()
            .is_some_and(|s| s.is_gitlink(path, insensitive))
    }

    fn has_descendant(&self, prefix: &[u8], insensitive: bool) -> bool {
        (if insensitive {
            let prefix = prefix.to_ascii_lowercase();
            self.folded
                .range(prefix.clone()..)
                .next()
                .is_some_and(|(p, _)| p.starts_with(&prefix))
        } else {
            self.paths
                .range(prefix.to_vec()..)
                .next()
                .is_some_and(|p| p.starts_with(prefix))
        }) || self
            .sparse
            .as_ref()
            .is_some_and(|s| s.has_descendant(prefix, insensitive))
    }

    fn children(&self, prefix: &[u8], insensitive: bool) -> Vec<OsString> {
        let name = |path: &[u8]| {
            bytes_path(path[prefix.len()..].split(|b| *b == b'/').next().unwrap()).into_os_string()
        };
        let mut names: BTreeSet<_> = if insensitive {
            let folded = prefix.to_ascii_lowercase();
            self.folded
                .range(folded.clone()..)
                .take_while(|(p, _)| p.starts_with(&folded))
                .map(|(_, p)| name(p))
                .collect()
        } else {
            self.paths
                .range(prefix.to_vec()..)
                .take_while(|p| p.starts_with(prefix))
                .map(|p| name(p))
                .collect()
        };
        if let Some(sparse) = &self.sparse {
            names.extend(sparse.children(prefix, insensitive));
        }
        names.into_iter().collect()
    }
}
type IndexCache = HashMap<PathBuf, (Option<Identity>, Arc<Tracked>)>;
type MatcherCache = HashMap<(PathBuf, PathBuf, bool), (Option<Identity>, Arc<Gitignore>)>;
static INDEX: OnceLock<Mutex<IndexCache>> = OnceLock::new();
static MATCHERS: OnceLock<Mutex<MatcherCache>> = OnceLock::new();

#[cfg(test)]
thread_local! { static INDEX_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

fn tracked(index: &Path) -> Arc<Tracked> {
    let stamp = identity(index);
    let mut cache = INDEX.get_or_init(Default::default).lock().unwrap();
    if let Some((previous, paths)) = cache.get(index) {
        if *previous == stamp {
            return paths.clone();
        }
    }
    // Missing, unreadable and malformed indexes confer no tracked exceptions.
    let paths = if stamp.as_ref().is_some_and(|s| s.size >= 32) {
        #[cfg(test)]
        INDEX_READS.with(|reads| reads.set(reads.get() + 1));
        gix_index::File::at(index, Default::default(), false, Default::default())
            .map(|file| {
                let mut result = Tracked::default();
                let mut roots = std::collections::BTreeMap::new();
                for entry in file.entries() {
                    let path = entry.path(&file);
                    if entry.mode.is_sparse() {
                        let path = path.strip_suffix(b"/").unwrap_or(path);
                        if valid_tracked_path(path) {
                            roots.insert(path.to_vec(), entry.id);
                        }
                    } else if valid_tracked_path(path) {
                        result.paths.insert(path.to_vec());
                        if entry.mode.is_submodule() {
                            result.gitlinks.insert(path.to_vec());
                        }
                    }
                }
                result.folded = result
                    .paths
                    .iter()
                    .map(|p| (p.to_ascii_lowercase(), p.clone()))
                    .collect();
                result.folded_gitlinks = result
                    .gitlinks
                    .iter()
                    .map(|p| p.to_ascii_lowercase())
                    .collect();
                if !roots.is_empty() {
                    let dir = index.parent().expect("index has a Git directory");
                    let common = fs::read_to_string(dir.join("commondir"))
                        .ok()
                        .map(|s| dir.join(s.trim()))
                        .unwrap_or_else(|| dir.to_owned());
                    result.sparse = Some(sparse::Sparse::new(roots, common.join("objects")));
                }
                result
            })
            .unwrap_or_default()
    } else {
        Tracked::default()
    };
    let paths = Arc::new(paths);
    cache.insert(index.to_owned(), (stamp, paths.clone()));
    paths
}

fn valid_tracked_path(path: &[u8]) -> bool {
    path.split(|b| *b == b'/').all(valid_child_name)
}

fn valid_child_name(name: &[u8]) -> bool {
    // Index and tree data cannot supply navigation components or empty names to the filesystem walk.
    let path = bytes_path(name);
    let mut components = path.components();
    !name.contains(&0)
        && matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
}

fn matcher(base: &Path, file: &Path, case_insensitive: bool) -> Arc<Gitignore> {
    let stamp = identity(file);
    let key = (base.to_owned(), file.to_owned(), case_insensitive);
    let mut cache = MATCHERS.get_or_init(Default::default).lock().unwrap();
    if let Some((previous, rules)) = cache.get(&key) {
        if *previous == stamp {
            return rules.clone();
        }
    }
    let mut builder = GitignoreBuilder::new(base);
    builder
        .case_insensitive(case_insensitive)
        .expect("case option is valid");
    // Git also tolerates absent/unreadable ignore files and individual invalid patterns.
    builder.add(file);
    let rules = Arc::new(builder.build().unwrap_or_else(|_| Gitignore::empty()));
    cache.insert(key, (stamp, rules.clone()));
    rules
}

pub(crate) fn bytes_path(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).as_ref())
    }
}

fn git_bytes(path: &Path) -> Vec<u8> {
    #[cfg(windows)]
    {
        path.to_string_lossy().replace('\\', "/").into_bytes()
    }
    #[cfg(not(windows))]
    {
        path.as_os_str().as_encoded_bytes().to_vec()
    }
}

#[derive(Debug, Clone)]
struct Repository {
    root: PathBuf,
    tracked: Arc<Tracked>,
    low_priority: Vec<Arc<Gitignore>>,
    case_insensitive: bool,
}

fn git_dir(root: &Path) -> Option<PathBuf> {
    let marker = root.join(".git");
    let metadata = fs::symlink_metadata(&marker).ok()?;
    let directory = if metadata.is_dir() {
        marker
    } else {
        if !metadata.is_file() {
            return None;
        }
        let content = fs::read_to_string(&marker).ok()?;
        root.join(content.trim().strip_prefix("gitdir: ")?)
    };
    let common = fs::read_to_string(directory.join("commondir"))
        .ok()
        .map(|s| directory.join(s.trim()))
        .unwrap_or_else(|| directory.clone());
    (directory.join("HEAD").is_file() && common.join("objects").is_dir()).then_some(directory)
}

#[derive(Clone, PartialEq, Eq)]
struct ConfigKey {
    environment: Vec<(OsString, OsString)>,
    files: Vec<(PathBuf, Option<Identity>)>,
}

type ConfigCache = HashMap<PathBuf, (ConfigKey, gix_config::File<'static>)>;
thread_local! { static CONFIGS: std::cell::RefCell<ConfigCache> = Default::default(); }

fn config(common: &Path, gitdir: &Path) -> anyhow::Result<gix_config::File<'static>> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".config")));
    let mut paths = Vec::new();
    if std::env::var_os("GIT_CONFIG_NOSYSTEM").is_none() {
        paths.push((
            std::env::var_os("GIT_CONFIG_SYSTEM")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/etc/gitconfig")),
            gix_config::Source::System,
        ));
    }
    if let Some(global) = std::env::var_os("GIT_CONFIG_GLOBAL") {
        paths.push((PathBuf::from(global), gix_config::Source::User));
    } else {
        if let Some(xdg) = &xdg {
            paths.push((xdg.join("git/config"), gix_config::Source::Git));
        }
        if let Some(home) = &home {
            paths.push((home.join(".gitconfig"), gix_config::Source::User));
        }
    }
    paths.push((common.join("config"), gix_config::Source::Local));
    let mut environment: Vec<_> = std::env::vars_os()
        .filter(|(name, _)| {
            let name = name.to_string_lossy();
            name.starts_with("GIT_CONFIG")
                || matches!(name.as_ref(), "HOME" | "USERPROFILE" | "XDG_CONFIG_HOME")
        })
        .collect();
    environment.sort();
    let mut dependencies: Vec<_> = paths.iter().map(|(path, _)| path.clone()).collect();
    dependencies.push(gitdir.join("config.worktree"));
    let key = ConfigKey {
        environment,
        files: dependencies
            .into_iter()
            .map(|p| {
                let stamp = identity(&p);
                (p, stamp)
            })
            .collect(),
    };
    let cached = CONFIGS.with(|cache| {
        cache
            .borrow()
            .get(gitdir)
            .filter(|(old, _)| old == &key)
            .map(|(_, file)| file.clone())
    });
    if let Some(file) = cached {
        return Ok(file);
    }
    let head = fs::read_to_string(gitdir.join("HEAD")).unwrap_or_default();
    let branch = head
        .trim()
        .strip_prefix("ref: ")
        .and_then(|name| name.try_into().ok());
    let options = gix_config::file::init::Options {
        includes: gix_config::file::includes::Options::follow(
            gix_config::path::interpolate::Context {
                home_dir: home.as_deref(),
                ..Default::default()
            },
            gix_config::file::includes::conditional::Context {
                git_dir: Some(gitdir),
                branch_name: branch,
            },
        ),
        ignore_io_errors: true,
        ..Default::default()
    };
    // Explicit paths avoid gix's convenience loader, which probes the Git installation with a subprocess.
    let mut result = gix_config::File::from_paths_metadata(
        paths
            .into_iter()
            .filter(|(p, _)| p.is_file())
            .map(|(p, source)| gix_config::file::Metadata::from(source).at(p)),
        options,
    )?
    .unwrap_or_default();
    if result
        .boolean("extensions.worktreeConfig")
        .and_then(Result::ok)
        .unwrap_or(false)
    {
        if let Some(worktree) = gix_config::File::from_paths_metadata(
            [
                gix_config::file::Metadata::from(gix_config::Source::Worktree)
                    .at(gitdir.join("config.worktree")),
            ]
            .into_iter()
            .filter(|m| m.path.as_ref().is_some_and(|p| p.is_file())),
            options,
        )? {
            result.append(worktree);
        }
    }
    if let Some(env) = gix_config::File::from_env(options)? {
        result.append(env);
    }
    // Includes may name missing files or depend on a branch; preserve their full evaluation.
    if !result.sections().any(|s| {
        s.header().name().eq_ignore_ascii_case(b"include")
            || s.header().name().eq_ignore_ascii_case(b"includeIf")
    }) {
        CONFIGS.with(|cache| {
            cache
                .borrow_mut()
                .insert(gitdir.to_owned(), (key, result.clone()));
        });
    }
    Ok(result)
}

impl Repository {
    fn open(root: &Path, gitdir: &Path) -> anyhow::Result<Self> {
        let common = fs::read_to_string(gitdir.join("commondir"))
            .ok()
            .map(|s| gitdir.join(s.trim()))
            .unwrap_or_else(|| gitdir.to_owned());
        let config = config(&common, gitdir)?;
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        let global = match config.path("core.excludesFile") {
            Some(path) => Some(
                path.interpolate(gix_config::path::interpolate::Context {
                    home_dir: home.as_deref(),
                    ..Default::default()
                })?
                .into_owned(),
            ),
            None => std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| home.map(|p| p.join(".config")))
                .map(|p| p.join("git/ignore")),
        };
        let case_insensitive = config.boolean("core.ignoreCase").and_then(Result::ok);
        #[cfg(windows)]
        let case_insensitive =
            case_insensitive.unwrap_or_else(|| crate::name_fold::NameFolding::probe(root).case);
        #[cfg(not(windows))]
        let case_insensitive = case_insensitive.unwrap_or(false);
        let mut low_priority = Vec::new();
        if let Some(global) = global {
            low_priority.push(matcher(root, &root.join(global), case_insensitive));
        }
        low_priority.push(matcher(
            root,
            &common.join("info/exclude"),
            case_insensitive,
        ));
        Ok(Self {
            root: root.to_owned(),
            tracked: tracked(&gitdir.join("index")),
            low_priority,
            case_insensitive,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Rules {
    root: PathBuf,
    repositories: HashMap<PathBuf, Arc<Repository>>,
    directories: HashMap<PathBuf, (Arc<Repository>, Vec<Arc<Gitignore>>)>,
    decisions: HashMap<Vec<u8>, (bool, bool)>,
}

impl Rules {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn discover(root: &Path) -> anyhow::Result<Option<Self>> {
        let Some((repo_root, dir)) = root
            .ancestors()
            .find_map(|p| git_dir(p).map(|g| (p.to_owned(), g)))
        else {
            return Ok(None);
        };
        let repo = Arc::new(Repository::open(&repo_root, &dir)?);
        Ok(Some(Self {
            root: root.to_owned(),
            repositories: HashMap::from([(repo_root, repo)]),
            directories: HashMap::new(),
            decisions: HashMap::new(),
        }))
    }

    fn directory(&mut self, directory: &Path) -> (Arc<Repository>, Vec<Arc<Gitignore>>) {
        if let Some(value) = self.directories.get(directory) {
            return value.clone();
        }
        let value = if let Some(repo) = self.repositories.get(directory) {
            (
                repo.clone(),
                vec![matcher(
                    directory,
                    &directory.join(".gitignore"),
                    repo.case_insensitive,
                )],
            )
        } else if let Some(dir) = git_dir(directory) {
            // Nested repositories and submodules start a new ignore/index scope.
            let repo = Arc::new(
                Repository::open(directory, &dir).unwrap_or_else(|_| Repository {
                    root: directory.to_owned(),
                    tracked: Arc::default(),
                    low_priority: Vec::new(),
                    case_insensitive: false,
                }),
            );
            self.repositories.insert(directory.to_owned(), repo.clone());
            (
                repo.clone(),
                vec![matcher(
                    directory,
                    &directory.join(".gitignore"),
                    repo.case_insensitive,
                )],
            )
        } else {
            let (repo, mut rules) =
                self.directory(directory.parent().expect("repository ancestor exists"));
            rules.push(matcher(
                directory,
                &directory.join(".gitignore"),
                repo.case_insensitive,
            ));
            (repo, rules)
        };
        self.directories.insert(directory.to_owned(), value.clone());
        value
    }

    fn decision(&mut self, relative: &[u8]) -> (bool, bool) {
        // Git control paths are outside Git's untracked/ignored namespace, including nested repositories.
        if is_git_control_path(relative) {
            return (false, false);
        }
        if relative.is_empty() && self.repositories.contains_key(&self.root) {
            return (false, false);
        }
        if let Some(value) = self.decisions.get(relative) {
            return *value;
        }
        let absolute = self.root.join(bytes_path(relative));
        let is_dir = fs::symlink_metadata(&absolute).is_ok_and(|m| m.is_dir());
        let parent = absolute.parent().unwrap();
        let parent_decision = parent
            .strip_prefix(&self.root)
            .ok()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| self.decision(&git_bytes(p)));
        if parent_decision.is_some_and(|(excluded, directory)| excluded && directory) {
            let result = (true, is_dir);
            self.decisions.insert(relative.to_vec(), result);
            return result;
        }
        let inherited = parent_decision.is_some_and(|(_, directory)| directory);
        let (repo, rules) = if inherited {
            let mut scope = parent;
            while scope != self.root
                && scope
                    .strip_prefix(&self.root)
                    .ok()
                    .is_some_and(|p| self.decision(&git_bytes(p)).1)
            {
                scope = scope.parent().unwrap();
            }
            self.directory(scope)
        } else {
            self.directory(parent)
        };
        let mut ignored = inherited;
        // Once an ancestor is ignored, a descendant negation cannot re-include it.
        let mut cursor = repo.root.clone();
        let components: Vec<_> = absolute
            .strip_prefix(&repo.root)
            .unwrap()
            .components()
            .collect();
        for (i, component) in components.iter().enumerate() {
            if ignored {
                break;
            }
            cursor.push(component);
            let directory = i + 1 < components.len() || is_dir;
            let mut matched = ignore::Match::None;
            for rule in repo.low_priority.iter().chain(rules.iter()) {
                if !cursor.starts_with(rule.path()) || cursor == rule.path() {
                    continue;
                }
                let candidate = rule.matched(&cursor, directory);
                if !candidate.is_none() {
                    matched = candidate;
                }
            }
            if matched.is_ignore() {
                ignored = true;
                break;
            }
        }
        let result = if ignored {
            let tracked_path = git_bytes(absolute.strip_prefix(&repo.root).unwrap());
            let exact = repo.tracked.contains(&tracked_path, repo.case_insensitive);
            let mut prefix = tracked_path.clone();
            prefix.push(b'/');
            let descendant = is_dir && repo.tracked.has_descendant(&prefix, repo.case_insensitive);
            let gitlink = is_dir
                && exact
                && repo
                    .tracked
                    .is_gitlink(&tracked_path, repo.case_insensitive);
            (!exact && !descendant, is_dir && !gitlink)
        } else {
            (false, false)
        };
        self.decisions.insert(relative.to_vec(), result);
        result
    }

    pub(crate) fn excludes(&mut self, relative: &[u8]) -> bool {
        self.decision(relative).0
    }
    pub(crate) fn ignored_directory(&mut self, relative: &[u8]) -> bool {
        self.decision(relative).1
    }

    pub(crate) fn tracked_children(&mut self, relative: &[u8]) -> Option<Vec<OsString>> {
        if !self.ignored_directory(relative) {
            return None;
        }
        let absolute = self.root.join(bytes_path(relative));
        let mut parent = absolute.parent()?;
        while parent != self.root
            && parent
                .strip_prefix(&self.root)
                .ok()
                .is_some_and(|p| self.decision(&git_bytes(p)).1)
        {
            parent = parent.parent()?;
        }
        let (repo, _) = self.directory(parent);
        let mut prefix = git_bytes(absolute.strip_prefix(&repo.root).ok()?);
        prefix.push(b'/');
        Some(repo.tracked.children(&prefix, repo.case_insensitive))
    }

    pub(crate) fn contains_ignored(&mut self, relative: &[u8]) -> bool {
        if is_git_control_path(relative) {
            return false;
        }
        let absolute = self.root.join(bytes_path(relative));
        if !fs::symlink_metadata(&absolute).is_ok_and(|m| m.is_dir()) {
            return false;
        }
        if self.decision(relative).0 {
            return true;
        }
        let Ok(entries) = fs::read_dir(absolute) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_name() == ".git" {
                continue;
            }
            let raw = git_bytes(path.strip_prefix(&self.root).unwrap());
            if self.excludes(&raw) {
                return true;
            }
            if entry.file_type().is_ok_and(|m| m.is_dir()) && self.contains_ignored(&raw) {
                return true;
            }
        }
        false
    }
}

fn is_git_control_path(relative: &[u8]) -> bool {
    relative
        .split(|b| *b == b'/')
        .any(|part| part == b".git" || (cfg!(windows) && part.eq_ignore_ascii_case(b".git")))
}

/// Includes, global excludes and environment overrides can change without an event inside the worktree.
pub(crate) fn control_identity(root: &Path) -> anyhow::Result<Vec<u8>> {
    let Some(gitdir) = root.ancestors().find_map(git_dir) else {
        return Ok(Vec::new());
    };
    let common = fs::read_to_string(gitdir.join("commondir"))
        .ok()
        .map(|s| gitdir.join(s.trim()))
        .unwrap_or_else(|| gitdir.clone());
    let config = config(&common, &gitdir)?;
    let mut files = BTreeSet::from([
        common.join("config"),
        gitdir.join("config.worktree"),
        gitdir.join("HEAD"),
        gitdir.join("index"),
        common.join("info/exclude"),
    ]);
    for section in config.sections() {
        if let Some(path) = &section.meta().path {
            files.insert(path.clone());
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let excludes = match config.path("core.excludesFile") {
        Some(path) => Some(
            path.interpolate(gix_config::path::interpolate::Context {
                home_dir: home.as_deref(),
                ..Default::default()
            })?
            .into_owned(),
        ),
        None => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|p| p.join(".config")))
            .map(|p| p.join("git/ignore")),
    };
    if let Some(path) = excludes {
        files.insert(root.join(path));
    }
    let mut result = Vec::new();
    for path in files {
        result.extend_from_slice(path.as_os_str().as_encoded_bytes());
        let stamp = fs::metadata(&path)
            .ok()
            .map(|m| (m.len(), m.modified().ok(), m.created().ok()));
        result.extend_from_slice(format!("{stamp:?}").as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let stamp = fs::metadata(&path)
                .ok()
                .map(|m| (m.dev(), m.ino(), m.ctime(), m.ctime_nsec()));
            result.extend_from_slice(format!("{stamp:?}").as_bytes());
        }
        #[cfg(windows)]
        result.extend_from_slice(
            format!("{:?}", crate::platform::path_identity(&path).ok()).as_bytes(),
        );
    }
    let mut env: Vec<_> = std::env::vars_os()
        .filter(|(key, _)| key.to_string_lossy().starts_with("GIT_CONFIG"))
        .collect();
    env.sort();
    result.extend_from_slice(format!("{env:?}").as_bytes());
    Ok(blake3::hash(&result).as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g1_unchanged_index_is_read_once_and_replacement_is_reloaded() {
        // G1 requires one index read per unchanged identity and a fresh read after atomic replacement.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        fs::write(root.join("one.log"), "one").unwrap();
        fs::write(root.join("two.log"), "two").unwrap();
        git(&["add", "one.log"]);
        let index = root.join(".git/index");
        let before = INDEX_READS.with(|reads| reads.get());
        assert!(tracked(&index).contains(b"one.log".as_slice(), false));
        assert!(tracked(&index).contains(b"one.log".as_slice(), false));
        assert_eq!(INDEX_READS.with(|reads| reads.get()), before + 1);
        let metadata = fs::metadata(&index).unwrap();
        git(&["rm", "--cached", "one.log"]);
        git(&["add", "two.log"]);
        let bytes = fs::read(&index).unwrap();
        assert_eq!(bytes.len() as u64, metadata.len());
        let replacement = root.join("new-index");
        fs::write(&replacement, bytes).unwrap();
        filetime::set_file_mtime(
            &replacement,
            filetime::FileTime::from_last_modification_time(&metadata),
        )
        .unwrap();
        fs::rename(replacement, &index).unwrap();
        let paths = tracked(&index);
        assert!(!paths.contains(b"one.log".as_slice(), false));
        assert!(paths.contains(b"two.log".as_slice(), false));
        assert_eq!(INDEX_READS.with(|reads| reads.get()), before + 2);
    }
}
