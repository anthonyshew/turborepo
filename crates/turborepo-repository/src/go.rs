//! Experimental Go module workspace support.
//!
//! A Go module is the package-graph boundary. A root `go.work` supplies the
//! module list; without one, a root `go.mod` is treated as a single module.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    io,
    process::Command,
    sync::Arc,
};

use serde::Deserialize;
use turbopath::{AbsoluteSystemPath, AbsoluteSystemPathBuf, AnchoredSystemPathBuf};

use crate::{
    change_knowledge::ChangeObservation,
    external_resolution::{
        ExternalPackageIdentity, ExternalResolutionData, ExternalResolutionDomain,
        PackageResolution, ResolutionCompleteness,
    },
    native_tasks::NativeTask,
    package_json::{DependencyKind, PackageJson},
    prune_knowledge::{PruneDomain, PrunePlan},
    relationships::{Relationship, RelationshipTarget},
    task_contracts::{ScopeTaskContract, TaskEntrypoint},
    toolchain::{
        DerivedTaskIO, DiscoverPackagesFuture, DiscoveredPackage, DiscoveredPackages,
        RepositoryContributor, TaskDefaults, ToolchainId, WorkspaceRoot,
    },
};

pub const GO_WORK: &str = "go.work";
pub const GO_WORK_SUM: &str = "go.work.sum";
pub const GO_MOD: &str = "go.mod";
pub const GO_SUM: &str = "go.sum";

pub(crate) const HASHED_ENV_VARS: &[&str] = &[
    "CC",
    "CGO_CFLAGS",
    "CGO_CPPFLAGS",
    "CGO_CXXFLAGS",
    "CGO_ENABLED",
    "CGO_FFLAGS",
    "CGO_LDFLAGS",
    "CXX",
    "GO386",
    "GOAMD64",
    "GOARCH",
    "GOARM",
    "GOARM64",
    "GOEXPERIMENT",
    "GOFLAGS",
    "GOMIPS",
    "GOMIPS64",
    "GOOS",
    "GOPPC64",
    "GORISCV64",
    "GOWORK",
    "PKG_CONFIG",
];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to read {path}: {source}")]
    Read { path: String, source: io::Error },
    #[error("invalid {path}: {message}")]
    Parse { path: String, message: String },
    #[error("Go module path is missing from {0}")]
    MissingModule(String),
    #[error("Go workspace module {path} has no go.mod")]
    MissingManifest { path: String },
    #[error("Go workspace path {path} resolves outside repository root {root}")]
    OutsideRepository { path: String, root: String },
    #[error("Go workspace contains duplicate module path {name}")]
    DuplicateModule { name: String },
    #[error("local Go replacement {path} is not a discovered workspace module")]
    UnknownLocalReplacement { path: String },
    #[error(
        "Go dependency {path}@{version} is missing module or go.mod checksum data; run `go mod \
         download` and commit the resulting go.sum changes"
    )]
    MissingChecksum { path: String, version: String },
    #[error("failed to run `{command}` in {cwd}: {source}")]
    Spawn {
        command: String,
        cwd: String,
        source: io::Error,
    },
    #[error("`{command}` failed in {cwd}: {stderr}")]
    Command {
        command: String,
        cwd: String,
        stderr: String,
    },
    #[error("invalid JSON from `{command}` in {cwd}: {source}")]
    Json {
        command: String,
        cwd: String,
        source: serde_json::Error,
    },
    #[error(transparent)]
    Path(#[from] turbopath::PathError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Replacement {
    old: String,
    old_version: Option<String>,
    new: String,
    version: Option<String>,
}

impl Replacement {
    fn render(&self) -> String {
        format!(
            "replace {}{} => {}{}",
            self.old,
            self.old_version
                .as_ref()
                .map(|version| format!(" {version}"))
                .unwrap_or_default(),
            self.new,
            self.version
                .as_ref()
                .map(|version| format!(" {version}"))
                .unwrap_or_default()
        )
    }
}

#[derive(Debug, Clone)]
struct ModuleManifest {
    name: String,
    requires: Vec<(String, String)>,
    replacements: Vec<Replacement>,
}

#[derive(Debug, Clone)]
struct ModuleInfo {
    name: String,
    directory: AbsoluteSystemPathBuf,
    manifest_path: AbsoluteSystemPathBuf,
    manifest: ModuleManifest,
}

#[derive(Debug, Default)]
struct WorkFile {
    go_version: Option<String>,
    toolchain: Option<String>,
    uses: Vec<String>,
    replacements: Vec<Replacement>,
}

fn read(path: &AbsoluteSystemPath) -> Result<String, Error> {
    std::fs::read_to_string(path.as_std_path()).map_err(|source| Error::Read {
        path: path.to_string(),
        source,
    })
}

fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    let mut raw = false;
    let mut escaped = false;
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if quoted => escaped = !escaped,
            b'"' if !raw && !escaped => quoted = !quoted,
            b'`' if !quoted => raw = !raw,
            b'/' if !quoted && !raw && index + 1 < bytes.len() && bytes[index + 1] == b'/' => {
                return &line[..index];
            }
            _ => escaped = false,
        }
        index += 1;
    }
    line
}

