//! Reader for a project's `.herdr-dev.toml`.
//!
//! Two tiers of complaint, because a typo must not lock you out of a project: anything attributable
//! to a single unit leaves the unit in the model marked broken, while a problem with the file as a
//! whole is a `LoadError`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item};

const TOP_LEVEL_KEYS: [&str; 4] = ["env", "local", "docker", "includes"];
const LOCAL_KEYS: [&str; 4] = ["cmd", "cwd", "env", "tty"];
const DOCKER_KEYS: [&str; 4] = ["names", "one_shot", "hidden", "notes"];
const INCLUDE_KEYS: [&str; 1] = ["path"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub root: PathBuf,
    pub manifest: PathBuf,
    pub name: String,
    pub env: BTreeMap<String, String>,
    pub local: Vec<LocalUnit>,
    pub docker: Vec<DockerService>,
    pub includes: Vec<Include>,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalUnit {
    pub name: String,
    pub cmd: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    /// A controlling terminal of its own instead of a pipe, for a unit that saves its interactive
    /// behaviour — a prompt raised mid-request, a pager, colour — for one.
    pub tty: bool,
    pub problem: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerService {
    pub name: String,
    pub one_shot: bool,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Include {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug)]
pub enum LoadError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Syntax {
        path: PathBuf,
        message: String,
    },
    Schema {
        path: PathBuf,
        message: String,
    },
}

impl LoadError {
    pub fn path(&self) -> &Path {
        match self {
            LoadError::Read { path, .. }
            | LoadError::Syntax { path, .. }
            | LoadError::Schema { path, .. } => path,
        }
    }

    /// One line, and nothing about where: a `toml_edit` complaint runs to several lines with an
    /// excerpt under them, and a row that already names the repo has room for one.
    pub fn terse(&self) -> String {
        let whole = match self {
            LoadError::Read { source, .. } => source.to_string(),
            LoadError::Syntax { message, .. } | LoadError::Schema { message, .. } => {
                message.clone()
            }
        };
        whole.lines().next().unwrap_or_default().trim().to_string()
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Read { path, source } => write!(f, "{}: {source}", path.display()),
            LoadError::Syntax { path, message } | LoadError::Schema { path, message } => {
                write!(f, "{}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LoadError::Read { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl Project {
    pub fn load(manifest: impl AsRef<Path>) -> Result<Project, LoadError> {
        let manifest = manifest.as_ref();
        let text = std::fs::read_to_string(manifest).map_err(|source| LoadError::Read {
            path: manifest.to_path_buf(),
            source,
        })?;
        Project::parse(&text, manifest)
    }

    pub fn parse(text: &str, manifest: &Path) -> Result<Project, LoadError> {
        let doc = text
            .parse::<DocumentMut>()
            .map_err(|source| LoadError::Syntax {
                path: manifest.to_path_buf(),
                message: source.to_string(),
            })?;
        build(&doc, manifest).map_err(|message| LoadError::Schema {
            path: manifest.to_path_buf(),
            message,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.local.is_empty() && self.docker.is_empty() && self.includes.is_empty()
    }
}

impl LocalUnit {
    pub fn is_broken(&self) -> bool {
        self.problem.is_some()
    }

    /// Innermost last: `base`, then the manifest's `[env]`, then this unit's own `env` — the latter
    /// two already merged into `self.env`.
    pub fn env_over<I, K, V>(&self, base: I) -> BTreeMap<String, String>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut merged: BTreeMap<String, String> = base
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        merged.extend(self.env.clone());
        merged
    }
}

fn build(doc: &DocumentMut, manifest: &Path) -> Result<Project, String> {
    for (key, _) in doc.iter() {
        if !TOP_LEVEL_KEYS.contains(&key) {
            return Err(format!(
                "unknown top-level key `{key}`; expected one of {}",
                keys(&TOP_LEVEL_KEYS)
            ));
        }
    }

    let root = manifest
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let name = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());

    let env = match doc.get("env") {
        None => BTreeMap::new(),
        Some(item) => string_table(item, "env")?,
    };

    let mut problems = Vec::new();

    let local = match doc.get("local") {
        None => Vec::new(),
        Some(item) => {
            let table = item.as_table_like().ok_or_else(|| {
                format!(
                    "`local` must be a table of units, found {}",
                    item.type_name()
                )
            })?;
            table
                .iter()
                .map(|(name, item)| local_unit(name, item, &env, &root))
                .collect()
        }
    };
    problems.extend(
        local
            .iter()
            .filter_map(|unit: &LocalUnit| unit.problem.clone()),
    );

    let docker = match doc.get("docker") {
        None => Vec::new(),
        Some(item) => docker_services(item, &mut problems)?,
    };

    let includes = match doc.get("includes") {
        None => Vec::new(),
        Some(item) => includes(item, &root, &mut problems)?,
    };

    Ok(Project {
        root,
        manifest: manifest.to_path_buf(),
        name,
        env,
        local,
        docker,
        includes,
        problems,
    })
}

fn local_unit(
    name: &str,
    item: &Item,
    top_level_env: &BTreeMap<String, String>,
    root: &Path,
) -> LocalUnit {
    match local_fields(name, item, root) {
        Ok(Fields { cmd, cwd, env, tty }) => LocalUnit {
            name: name.to_string(),
            cmd,
            cwd: cwd.unwrap_or_else(|| root.to_path_buf()),
            env: layered(top_level_env, env),
            tty,
            problem: None,
        },
        Err(problem) => LocalUnit {
            name: name.to_string(),
            cmd: Vec::new(),
            cwd: root.to_path_buf(),
            env: top_level_env.clone(),
            tty: false,
            problem: Some(problem),
        },
    }
}

struct Fields {
    cmd: Vec<String>,
    cwd: Option<PathBuf>,
    env: BTreeMap<String, String>,
    tty: bool,
}

fn local_fields(name: &str, item: &Item, root: &Path) -> Result<Fields, String> {
    let table = item
        .as_table_like()
        .ok_or_else(|| format!("[local.{name}] must be a table, found {}", item.type_name()))?;

    for (key, _) in table.iter() {
        if !LOCAL_KEYS.contains(&key) {
            return Err(format!(
                "[local.{name}] has unknown key `{key}`; expected one of {}",
                keys(&LOCAL_KEYS)
            ));
        }
    }

    let cmd = match table.get("cmd") {
        None => return Err(format!("[local.{name}] has no `cmd`")),
        Some(item) => string_array(item).map_err(|why| format!("[local.{name}] `cmd` {why}"))?,
    };
    if cmd.is_empty() {
        return Err(format!("[local.{name}] `cmd` is an empty array"));
    }

    let cwd = match table.get("cwd") {
        None => None,
        Some(item) => Some(resolve_path(
            item.as_str().ok_or_else(|| {
                format!(
                    "[local.{name}] `cwd` must be a string, found {}",
                    item.type_name()
                )
            })?,
            root,
        )),
    };

    let env = match table.get("env") {
        None => BTreeMap::new(),
        Some(item) => string_table(item, &format!("local.{name}.env"))?,
    };

    let tty = match table.get("tty") {
        None => false,
        Some(item) => item.as_bool().ok_or_else(|| {
            format!(
                "[local.{name}] `tty` must be a boolean, found {}",
                item.type_name()
            )
        })?,
    };

    Ok(Fields { cmd, cwd, env, tty })
}

fn docker_services(item: &Item, problems: &mut Vec<String>) -> Result<Vec<DockerService>, String> {
    let table = item
        .as_table_like()
        .ok_or_else(|| format!("`docker` must be a table, found {}", item.type_name()))?;

    for (key, _) in table.iter() {
        if !DOCKER_KEYS.contains(&key) {
            return Err(format!(
                "[docker] has unknown key `{key}`; expected one of {}",
                keys(&DOCKER_KEYS)
            ));
        }
    }

    let names = string_array_at(table, "names", "docker.names")?;
    let one_shot = string_array_at(table, "one_shot", "docker.one_shot")?;
    let hidden = string_array_at(table, "hidden", "docker.hidden")?;
    let notes = match table.get("notes") {
        None => BTreeMap::new(),
        Some(item) => string_table(item, "docker.notes")?,
    };

    let mut declared: BTreeSet<&str> = BTreeSet::new();
    for name in &names {
        if !declared.insert(name.as_str()) {
            return Err(format!("`docker.names` lists `{name}` twice"));
        }
    }
    let hidden: BTreeSet<&str> = hidden.iter().map(String::as_str).collect();
    let one_shot: BTreeSet<&str> = one_shot.iter().map(String::as_str).collect();

    for name in &one_shot {
        if !declared.contains(name) && !hidden.contains(name) {
            problems.push(format!(
                "`docker.one_shot` names `{name}`, which is in neither `names` nor `hidden`"
            ));
        }
    }
    for name in notes.keys() {
        if !declared.contains(name.as_str()) {
            let why = if hidden.contains(name.as_str()) {
                "which is hidden, so the note is never shown"
            } else {
                "which is not in `names`"
            };
            problems.push(format!("[docker.notes] has a note for `{name}`, {why}"));
        }
    }

    let mut services = Vec::new();
    for name in names {
        if hidden.contains(name.as_str()) {
            problems.push(format!(
                "`{name}` is in both `names` and `hidden`; hidden wins, so it is not shown"
            ));
            continue;
        }
        services.push(DockerService {
            one_shot: one_shot.contains(name.as_str()),
            note: notes.get(&name).cloned(),
            name,
        });
    }
    Ok(services)
}

fn includes(item: &Item, root: &Path, problems: &mut Vec<String>) -> Result<Vec<Include>, String> {
    let table = item
        .as_table_like()
        .ok_or_else(|| format!("`includes` must be a table, found {}", item.type_name()))?;

    let mut includes = Vec::new();
    for (name, item) in table.iter() {
        match include(name, item, root) {
            Ok(include) => includes.push(include),
            Err(problem) => problems.push(problem),
        }
    }
    Ok(includes)
}

fn include(name: &str, item: &Item, root: &Path) -> Result<Include, String> {
    let table = item.as_table_like().ok_or_else(|| {
        format!(
            "[includes.{name}] must be a table, found {}",
            item.type_name()
        )
    })?;
    for (key, _) in table.iter() {
        if !INCLUDE_KEYS.contains(&key) {
            return Err(format!(
                "[includes.{name}] has unknown key `{key}`; expected one of {}",
                keys(&INCLUDE_KEYS)
            ));
        }
    }
    let path = table
        .get("path")
        .ok_or_else(|| format!("[includes.{name}] has no `path`"))?;
    let path = path.as_str().ok_or_else(|| {
        format!(
            "[includes.{name}] `path` must be a string, found {}",
            path.type_name()
        )
    })?;
    Ok(Include {
        name: name.to_string(),
        path: normalize(resolve_path(path, root)),
    })
}

fn string_array_at(
    table: &dyn toml_edit::TableLike,
    key: &str,
    label: &str,
) -> Result<Vec<String>, String> {
    match table.get(key) {
        None => Ok(Vec::new()),
        Some(item) => string_array(item).map_err(|why| format!("`{label}` {why}")),
    }
}

fn string_array(item: &Item) -> Result<Vec<String>, String> {
    let array = item
        .as_array()
        .ok_or_else(|| format!("must be an array of strings, found {}", item.type_name()))?;
    let mut strings = Vec::with_capacity(array.len());
    for (index, value) in array.iter().enumerate() {
        match value.as_str() {
            Some(string) => strings.push(string.to_string()),
            None => {
                return Err(format!(
                    "must be an array of strings, but element {index} is {}",
                    value.type_name()
                ));
            }
        }
    }
    Ok(strings)
}

fn string_table(item: &Item, label: &str) -> Result<BTreeMap<String, String>, String> {
    let table = item.as_table_like().ok_or_else(|| {
        format!(
            "`{label}` must be a table of strings, found {}",
            item.type_name()
        )
    })?;
    let mut strings = BTreeMap::new();
    for (key, value) in table.iter() {
        match value.as_str() {
            Some(string) => {
                strings.insert(key.to_string(), string.to_string());
            }
            None => {
                return Err(format!(
                    "`{label}.{key}` must be a string, found {}",
                    value.type_name()
                ));
            }
        }
    }
    Ok(strings)
}

fn layered(
    base: &BTreeMap<String, String>,
    over: BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut merged = base.clone();
    merged.extend(over);
    merged
}

fn resolve_path(raw: &str, root: &Path) -> PathBuf {
    let path = expand_tilde(raw);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

/// Lexical only: `.` and `..` are folded away and nothing is asked of the filesystem. A project is
/// keyed by its path as written, so `~/acme/storefront/../accounts` has to key like the plain
/// spelling of the same repo — while a symlink and its target stay two projects, exactly as two
/// checkouts of one repo already are.
fn normalize(path: PathBuf) -> PathBuf {
    let mut folded = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !folded.pop() {
                    folded.push("..");
                }
            }
            component => folded.push(component),
        }
    }
    match folded.as_os_str().is_empty() {
        true => PathBuf::from("."),
        false => folded,
    }
}

/// `~` and `~/…` only; `~user` needs a passwd lookup and is left as written.
fn expand_tilde(raw: &str) -> PathBuf {
    let rest = match raw.strip_prefix('~') {
        None => return PathBuf::from(raw),
        Some("") => "",
        Some(rest) => match rest.strip_prefix('/') {
            Some(rest) => rest,
            None => return PathBuf::from(raw),
        },
    };
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => Path::new(&home).join(rest),
        _ => PathBuf::from(raw),
    }
}

fn keys(keys: &[&str]) -> String {
    keys.iter()
        .map(|key| format!("`{key}`"))
        .collect::<Vec<_>>()
        .join(", ")
}