fn unquote(value: &str) -> Result<String, String> {
    if value.starts_with('"') {
        serde_json::from_str(value).map_err(|error| error.to_string())
    } else if value.starts_with('`') && value.ends_with('`') && value.len() >= 2 {
        Ok(value[1..value.len() - 1].to_string())
    } else {
        Ok(value.to_string())
    }
}

fn directive_lines(contents: &str) -> Vec<(String, Vec<String>)> {
    let mut result = Vec::new();
    let mut block: Option<String> = None;
    for raw in contents.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line == ")" {
            block = None;
            continue;
        }
        let fields: Vec<String> = line.split_whitespace().map(str::to_string).collect();
        if fields.is_empty() {
            continue;
        }
        if fields.len() == 2 && fields[1] == "(" {
            block = Some(fields[0].clone());
            continue;
        }
        if let Some(kind) = &block {
            result.push((kind.clone(), fields));
        } else {
            result.push((fields[0].clone(), fields[1..].to_vec()));
        }
    }
    result
}

fn parse_replacement(fields: &[String]) -> Option<Replacement> {
    let arrow = fields.iter().position(|field| field == "=>")?;
    if arrow == 0 || arrow + 1 >= fields.len() {
        return None;
    }
    Some(Replacement {
        old: fields[0].clone(),
        old_version: (arrow == 2).then(|| fields[1].clone()),
        new: fields[arrow + 1].clone(),
        version: fields.get(arrow + 2).cloned(),
    })
}

fn parse_work(path: &AbsoluteSystemPath, contents: &str) -> Result<WorkFile, Error> {
    let mut work = WorkFile::default();
    for (kind, fields) in directive_lines(contents) {
        match kind.as_str() {
            "go" => work.go_version = fields.first().cloned(),
            "toolchain" => work.toolchain = fields.first().cloned(),
            "use" => {
                let value = fields.first().ok_or_else(|| Error::Parse {
                    path: path.to_string(),
                    message: "use directive requires a module directory".to_string(),
                })?;
                work.uses
                    .push(unquote(value).map_err(|message| Error::Parse {
                        path: path.to_string(),
                        message,
                    })?);
            }
            "replace" => {
                let replacement = parse_replacement(&fields).ok_or_else(|| Error::Parse {
                    path: path.to_string(),
                    message: "invalid replace directive".to_string(),
                })?;
                work.replacements.push(replacement);
            }
            _ => {}
        }
    }
    Ok(work)
}

fn parse_mod(path: &AbsoluteSystemPath, contents: &str) -> Result<ModuleManifest, Error> {
    let mut name = None;
    let mut requires = Vec::new();
    let mut replacements = Vec::new();
    for (kind, fields) in directive_lines(contents) {
        match kind.as_str() {
            "module" => name = fields.first().cloned(),
            "require" if fields.len() >= 2 => {
                requires.push((fields[0].clone(), fields[1].clone()));
            }
            "replace" => {
                let replacement = parse_replacement(&fields).ok_or_else(|| Error::Parse {
                    path: path.to_string(),
                    message: "invalid replace directive".to_string(),
                })?;
                replacements.push(replacement);
            }
            _ => {}
        }
    }
    let name = name.ok_or_else(|| Error::MissingModule(path.to_string()))?;
    Ok(ModuleManifest {
        name,
        requires,
        replacements,
    })
}

fn canonical_module_dir(
    repo_root: &AbsoluteSystemPath,
    raw: &str,
) -> Result<AbsoluteSystemPathBuf, Error> {
    let candidate = if std::path::Path::new(raw).is_absolute() {
        std::path::PathBuf::from(raw)
    } else {
        repo_root.as_std_path().join(raw)
    };
    let canonical = std::fs::canonicalize(&candidate).map_err(|source| Error::Read {
        path: candidate.display().to_string(),
        source,
    })?;
    let canonical = AbsoluteSystemPathBuf::try_from(canonical.as_path())?;
    if repo_root.anchor(&canonical).is_err() {
        return Err(Error::OutsideRepository {
            path: raw.to_string(),
            root: repo_root.to_string(),
        });
    }
    Ok(canonical)
}

fn discover_modules(repo_root: &AbsoluteSystemPath) -> Result<(Vec<ModuleInfo>, WorkFile), Error> {
    let work_path = repo_root.join_component(GO_WORK);
    let work = if work_path.exists() {
        parse_work(&work_path, &read(&work_path)?)?
    } else {
        WorkFile {
            uses: vec![".".to_string()],
            ..Default::default()
        }
    };
    let mut modules = Vec::with_capacity(work.uses.len());
    let mut names = HashSet::new();
    for use_path in &work.uses {
        let directory = canonical_module_dir(repo_root, use_path)?;
        let manifest_path = directory.join_component(GO_MOD);
        if !manifest_path.exists() {
            return Err(Error::MissingManifest {
                path: use_path.clone(),
            });
        }
        let manifest = parse_mod(&manifest_path, &read(&manifest_path)?)?;
        if !names.insert(manifest.name.clone()) {
            return Err(Error::DuplicateModule {
                name: manifest.name,
            });
        }
        modules.push(ModuleInfo {
            name: manifest.name.clone(),
            directory,
            manifest_path,
            manifest,
        });
    }
    Ok((modules, work))
}

fn is_local_path(value: &str) -> bool {
    value == "."
        || value == ".."
        || value.starts_with("./")
        || value.starts_with("../")
        || std::path::Path::new(value).is_absolute()
}

fn replacement_target(
    repo_root: &AbsoluteSystemPath,
    module_dir: &AbsoluteSystemPath,
    replacement: &Replacement,
    modules_by_dir: &HashMap<AbsoluteSystemPathBuf, String>,
) -> Result<Option<String>, Error> {
    if !is_local_path(&replacement.new) {
        return Ok(None);
    }
    let candidate = if std::path::Path::new(&replacement.new).is_absolute() {
        std::path::PathBuf::from(&replacement.new)
    } else {
        module_dir.as_std_path().join(&replacement.new)
    };
    let canonical = std::fs::canonicalize(&candidate).map_err(|source| Error::Read {
        path: candidate.display().to_string(),
        source,
    })?;
    let canonical = AbsoluteSystemPathBuf::try_from(canonical.as_path())?;
    if repo_root.anchor(&canonical).is_err() {
        return Err(Error::OutsideRepository {
            path: replacement.new.clone(),
            root: repo_root.to_string(),
        });
    }
    modules_by_dir
        .get(&canonical)
        .cloned()
        .map(Some)
        .ok_or_else(|| Error::UnknownLocalReplacement {
            path: replacement.new.clone(),
        })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GoListPackage {
    #[serde(default)]
    name: String,
    #[serde(default)]
    import_path: String,
    #[serde(default)]
    imports: Vec<String>,
    module: Option<GoListModule>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GoListModule {
    path: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    sum: String,
    #[serde(default)]
    go_mod_sum: String,
    #[serde(default)]
    main: bool,
    replace: Option<Box<GoListModule>>,
    #[serde(default)]
    dir: String,
}

fn go_json<T: for<'de> Deserialize<'de>>(
    cwd: &AbsoluteSystemPath,
    args: &[&str],
) -> Result<Vec<T>, Error> {
    let command = format!("go {}", args.join(" "));
    let output = Command::new("go")
        .args(args)
        .current_dir(cwd.as_std_path())
        .env("GOTOOLCHAIN", "local")
        .output()
        .map_err(|source| Error::Spawn {
            command: command.clone(),
            cwd: cwd.to_string(),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::Command {
            command,
            cwd: cwd.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    serde_json::Deserializer::from_slice(&output.stdout)
        .into_iter::<T>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| Error::Json {
            command,
            cwd: cwd.to_string(),
            source,
        })
}

fn go_version(repo_root: &AbsoluteSystemPath) -> Result<String, Error> {
    let output = Command::new("go")
        .arg("version")
        .current_dir(repo_root.as_std_path())
        .env("GOTOOLCHAIN", "local")
        .output()
        .map_err(|source| Error::Spawn {
            command: "go version".to_string(),
            cwd: repo_root.to_string(),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::Command {
            command: "go version".to_string(),
            cwd: repo_root.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn module_identity(module: &GoListModule) -> Result<Option<ExternalPackageIdentity>, Error> {
    if module.main {
        return Ok(None);
    }
    let effective = module.replace.as_deref().unwrap_or(module);
    if !effective.dir.is_empty() && effective.version.is_empty() {
        return Ok(None);
    }
    if effective.version.is_empty() || effective.sum.is_empty() || effective.go_mod_sum.is_empty() {
        return Err(Error::MissingChecksum {
            path: effective.path.clone(),
            version: effective.version.clone(),
        });
    }
    let key = if module.replace.is_some() {
        format!("{}=>{}", module.path, effective.path)
    } else {
        module.path.clone()
    };
    let version = format!(
        "{}#{}#{}",
        effective.version, effective.sum, effective.go_mod_sum
    );
    Ok(Some(
        ExternalPackageIdentity::new(key, version).with_human_name(module.path.clone()),
    ))
}

fn module_sum_keys(module: &GoListModule) -> BTreeSet<(String, String)> {
    if module.main {
        return BTreeSet::new();
    }
    let mut keys = BTreeSet::new();
    if !module.version.is_empty() {
        keys.insert((module.path.clone(), module.version.clone()));
    }
    if let Some(replacement) = module.replace.as_deref()
        && !replacement.version.is_empty()
    {
        keys.insert((replacement.path.clone(), replacement.version.clone()));
    }
    keys
}

fn prune_sum(contents: &str, keys: &BTreeSet<(String, String)>) -> String {
    let mut output = String::new();
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        let Some(path) = fields.next() else { continue };
        let Some(version) = fields.next() else {
            continue;
        };
        let version = version.strip_suffix("/go.mod").unwrap_or(version);
        if keys.contains(&(path.to_string(), version.to_string())) {
            output.push_str(line);
            output.push('\n');
        }
    }
    output
}

#[derive(Debug)]
struct ModuleSumKnowledge {
    path: String,
    contents: String,
    keys: BTreeSet<(String, String)>,
}

fn native_tasks(main: Option<&str>) -> Vec<NativeTask> {
    let mut tasks = vec![
        NativeTask::go(
            "build",
            "go build ./...".to_string(),
            "build",
            vec!["./...".into()],
        ),
        NativeTask::go(
            "test",
            "go test ./...".to_string(),
            "test",
            vec!["./...".into()],
        ),
        NativeTask::go(
            "check",
            "go vet ./...".to_string(),
            "vet",
            vec!["./...".into()],
        ),
        NativeTask::go(
            "lint",
            "go vet ./...".to_string(),
            "vet",
            vec!["./...".into()],
        ),
        NativeTask::go(
            "format",
            "go fmt ./...".to_string(),
            "fmt",
            vec!["./...".into()],
        ),
    ];
    if let Some(main) = main {
        for name in ["run", "dev"] {
            tasks.push(NativeTask::go(
                name,
                format!("go run {main}"),
                "run",
                vec![main.to_string()],
            ));
        }
    }
    tasks
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GoTaskContract;

impl GoTaskContract {
    pub(crate) fn derives_task_io(&self, task: &str) -> bool {
        matches!(
            task,
            "build" | "test" | "check" | "lint" | "format" | "run" | "dev"
        )
    }

    pub(crate) fn derived_task_io(
        &self,
        _package: &crate::package_graph::PackageTaskContext<'_>,
        task: &str,
        path_to_root: &str,
        dependencies: &[crate::package_graph::PackageTaskContext<'_>],
        wants_automatic_inputs: bool,
        _context: &crate::toolchain::TaskIOContext<'_>,
    ) -> Option<DerivedTaskIO> {
        if !self.derives_task_io(task) {
            return None;
        }
        let mut io = DerivedTaskIO {
            env: HASHED_ENV_VARS.iter().map(|var| var.to_string()).collect(),
            ..Default::default()
        };
        if wants_automatic_inputs {
            io.package_default_inputs = Some(true);
            for dependency in dependencies.iter().filter(|dependency| {
                dependency.task_contract().dependency_source_inputs()
                    == crate::task_contracts::DependencySourceInputs::Include
            }) {
                let directory = dependency.directory().to_unix();
                let prefix = match (path_to_root.is_empty(), directory.is_empty()) {
                    (true, true) => ".".to_string(),
                    (true, false) => directory.to_string(),
                    (false, true) => path_to_root.to_string(),
                    (false, false) => format!("{path_to_root}/{directory}"),
                };
                io.input_globs.push(format!("{prefix}/**"));
                io.input_globs.push(format!("!{prefix}/.turbo/**"));
                io.input_globs.push(format!("!{prefix}/.cache/go-build/**"));
            }
            io.input_globs.sort();
            io.input_globs.dedup();
        }
        Some(io)
    }
}

fn task_contract(is_main: bool) -> ScopeTaskContract {
    let defaults = BTreeMap::from([
        ("format".to_string(), TaskDefaults { cache: Some(false) }),
        ("dev".to_string(), TaskDefaults { cache: Some(false) }),
        ("run".to_string(), TaskDefaults { cache: Some(false) }),
    ]);
    let build_entrypoint = if is_main {
        TaskEntrypoint::Preferred
    } else {
        TaskEntrypoint::Candidate
    };
    let mut entrypoints = BTreeMap::from([("build".to_string(), build_entrypoint)]);
    if is_main {
        entrypoints.insert("run".to_string(), TaskEntrypoint::Preferred);
        entrypoints.insert("dev".to_string(), TaskEntrypoint::Preferred);
    }
    ScopeTaskContract::go(GoTaskContract, defaults, entrypoints)
}

#[derive(Debug)]
struct GoPruneKnowledge {
    directories: BTreeMap<String, String>,
    dependencies: HashMap<String, Vec<String>>,
    go_version: Option<String>,
    toolchain: Option<String>,
    replacements: Vec<Replacement>,
    requirements: HashMap<String, BTreeSet<String>>,
    module_sums: HashMap<String, ModuleSumKnowledge>,
    work_sum: Option<String>,
    workspace: bool,
}

impl PruneDomain for GoPruneKnowledge {
    fn id(&self) -> &crate::prune_knowledge::PruneDomainId {
        &crate::prune_knowledge::GO_PRUNE_DOMAIN
    }

    fn plan(
        &self,
        kept_packages: &[String],
    ) -> Result<Option<PrunePlan>, crate::prune_knowledge::Error> {
        if kept_packages.is_empty() {
            return Ok(None);
        }
        let requested: HashSet<&str> = kept_packages.iter().map(String::as_str).collect();
        let mut retained: BTreeSet<String> = kept_packages.iter().cloned().collect();
        let mut queue: VecDeque<String> = kept_packages.iter().cloned().collect();
        while let Some(package) = queue.pop_front() {
            for dependency in self.dependencies.get(&package).into_iter().flatten() {
                if retained.insert(dependency.clone()) {
                    queue.push_back(dependency.clone());
                }
            }
        }
        let extra_packages = retained
            .iter()
            .filter(|name| !requested.contains(name.as_str()))
            .cloned()
            .collect();
        let mut root_files = Vec::new();
        let sum_keys: BTreeSet<_> = retained
            .iter()
            .filter_map(|package| self.module_sums.get(package))
            .flat_map(|sum| sum.keys.iter().cloned())
            .collect();
        for package in &retained {
            if let Some(sum) = self.module_sums.get(package) {
                root_files.push((sum.path.clone(), prune_sum(&sum.contents, &sum.keys)));
            }
        }
        if self.workspace {
            let mut work = format!("go {}\n", self.go_version.as_deref().unwrap_or("1.18"));
            if let Some(toolchain) = &self.toolchain {
                work.push_str(&format!("toolchain {toolchain}\n"));
            }
            work.push_str("\nuse (\n");
            for name in &retained {
                if let Some(directory) = self.directories.get(name) {
                    let directory = directory.trim_end_matches('/');
                    if directory.is_empty() {
                        work.push_str("\t.\n");
                    } else {
                        work.push_str(&format!("\t./{directory}\n"));
                    }
                }
            }
            work.push_str(")\n");
            let required: BTreeSet<_> = retained
                .iter()
                .filter_map(|package| self.requirements.get(package))
                .flat_map(|requirements| requirements.iter())
                .collect();
            for replacement in self
                .replacements
                .iter()
                .filter(|replacement| required.contains(&replacement.old))
            {
                work.push_str(&format!("\n{}\n", replacement.render()));
            }
            root_files.push((GO_WORK.to_string(), work));
            if let Some(work_sum) = &self.work_sum {
                root_files.push((GO_WORK_SUM.to_string(), prune_sum(work_sum, &sum_keys)));
            }
        }
        Ok(Some(PrunePlan {
            extra_packages,
            root_files,
            copy_paths: Vec::new(),
        }))
    }
}

fn change_observation(modules: &[ModuleInfo], repo_root: &AbsoluteSystemPath) -> ChangeObservation {
    let mut observation = ChangeObservation::new()
        .with_rediscovery_file_name(GO_MOD)
        .with_resolution_path(GO_WORK)
        .with_resolution_path(GO_WORK_SUM);
    for module in modules {
        if let Ok(directory) = repo_root.anchor(&module.directory) {
            let prefix = directory.to_unix().to_string();
            observation = observation
                .with_resolution_path(if prefix.is_empty() {
                    GO_SUM.to_string()
                } else {
                    format!("{prefix}/{GO_SUM}")
                })
                .with_ignore_prefix(if prefix.is_empty() {
                    ".cache/go-build".to_string()
                } else {
                    format!("{prefix}/.cache/go-build")
                });
        }
    }
    observation
}

/// Repository contributor for Go modules.
pub(crate) struct GoContributor {
    repo_root: AbsoluteSystemPathBuf,
}

impl GoContributor {
    pub(crate) fn new(repo_root: AbsoluteSystemPathBuf) -> Arc<Self> {
        Arc::new(Self { repo_root })
    }
}

impl RepositoryContributor for GoContributor {
    fn id(&self) -> ToolchainId {
        ToolchainId::GO
    }

    fn discover_packages(&self) -> DiscoverPackagesFuture<'_> {
        Box::pin(async move {
            let result = turborepo_rayon_compat::block_in_place(|| self.discover());
            result.map_err(|error| crate::toolchain::Error::Failed(Box::new(error)))
        })
    }
}

impl GoContributor {
    fn discover(&self) -> Result<DiscoveredPackages, Error> {
        let (modules, work) = discover_modules(&self.repo_root)?;
        let modules_by_dir: HashMap<_, _> = modules
            .iter()
            .map(|module| (module.directory.clone(), module.name.clone()))
            .collect();
        let module_names: HashSet<_> = modules.iter().map(|module| module.name.clone()).collect();
        let compiler = go_version(&self.repo_root)?;
        let compiler_identity = ExternalPackageIdentity::new("go", compiler.clone());
        let mut packages = Vec::with_capacity(modules.len());
        let mut resolutions = Vec::with_capacity(modules.len());
        let mut dependencies = HashMap::new();
        let mut directories = BTreeMap::new();
        let mut requirements = HashMap::new();
        let mut module_sums = HashMap::new();
        for module in &modules {
            let local = go_json::<GoListPackage>(
                &module.directory,
                &["list", "-mod=readonly", "-json", "./..."],
            )?;
            let mains: Vec<_> = local
                .iter()
                .filter(|package| package.name == "main")
                .map(|package| package.import_path.clone())
                .collect();
            let main = (mains.len() == 1).then(|| mains[0].as_str());
            let listed = go_json::<GoListPackage>(
                &module.directory,
                &["list", "-mod=readonly", "-deps", "-test", "-json", "./..."],
            )?;
            let listed_modules: Vec<_> = listed
                .iter()
                .filter_map(|package| package.module.as_ref())
                .collect();
            for listed_module in &listed_modules {
                let effective = listed_module.replace.as_deref().unwrap_or(listed_module);
                if effective.version.is_empty() && !effective.dir.is_empty() {
                    let canonical =
                        std::fs::canonicalize(&effective.dir).map_err(|source| Error::Read {
                            path: effective.dir.clone(),
                            source,
                        })?;
                    let canonical = AbsoluteSystemPathBuf::try_from(canonical.as_path())?;
                    if self.repo_root.anchor(&canonical).is_err() {
                        return Err(Error::OutsideRepository {
                            path: effective.dir.clone(),
                            root: self.repo_root.to_string(),
                        });
                    }
                    if !modules_by_dir.contains_key(&canonical) {
                        return Err(Error::UnknownLocalReplacement {
                            path: effective.dir.clone(),
                        });
                    }
                }
            }
            let mut identities = BTreeSet::new();
            for listed_module in &listed_modules {
                if let Some(identity) = module_identity(listed_module)? {
                    identities.insert(identity);
                }
            }
            let import_targets: HashMap<_, _> = listed
                .iter()
                .filter_map(|package| {
                    let module = package.module.as_ref()?;
                    let effective = module.replace.as_deref().unwrap_or(module);
                    let canonical = std::fs::canonicalize(&effective.dir).ok()?;
                    let canonical =
                        AbsoluteSystemPathBuf::new(canonical.to_string_lossy().to_string()).ok()?;
                    modules_by_dir
                        .get(&canonical)
                        .cloned()
                        .map(|target| (package.import_path.clone(), target))
                })
                .collect();
            let imported_modules: BTreeSet<_> = local
                .iter()
                .flat_map(|package| &package.imports)
                .filter_map(|import| import_targets.get(import))
                .filter(|target| *target != &module.name)
                .cloned()
                .collect();
            let sum_keys = listed_modules
                .iter()
                .flat_map(|module| module_sum_keys(module))
                .collect();
            identities.insert(compiler_identity.clone());
            resolutions.push(PackageResolution::new(module.name.clone(), identities));

            let mut relationships: Vec<_> = imported_modules
                .iter()
                .cloned()
                .map(|target| Relationship::internal(target, DependencyKind::Production))
                .collect();
            let local_dependencies: Vec<_> = imported_modules.into_iter().collect();
            for (required, version) in &module.manifest.requires {
                let matches = |replacement: &&Replacement| {
                    replacement.old == *required
                        && replacement
                            .old_version
                            .as_ref()
                            .is_none_or(|old_version| old_version == version)
                };
                let workspace_replacement = work.replacements.iter().find(matches);
                let module_replacement = module.manifest.replacements.iter().find(matches);
                let replacement = workspace_replacement.or(module_replacement);
                let target = if let Some(replacement) = workspace_replacement {
                    replacement_target(
                        &self.repo_root,
                        &self.repo_root,
                        replacement,
                        &modules_by_dir,
                    )?
                } else if let Some(replacement) = module_replacement {
                    replacement_target(
                        &self.repo_root,
                        &module.directory,
                        replacement,
                        &modules_by_dir,
                    )?
                } else if module_names.contains(required) {
                    Some(required.clone())
                } else {
                    None
                };
                if target.is_none() {
                    let specifier = replacement
                        .and_then(|replacement| replacement.version.clone())
                        .unwrap_or_else(|| version.clone());
                    relationships.push(Relationship::new(
                        required.clone(),
                        DependencyKind::Production,
                        RelationshipTarget::UnresolvedExternal {
                            name: required.clone(),
                            specifier,
                        },
                    ));
                }
            }
            dependencies.insert(module.name.clone(), local_dependencies);
            requirements.insert(
                module.name.clone(),
                listed_modules
                    .iter()
                    .map(|module| module.path.clone())
                    .collect(),
            );
            let directory = self
                .repo_root
                .anchor(&module.directory)?
                .to_unix()
                .to_string();
            directories.insert(module.name.clone(), directory.clone());
            let sum_path = module.directory.join_component(GO_SUM);
            if sum_path.exists() {
                module_sums.insert(
                    module.name.clone(),
                    ModuleSumKnowledge {
                        path: if directory.is_empty() {
                            GO_SUM.to_string()
                        } else {
                            format!("{directory}/{GO_SUM}")
                        },
                        contents: read(&sum_path)?,
                        keys: sum_keys,
                    },
                );
            }
            packages.push(
                DiscoveredPackage::package(
                    Some(module.name.clone()),
                    PackageJson::default(),
                    module.manifest_path.clone(),
                )
                .with_native_relationships(relationships)
                .with_native_tasks(native_tasks(main))
                .with_task_contract(task_contract(main.is_some())),
            );
        }

        let mut definition_sources = Vec::new();
        if self.repo_root.join_component(GO_WORK).exists() {
            definition_sources.push(AnchoredSystemPathBuf::from_raw(GO_WORK)?);
            if self.repo_root.join_component(GO_WORK_SUM).exists() {
                definition_sources.push(AnchoredSystemPathBuf::from_raw(GO_WORK_SUM)?);
            }
        }
        for module in &modules {
            definition_sources.push(self.repo_root.anchor(&module.manifest_path)?.to_owned());
            let sum = module.directory.join_component(GO_SUM);
            if sum.exists() {
                definition_sources.push(self.repo_root.anchor(&sum)?.to_owned());
            }
        }
        definition_sources.sort();
        definition_sources.dedup();
        let members = modules
            .iter()
            .map(|module| module.name.clone())
            .collect::<Vec<_>>();
        let resolution = ExternalResolutionDomain::new(
            crate::external_resolution::GO_RESOLUTION_DOMAIN.clone(),
            ToolchainId::GO,
            AnchoredSystemPathBuf::default(),
            members,
            definition_sources,
            ExternalResolutionData::Resolved {
                completeness: ResolutionCompleteness::Complete,
                packages: resolutions,
            },
        );
        let work_sum_path = self.repo_root.join_component(GO_WORK_SUM);
        let work_sum = work_sum_path
            .exists()
            .then(|| read(&work_sum_path))
            .transpose()?;
        let prune = GoPruneKnowledge {
            directories,
            dependencies,
            go_version: work.go_version,
            toolchain: work.toolchain,
            replacements: work.replacements,
            requirements,
            module_sums,
            work_sum,
            workspace: self.repo_root.join_component(GO_WORK).exists(),
        };
        Ok(DiscoveredPackages::new(
            packages,
            vec![WorkspaceRoot::new("go", self.repo_root.clone())],
        )
        .with_external_resolution(resolution)
        .with_change_observation(change_observation(&modules, &self.repo_root))
        .with_prune_domain(Arc::new(prune)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_work_blocks_and_comments() {
        let path = AbsoluteSystemPathBuf::new(if cfg!(windows) {
            r"C:\repo\go.work"
        } else {
            "/repo/go.work"
        })
        .unwrap();
        let parsed = parse_work(
            &path,
            r#"go 1.24
toolchain go1.24.1
use (
  ./services/api // API
  "./libs/auth"
)
replace example.com/old => example.com/new v1.2.3
"#,
        )
        .unwrap();
        assert_eq!(parsed.go_version.as_deref(), Some("1.24"));
        assert_eq!(parsed.toolchain.as_deref(), Some("go1.24.1"));
        assert_eq!(parsed.uses, ["./services/api", "./libs/auth"]);
        assert_eq!(parsed.replacements[0].new, "example.com/new");
    }

    #[test]
    fn parses_module_requirements_and_local_replacements() {
        let path = AbsoluteSystemPathBuf::new(if cfg!(windows) {
            r"C:\repo\go.mod"
        } else {
            "/repo/go.mod"
        })
        .unwrap();
        let parsed = parse_mod(
            &path,
            r#"module example.com/api

go 1.24
require (
 example.com/auth v0.0.0
 github.com/google/uuid v1.6.0
)
replace example.com/auth => ../auth
"#,
        )
        .unwrap();
        assert_eq!(parsed.name, "example.com/api");
        assert_eq!(parsed.requires.len(), 2);
        assert_eq!(parsed.replacements[0].new, "../auth");
    }

    #[test]
    fn discovers_modules_edges_and_main_tasks() {
        if which::which("go").is_err() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = AbsoluteSystemPathBuf::new(temp.path().to_string_lossy().to_string()).unwrap();
        std::fs::create_dir_all(temp.path().join("apps/api")).unwrap();
        std::fs::create_dir_all(temp.path().join("packages/auth")).unwrap();
        std::fs::write(
            temp.path().join("go.work"),
            "go 1.20\n\nuse (\n ./apps/api\n ./packages/auth\n)\n",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("apps/api/go.mod"),
            "module example.com/api\n\ngo 1.20\n\nrequire example.com/auth v0.0.0\n\nreplace \
             example.com/auth => ../../packages/auth\n",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("apps/api/main.go"),
            "package main\nimport _ \"example.com/auth\"\nfunc main() {}\n",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("packages/auth/go.mod"),
            "module example.com/auth\n\ngo 1.20\n",
        )
        .unwrap();
        std::fs::write(temp.path().join("packages/auth/auth.go"), "package auth\n").unwrap();

        let discovered = GoContributor::new(root).discover().unwrap();
        let (packages, _, resolutions, observations, prune_domains) = discovered.into_parts();
        assert_eq!(packages.len(), 2);
        assert_eq!(resolutions.len(), 1);
        assert_eq!(observations.len(), 1);
        assert_eq!(prune_domains.len(), 1);
        let api = packages
            .into_iter()
            .map(DiscoveredPackage::into_parts)
            .find(|package| package.name.as_deref() == Some("example.com/api"))
            .unwrap();
        assert!(
            api.native_relationships
                .unwrap()
                .iter()
                .any(|relationship| {
                    relationship.target()
                        == &RelationshipTarget::Internal("example.com/auth".into())
                })
        );
        let task_names: BTreeSet<_> = api
            .native_tasks
            .unwrap()
            .into_iter()
            .map(|task| task.name().to_string())
            .collect();
        assert!(task_names.contains("build"));
        assert!(task_names.contains("run"));
        assert!(task_names.contains("test"));
    }

    #[test]
    fn external_identity_and_pruned_sums_include_checksum_data() {
        let module = GoListModule {
            path: "example.com/dependency".into(),
            version: "v1.2.3".into(),
            sum: "h1:module".into(),
            go_mod_sum: "h1:manifest".into(),
            main: false,
            replace: None,
            dir: String::new(),
        };
        assert_eq!(
            module_identity(&module).unwrap(),
            Some(
                ExternalPackageIdentity::new(
                    "example.com/dependency",
                    "v1.2.3#h1:module#h1:manifest",
                )
                .with_human_name("example.com/dependency"),
            )
        );
        let keys = module_sum_keys(&module);
        assert_eq!(
            prune_sum(
                "example.com/dependency v1.2.3 h1:module\nexample.com/other v2.0.0 \
                 h1:other\nexample.com/dependency v1.2.3/go.mod h1:manifest\n",
                &keys,
            ),
            "example.com/dependency v1.2.3 h1:module\nexample.com/dependency v1.2.3/go.mod \
             h1:manifest\n"
        );
    }

    #[test]
    fn rejects_external_modules_without_complete_checksums() {
        let module = GoListModule {
            path: "example.com/dependency".into(),
            version: "v1.2.3".into(),
            sum: String::new(),
            go_mod_sum: "h1:manifest".into(),
            main: false,
            replace: None,
            dir: String::new(),
        };
        assert!(matches!(
            module_identity(&module),
            Err(Error::MissingChecksum { ref path, ref version })
                if path == "example.com/dependency" && version == "v1.2.3"
        ));
    }

    #[test]
    fn change_observation_tracks_workspace_manifests_sums_and_cache_prefixes() {
        let temp = tempfile::tempdir().unwrap();
        let root = AbsoluteSystemPathBuf::new(temp.path().to_string_lossy().to_string()).unwrap();
        std::fs::create_dir_all(temp.path().join("module")).unwrap();
        std::fs::write(temp.path().join("go.work"), "go 1.20\nuse ./module\n").unwrap();
        std::fs::write(
            temp.path().join("module/go.mod"),
            "module example.com/module\n\ngo 1.20\n",
        )
        .unwrap();
        let (modules, _) = discover_modules(&root).unwrap();
        assert_eq!(
            change_observation(&modules, &root),
            ChangeObservation::new()
                .with_rediscovery_file_name(GO_MOD)
                .with_resolution_path(GO_WORK)
                .with_resolution_path(GO_WORK_SUM)
                .with_resolution_path("module/go.sum")
                .with_ignore_prefix("module/.cache/go-build")
        );
    }

    #[test]
    fn rejects_duplicate_module_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = AbsoluteSystemPathBuf::new(temp.path().to_string_lossy().to_string()).unwrap();
        for directory in ["one", "two"] {
            std::fs::create_dir_all(temp.path().join(directory)).unwrap();
            std::fs::write(
                temp.path().join(directory).join("go.mod"),
                "module example.com/duplicate\n\ngo 1.20\n",
            )
            .unwrap();
        }
        std::fs::write(
            temp.path().join("go.work"),
            "go 1.20\nuse (\n ./one\n ./two\n)\n",
        )
        .unwrap();
        assert!(matches!(
            discover_modules(&root),
            Err(Error::DuplicateModule { name }) if name == "example.com/duplicate"
        ));
    }

    #[test]
    fn rejects_workspace_members_outside_repository() {
        let parent = tempfile::tempdir().unwrap();
        let repository = parent.path().join("repository");
        let outside = parent.path().join("outside");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            outside.join("go.mod"),
            "module example.com/outside\n\ngo 1.20\n",
        )
        .unwrap();
        std::fs::write(repository.join("go.work"), "go 1.20\nuse ../outside\n").unwrap();
        let root = AbsoluteSystemPathBuf::new(repository.to_string_lossy().to_string()).unwrap();
        assert!(matches!(
            discover_modules(&root),
            Err(Error::OutsideRepository { .. })
        ));
    }

    #[test]
    fn prune_closes_over_local_modules() {
        let knowledge = GoPruneKnowledge {
            directories: BTreeMap::from([
                ("example.com/api".into(), "services/api".into()),
                ("example.com/auth".into(), "libs/auth".into()),
            ]),
            dependencies: HashMap::from([(
                "example.com/api".into(),
                vec!["example.com/auth".into()],
            )]),
            go_version: Some("1.24".into()),
            toolchain: Some("go1.24.1".into()),
            replacements: Vec::new(),
            requirements: HashMap::new(),
            module_sums: HashMap::new(),
            work_sum: None,
            workspace: true,
        };
        let plan = knowledge
            .plan(&["example.com/api".into()])
            .unwrap()
            .unwrap();
        assert_eq!(plan.extra_packages, ["example.com/auth"]);
        assert!(plan.root_files[0].1.contains("./libs/auth"));
        assert!(plan.root_files[0].1.contains("./services/api"));
    }
}
