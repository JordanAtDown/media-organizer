use crate::error::ConfigError;
use crate::http;
use crate::naming;
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};

/// A Unix permission mode, written in the config as an **octal string**.
///
/// TOML has no octal literal, so `file_mode = 777` would be read as decimal 777
/// and mean something entirely different. Accepts `"777"`, `"0777"`, `"0o777"`;
/// rejects anything above `0o7777`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode(pub u32);

impl<'de> Deserialize<'de> for Mode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        let digits = raw.strip_prefix("0o").unwrap_or(&raw);
        let value = u32::from_str_radix(digits, 8).map_err(|_| {
            de::Error::custom(format!(
                "invalid octal mode {raw:?}, expected e.g. \"0777\""
            ))
        })?;
        if value > 0o7777 {
            return Err(de::Error::custom(format!("mode {raw:?} exceeds 0o7777")));
        }
        Ok(Mode(value))
    }
}

impl Serialize for Mode {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{:04o}", self.0))
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnConflict {
    /// Two *different* files want the same name: append `_1`, `_2`, …
    #[default]
    Rename,
    /// Keep the file already there, leave the newcomer in place.
    Skip,
    /// Replace the file already there.
    Overwrite,
}

fn default_pattern() -> String {
    "{year}/{month}/{year}-{month}-{day}_{hour}{min}{sec}_{stem}{ext}".to_string()
}

fn default_extensions() -> Vec<String> {
    ["jpg", "jpeg", "png", "heic", "mp4", "mov", "avi", "mkv"]
        .map(String::from)
        .to_vec()
}

/// The data still lives on a DSM volume exported over NFS, which keeps
/// generating `@eaDir` (one thumbnail per photo), `#recycle`, `#snapshot`…
/// Sync clients and other OSes add their own clutter.
fn default_excluded_dirs() -> Vec<String> {
    [
        "@eaDir",
        "@SynoEAStream",
        "@Recycle",
        "#recycle",
        "#snapshot",
        "@tmp",
        ".stfolder",
        ".stversions",
        ".Trash",
        "$RECYCLE.BIN",
        "System Volume Information",
    ]
    .map(String::from)
    .to_vec()
}

fn default_true() -> bool {
    true
}

fn default_poll_interval() -> u64 {
    30
}

fn default_workers() -> usize {
    8
}

fn default_stability_secs() -> u64 {
    10
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_inputs_root() -> PathBuf {
    PathBuf::from("/inputs")
}

fn default_libraries_root() -> PathBuf {
    PathBuf::from("/libraries")
}

fn default_max_batch() -> usize {
    1000
}

fn default_health_bind() -> String {
    "0.0.0.0:8080".to_string()
}

fn default_startup_grace() -> u64 {
    120
}

fn default_stale_intervals() -> u64 {
    3
}

fn default_notify_quiet_secs() -> u64 {
    60
}

fn default_notify_max_wait_secs() -> u64 {
    600
}

/// Global default modes; a folder may override either one.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    /// Mode applied to each organized file. Omit to leave it untouched.
    pub file_mode: Option<Mode>,
    /// Mode applied to every directory from the library root down to the file's parent.
    pub dir_mode: Option<Mode>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StagingConfig {
    /// Safety valve bounding one commit burst on a huge first run (0 = unlimited).
    #[serde(default = "default_max_batch")]
    pub max_batch: usize,
}

impl Default for StagingConfig {
    fn default() -> Self {
        Self {
            max_batch: default_max_batch(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DedupConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Also compare against files already present at the destination.
    #[serde(default = "default_true")]
    pub check_existing: bool,
    /// Remove a source proven byte-identical to a file already in the library.
    /// Off by default: duplicates are otherwise never deleted.
    #[serde(default)]
    pub delete_source_duplicates: bool,
}

impl Default for DedupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_existing: true,
            delete_source_duplicates: false,
        }
    }
}

/// Memory of files that could not be dated (only reachable with
/// `use_mtime_fallback = false`, or when `stat` itself fails).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Days before a cached entry is retried; 0 = only when the mtime changes.
    #[serde(default)]
    pub ttl_days: u32,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl_days: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HealthConfig {
    /// `host:port` of the `/healthz` endpoint; empty disables it.
    #[serde(default = "default_health_bind")]
    pub bind: String,
    /// Seconds after startup during which "no scan yet" is `starting`, not `stale`.
    #[serde(default = "default_startup_grace")]
    pub startup_grace_secs: u64,
    /// `stale` once nothing progressed for `poll_interval_secs * stale_intervals`
    /// (floored at 60 s).
    #[serde(default = "default_stale_intervals")]
    pub stale_intervals: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            bind: default_health_bind(),
            startup_grace_secs: default_startup_grace(),
            stale_intervals: default_stale_intervals(),
        }
    }
}

/// Where a finished batch is announced. Both sinks are optional and independent;
/// with neither, the feature is off and nothing is written. An empty string counts
/// as absent, like `health.bind`: the file is templated.
///
/// Files that arrive by upload trickle in and are organized as they come, so one
/// scan's batch is rarely the whole upload. The announcement therefore waits for
/// the input to go quiet (`quiet_secs`), but never longer than `max_wait_secs`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NotifyConfig {
    /// Absolute directory receiving `<folder>.json`, overwritten at each event.
    pub dir: Option<PathBuf>,
    /// `http://` URL receiving the same JSON in a `POST`.
    pub webhook_url: Option<String>,
    /// Announce once nothing was organized for this long; 0 = after every batch.
    #[serde(default = "default_notify_quiet_secs")]
    pub quiet_secs: u64,
    /// Never hold an announcement longer than this, however busy the input is.
    #[serde(default = "default_notify_max_wait_secs")]
    pub max_wait_secs: u64,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            dir: None,
            webhook_url: None,
            quiet_secs: default_notify_quiet_secs(),
            max_wait_secs: default_notify_max_wait_secs(),
        }
    }
}

impl NotifyConfig {
    pub fn enabled(&self) -> bool {
        self.dir.is_some() || self.webhook_url.is_some()
    }

    fn normalize(&mut self) {
        self.dir = self.dir.take().filter(|d| !d.as_os_str().is_empty());
        self.webhook_url = self.webhook_url.take().filter(|u| !u.trim().is_empty());
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FolderConfig {
    /// Source folder: an absolute path, or a bare name resolved under `inputs_root`.
    pub input: PathBuf,
    /// Destination library: an absolute path, or a bare name resolved under
    /// `libraries_root`.
    pub library: PathBuf,
    /// Label in logs, `/healthz` and the work area's directory name; must be
    /// unique. Defaults to the last component of `input`.
    pub name: Option<String>,
    #[serde(default = "default_pattern")]
    pub pattern: String,
    #[serde(default = "default_true")]
    pub recursive: bool,
    #[serde(default)]
    pub photo_prefix: String,
    #[serde(default)]
    pub video_prefix: String,
    #[serde(default)]
    pub on_conflict: OnConflict,
    /// Allowed extensions, lowercase, without the dot.
    #[serde(default = "default_extensions")]
    pub extensions: Vec<String>,
    /// Directory names pruned from the walk (matched against each component).
    #[serde(default = "default_excluded_dirs")]
    pub excluded_dirs: Vec<String>,
    /// Last resort of the date chain: the file's mtime. `false` restores the
    /// v1.2.4 contract — a file with no other date is never moved.
    #[serde(default = "default_true")]
    pub use_mtime_fallback: bool,
    /// Inject `DateTimeOriginal` when the date came from the file **name**.
    #[serde(default = "default_true")]
    pub write_exif_from_filename: bool,
    /// Same when the date came from the **mtime** — an approximation, so off by default.
    #[serde(default)]
    pub write_exif_from_mtime: bool,
    /// Render `{ext}` in lowercase (`.JPG` → `.jpg`). Off by default: it changes the
    /// destination name of every newly organized file.
    #[serde(default)]
    pub lowercase_ext: bool,
    pub file_mode: Option<Mode>,
    pub dir_mode: Option<Mode>,
}

impl FolderConfig {
    /// Label for logs and `/healthz`.
    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| {
            self.input.file_name().map_or_else(
                || self.input.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            )
        })
    }

    /// The label as a safe file name: the work area's directory, and the file
    /// name of the folder's batch event.
    pub fn slug(&self) -> String {
        sanitize_label(&self.label())
    }

    /// This folder's private work area (staging and journal) under `work_root`.
    pub fn work_subdir(&self, work_root: &Path) -> PathBuf {
        work_root.join(self.slug())
    }

    pub fn file_mode(&self, global: &Permissions) -> Option<Mode> {
        self.file_mode.or(global.file_mode)
    }

    pub fn dir_mode(&self, global: &Permissions) -> Option<Mode> {
        self.dir_mode.or(global.dir_mode)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub folders: Vec<FolderConfig>,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Worker threads for the parallel phases; 1 = sequential and deterministic.
    #[serde(default = "default_workers")]
    pub workers: usize,
    /// Ignore files modified less than this many seconds ago (still uploading).
    #[serde(default = "default_stability_secs")]
    pub stability_secs: u64,
    /// Where bare input names are looked up. Mount each source folder under it.
    #[serde(default = "default_inputs_root")]
    pub inputs_root: PathBuf,
    /// Where bare library names are looked up. Mount each library under it.
    #[serde(default = "default_libraries_root")]
    pub libraries_root: PathBuf,
    /// The service's private work area (staging, journal). Defaults to
    /// `<state_dir>/work`; it never needs to be mounted except to debug.
    pub work_dir: Option<PathBuf>,
    /// Overridden by `--state-dir` / `MO_STATE_DIR`.
    pub state_dir: Option<PathBuf>,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub staging: StagingConfig,
    #[serde(default)]
    pub dedup: DedupConfig,
    #[serde(default)]
    pub no_date_cache: CacheConfig,
    #[serde(default)]
    pub health: HealthConfig,
    #[serde(default)]
    pub notify: NotifyConfig,
}

pub fn load(path: &Path) -> Result<Config, ConfigError> {
    parse(&std::fs::read_to_string(path)?)
}

pub fn parse(content: &str) -> Result<Config, ConfigError> {
    let mut config: Config = toml::from_str(content)?;
    config.notify.normalize();
    resolve_names(&mut config)?;
    validate(&config)?;
    Ok(config)
}

impl Config {
    /// The work area root for a given state directory.
    pub fn work_root(&self, state_dir: &Path) -> PathBuf {
        self.work_dir
            .clone()
            .unwrap_or_else(|| state_dir.join("work"))
    }
}

/// Turn bare names into paths under the configured roots, so a compose file can
/// mount `/inputs/jordan` and `/libraries/jordan` and the config just says `jordan`.
fn resolve_names(config: &mut Config) -> Result<(), ConfigError> {
    for (what, root) in [
        ("inputs_root", &config.inputs_root),
        ("libraries_root", &config.libraries_root),
    ] {
        if !root.is_absolute() {
            return invalid(format!("{what} must be an absolute path"));
        }
    }
    let (inputs, libraries) = (config.inputs_root.clone(), config.libraries_root.clone());
    for folder in &mut config.folders {
        folder.input = resolve_dir("input", &folder.input, &inputs)?;
        folder.library = resolve_dir("library", &folder.library, &libraries)?;
    }
    Ok(())
}

fn resolve_dir(what: &str, value: &Path, root: &Path) -> Result<PathBuf, ConfigError> {
    if value.is_absolute() {
        return Ok(value.to_path_buf());
    }
    let mut parts = value.components();
    match (parts.next(), parts.next()) {
        (Some(Component::Normal(_)), None) => Ok(root.join(value)),
        _ => invalid(format!(
            "{what} '{}' must be an absolute path or a bare name without '/'",
            value.display()
        )),
    }
}

/// Make a label safe as a directory name.
fn sanitize_label(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    match cleaned.as_str() {
        "" | "." | ".." => "folder".to_string(),
        _ => cleaned,
    }
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError::Invalid(msg.into()))
}

pub fn validate(config: &Config) -> Result<(), ConfigError> {
    if config.folders.is_empty() {
        return invalid("at least one [[folders]] entry is required");
    }
    if config.workers == 0 {
        return invalid("workers must be >= 1");
    }
    if !matches!(
        config.log_level.as_str(),
        "error" | "warn" | "info" | "debug" | "trace"
    ) {
        return invalid(format!(
            "log_level must be one of: error, warn, info, debug, trace — got '{}'",
            config.log_level
        ));
    }
    if !config.health.bind.is_empty() && config.health.bind.parse::<SocketAddr>().is_err() {
        return invalid(format!(
            "health.bind must be a host:port socket address, got '{}'",
            config.health.bind
        ));
    }

    if config.dedup.delete_source_duplicates
        && !(config.dedup.enabled && config.dedup.check_existing)
    {
        return invalid(
            "dedup.delete_source_duplicates needs dedup.enabled and dedup.check_existing: \
             a source is only removed against a file already in the library",
        );
    }

    let mut seen = std::collections::HashSet::new();
    for folder in &config.folders {
        validate_folder(folder)?;
        // Each folder gets its own work area named after its label.
        if !seen.insert(folder.slug()) {
            return invalid(format!(
                "two folders share the label '{}': give each a distinct `name`",
                folder.label()
            ));
        }
    }
    validate_notify(config)
}

fn validate_notify(config: &Config) -> Result<(), ConfigError> {
    let notify = &config.notify;
    if notify.max_wait_secs < notify.quiet_secs {
        return invalid(format!(
            "notify.max_wait_secs ({}) must be >= notify.quiet_secs ({}): an announcement \
             cannot be held for less time than the quiet period it waits for",
            notify.max_wait_secs, notify.quiet_secs
        ));
    }
    if let Some(dir) = &notify.dir {
        if !dir.is_absolute() {
            return invalid(format!(
                "notify.dir must be an absolute path, got '{}'",
                dir.display()
            ));
        }
        // An event inside a tree that is scanned would be walked like any file, and
        // one inside a library would be seen by Immich and Nextcloud.
        for folder in &config.folders {
            for (what, root) in [("input", &folder.input), ("library", &folder.library)] {
                if dir.starts_with(root) {
                    return invalid(format!(
                        "notify.dir {} is inside the {what} of '{}' ({})",
                        dir.display(),
                        folder.label(),
                        root.display()
                    ));
                }
            }
        }
    }
    if let Some(url) = &notify.webhook_url {
        http::Url::parse(url)
            .map_err(|e| ConfigError::Invalid(format!("notify.webhook_url: {e}")))?;
    }
    Ok(())
}

fn validate_folder(folder: &FolderConfig) -> Result<(), ConfigError> {
    let label = folder.label();

    if folder.input == folder.library {
        return invalid(format!("{label}: input and library must differ"));
    }
    // A recursive scan of an input that contains the library would feed on its own output.
    if folder.recursive && folder.library.starts_with(&folder.input) {
        return invalid(format!(
            "{label}: library {} is inside the recursive input {}",
            folder.library.display(),
            folder.input.display()
        ));
    }
    naming::validate_pattern(&folder.pattern)
        .map_err(|e| ConfigError::Invalid(format!("{label}: pattern: {e}")))?;
    if folder.extensions.is_empty() {
        return invalid(format!("{label}: extensions must not be empty"));
    }
    if let Some(bad) = folder
        .extensions
        .iter()
        .find(|e| e.starts_with('.') || **e != e.to_lowercase())
    {
        return invalid(format!(
            "{label}: extension '{bad}' must be lowercase and without a leading dot"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const MINIMAL: &str = r#"
[[folders]]
input = "/in"
library = "/out"
"#;

    fn with_folder(extra: &str) -> String {
        format!("{extra}\n{MINIMAL}")
    }

    #[test]
    fn minimal_config_gets_defaults() {
        let cfg = parse(MINIMAL).unwrap();
        let f = &cfg.folders[0];
        assert_eq!(cfg.poll_interval_secs, 30);
        assert_eq!(cfg.workers, 8);
        assert_eq!(cfg.stability_secs, 10);
        assert_eq!(cfg.log_level, "info");
        assert!(f.recursive && f.use_mtime_fallback && f.write_exif_from_filename);
        assert!(!f.write_exif_from_mtime);
        assert!(!f.lowercase_ext);
        assert_eq!(f.on_conflict, OnConflict::Rename);
        assert!(cfg.dedup.enabled && cfg.dedup.check_existing);
        assert!(
            !cfg.dedup.delete_source_duplicates,
            "never deletes unless asked"
        );
        assert_eq!(cfg.staging.max_batch, 1000);
        assert_eq!(cfg.health.bind, "0.0.0.0:8080");
    }

    #[test]
    fn synology_dirs_stay_excluded() {
        let cfg = parse(MINIMAL).unwrap();
        for d in [
            "@eaDir",
            "@SynoEAStream",
            "@Recycle",
            "#recycle",
            "#snapshot",
            "@tmp",
        ] {
            assert!(cfg.folders[0].excluded_dirs.iter().any(|e| e == d), "{d}");
        }
    }

    #[rstest]
    #[case("777", 0o777)]
    #[case("0777", 0o777)]
    #[case("0o666", 0o666)]
    #[case("0644", 0o644)]
    fn modes_are_octal_strings(#[case] raw: &str, #[case] expected: u32) {
        let cfg = parse(&format!("{MINIMAL}file_mode = \"{raw}\"\n")).unwrap();
        assert_eq!(cfg.folders[0].file_mode, Some(Mode(expected)));
    }

    #[rstest]
    #[case("file_mode = 777")] // an integer would be read as decimal
    #[case("file_mode = \"abc\"")]
    #[case("file_mode = \"0888\"")]
    #[case("file_mode = \"17777\"")]
    fn bad_modes_are_rejected(#[case] line: &str) {
        assert!(parse(&format!("{MINIMAL}{line}\n")).is_err());
    }

    #[test]
    fn folder_mode_overrides_global() {
        let cfg = parse(&format!(
            "[permissions]\nfile_mode = \"0644\"\ndir_mode = \"0755\"\n{MINIMAL}file_mode = \"0666\"\n"
        ))
        .unwrap();
        let f = &cfg.folders[0];
        assert_eq!(f.file_mode(&cfg.permissions), Some(Mode(0o666)));
        assert_eq!(f.dir_mode(&cfg.permissions), Some(Mode(0o755)));
    }

    #[test]
    fn omitted_modes_mean_no_chmod() {
        let cfg = parse(MINIMAL).unwrap();
        assert_eq!(cfg.folders[0].file_mode(&cfg.permissions), None);
        assert_eq!(cfg.folders[0].dir_mode(&cfg.permissions), None);
    }

    #[rstest]
    #[case("")] // no [[folders]]
    #[case("this is not toml ::::")]
    #[case("workers = 0\n[[folders]]\ninput = \"/in\"\nlibrary = \"/out\"")]
    #[case("log_level = \"verbose\"\n[[folders]]\ninput = \"/in\"\nlibrary = \"/out\"")]
    #[case("[health]\nbind = \"nonsense\"\n[[folders]]\ninput = \"/in\"\nlibrary = \"/out\"")]
    #[case("[[folders]]\ninput = \"/same\"\nlibrary = \"/same\"")]
    #[case("[[folders]]\ninput = \"/in\"\nlibrary = \"/in/sorted\"")]
    #[case("[[folders]]\ninput = \"/in\"\nlibrary = \"/out\"\npattern = \"{stem}_{counter}{ext}\"")]
    #[case("[[folders]]\ninput = \"/in\"\nlibrary = \"/out\"\npattern = \"{yeer}/{stem}\"")]
    #[case("[[folders]]\ninput = \"/in\"\nlibrary = \"/out\"\nextensions = []")]
    #[case("[[folders]]\ninput = \"/in\"\nlibrary = \"/out\"\nextensions = [\".jpg\"]")]
    #[case("[[folders]]\ninput = \"/in\"\nlibrary = \"/out\"\nextensions = [\"JPG\"]")]
    fn invalid_configs_are_rejected(#[case] toml: &str) {
        assert!(parse(toml).is_err(), "accepted: {toml}");
    }

    #[test]
    fn output_inside_input_is_fine_when_not_recursive() {
        let toml = "[[folders]]\ninput = \"/in\"\nlibrary = \"/in/sorted\"\nrecursive = false";
        assert!(parse(toml).is_ok());
    }

    #[test]
    fn unknown_keys_are_rejected_everywhere() {
        assert!(parse(&with_folder("wokers = 4")).is_err());
        assert!(parse(&format!("{MINIMAL}oops = 1")).is_err());
        assert!(parse(&with_folder("[dedup]\nenable = true")).is_err());
    }

    #[test]
    fn delete_source_duplicates_is_opt_in_and_needs_a_comparison_with_the_library() {
        let on = |dedup: &str| parse(&with_folder(&format!("[dedup]\n{dedup}")));
        assert!(on("delete_source_duplicates = true").is_ok());
        assert!(on("delete_source_duplicates = false\nenabled = false").is_ok());
        assert!(on("delete_source_duplicates = true\nenabled = false").is_err());
        assert!(on("delete_source_duplicates = true\ncheck_existing = false").is_err());
    }

    #[test]
    fn missing_config_file_is_an_io_error() {
        assert!(matches!(
            load(Path::new("/nonexistent/config.toml")),
            Err(ConfigError::Io(_))
        ));
    }

    // -- inputs, libraries and the work area ---------------------------------

    fn folders(toml: &str) -> Vec<FolderConfig> {
        parse(toml).unwrap().folders
    }

    #[test]
    fn bare_names_resolve_under_the_roots() {
        let f = &folders("[[folders]]\ninput = \"jordan\"\nlibrary = \"photos\"")[0];
        assert_eq!(f.input, PathBuf::from("/inputs/jordan"));
        assert_eq!(f.library, PathBuf::from("/libraries/photos"));
    }

    #[test]
    fn the_roots_are_configurable() {
        let f = &folders(
            "inputs_root = \"/in\"\nlibraries_root = \"/lib\"\n[[folders]]\ninput = \"a\"\nlibrary = \"b\"",
        )[0];
        assert_eq!(f.input, PathBuf::from("/in/a"));
        assert_eq!(f.library, PathBuf::from("/lib/b"));
    }

    #[test]
    fn absolute_paths_are_kept_as_they_are() {
        let f = &folders("[[folders]]\ninput = \"/mnt/x/in\"\nlibrary = \"/mnt/x/lib\"")[0];
        assert_eq!(f.input, PathBuf::from("/mnt/x/in"));
        assert_eq!(f.library, PathBuf::from("/mnt/x/lib"));
    }

    #[rstest]
    #[case("sub/dir")]
    #[case("../escape")]
    #[case("./here")]
    #[case("")]
    fn ambiguous_relative_paths_are_rejected(#[case] value: &str) {
        let toml = format!("[[folders]]\ninput = \"{value}\"\nlibrary = \"/lib\"");
        let err = parse(&toml).unwrap_err().to_string();
        assert!(err.contains("bare name"), "{value:?}: {err}");
    }

    #[test]
    fn relative_roots_are_rejected() {
        assert!(
            parse("inputs_root = \"inputs\"\n[[folders]]\ninput = \"a\"\nlibrary = \"b\"").is_err()
        );
    }

    #[test]
    fn the_label_defaults_to_the_input_name() {
        let f = &folders("[[folders]]\ninput = \"jordan\"\nlibrary = \"photos\"")[0];
        assert_eq!(f.label(), "jordan");
        let f =
            &folders("[[folders]]\nname = \"phone\"\ninput = \"jordan\"\nlibrary = \"photos\"")[0];
        assert_eq!(f.label(), "phone");
    }

    #[test]
    fn two_folders_with_the_same_label_are_rejected() {
        // Two Nextcloud users both sync into ".../synchronisation".
        let toml = "[[folders]]\ninput = \"/a/synchronisation\"\nlibrary = \"/x\"\n\
                    [[folders]]\ninput = \"/b/synchronisation\"\nlibrary = \"/y\"";
        let err = parse(toml).unwrap_err().to_string();
        assert!(err.contains("distinct `name`"), "{err}");

        let fixed = toml.replacen("[[folders]]", "[[folders]]\nname = \"one\"", 1);
        assert!(parse(&fixed).is_ok());
    }

    #[test]
    fn the_work_area_is_private_per_folder_and_defaults_under_the_state_dir() {
        let cfg = parse(
            "[[folders]]\ninput = \"jordan\"\nlibrary = \"a\"\n[[folders]]\ninput = \"sylvie\"\nlibrary = \"b\"",
        )
        .unwrap();
        let root = cfg.work_root(Path::new("/var/lib/media-organizer"));
        assert_eq!(root, PathBuf::from("/var/lib/media-organizer/work"));
        assert_eq!(cfg.folders[0].work_subdir(&root), root.join("jordan"));
        assert_ne!(
            cfg.folders[0].work_subdir(&root),
            cfg.folders[1].work_subdir(&root)
        );
    }

    #[test]
    fn work_dir_can_be_set_explicitly() {
        let cfg =
            parse("work_dir = \"/work\"\n[[folders]]\ninput = \"a\"\nlibrary = \"b\"").unwrap();
        assert_eq!(cfg.work_root(Path::new("/state")), PathBuf::from("/work"));
    }

    #[test]
    fn labels_become_safe_directory_names() {
        assert_eq!(sanitize_label("Sony Alpha 7"), "Sony_Alpha_7");
        assert_eq!(sanitize_label("a/b"), "a_b");
        assert_eq!(sanitize_label(".."), "folder");
        assert_eq!(sanitize_label(""), "folder");
    }

    #[test]
    fn notify_is_off_by_default() {
        let cfg = parse(MINIMAL).unwrap();
        assert_eq!(cfg.notify, NotifyConfig::default());
        assert!(!cfg.notify.enabled());
    }

    #[test]
    fn notify_sinks_parse_independently() {
        let dir = parse(&with_folder("[notify]\ndir = \"/state/events\"")).unwrap();
        assert_eq!(dir.notify.dir, Some(PathBuf::from("/state/events")));
        assert_eq!(dir.notify.webhook_url, None);
        assert!(dir.notify.enabled());

        let hook = parse(&with_folder(
            "[notify]\nwebhook_url = \"http://host.docker.internal:8123/batch\"",
        ))
        .unwrap();
        assert_eq!(hook.notify.dir, None);
        assert!(hook.notify.enabled());
    }

    #[test]
    fn notify_debounce_defaults_to_a_minute_of_quiet_capped_at_ten() {
        let cfg = parse(&with_folder("[notify]\ndir = \"/state/events\"")).unwrap();
        assert_eq!((cfg.notify.quiet_secs, cfg.notify.max_wait_secs), (60, 600));
    }

    #[test]
    fn notify_debounce_is_configurable_and_zero_means_immediate() {
        let cfg = parse(&with_folder(
            "[notify]\ndir = \"/e\"\nquiet_secs = 0\nmax_wait_secs = 0",
        ))
        .unwrap();
        assert_eq!((cfg.notify.quiet_secs, cfg.notify.max_wait_secs), (0, 0));
        let cfg = parse(&with_folder(
            "[notify]\ndir = \"/e\"\nquiet_secs = 120\nmax_wait_secs = 1800",
        ))
        .unwrap();
        assert_eq!(
            (cfg.notify.quiet_secs, cfg.notify.max_wait_secs),
            (120, 1800)
        );
    }

    #[test]
    fn a_cap_below_the_quiet_period_is_rejected() {
        let err = parse(&with_folder(
            "[notify]\ndir = \"/e\"\nquiet_secs = 900\nmax_wait_secs = 600",
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("max_wait_secs"), "{err}");
        // Even when only the quiet period is raised past the default cap.
        assert!(parse(&with_folder("[notify]\ndir = \"/e\"\nquiet_secs = 900")).is_err());
    }

    #[test]
    fn empty_notify_values_mean_disabled() {
        let cfg = parse(&with_folder("[notify]\ndir = \"\"\nwebhook_url = \"\"")).unwrap();
        assert_eq!(cfg.notify, NotifyConfig::default());
    }

    #[rstest]
    #[case("dir = \"events\"")]
    #[case("dir = \"./events\"")]
    #[case("dir = \"/in/events\"")] // inside the input
    #[case("dir = \"/out\"")] // the library itself
    #[case("dir = \"/out/2026/events\"")] // inside the library
    #[case("webhook_url = \"https://host/batch\"")]
    #[case("webhook_url = \"host:8123/batch\"")]
    #[case("webhook_url = \"http://user:pw@host/batch\"")]
    #[case("webhook_url = \"http://host:99999/batch\"")]
    #[case("directory = \"/state/events\"")] // unknown key
    fn bad_notify_settings_are_rejected(#[case] line: &str) {
        assert!(
            parse(&with_folder(&format!("[notify]\n{line}"))).is_err(),
            "{line}"
        );
    }

    #[test]
    fn a_bad_webhook_says_why() {
        let err = parse(&with_folder("[notify]\nwebhook_url = \"https://h/x\""))
            .unwrap_err()
            .to_string();
        assert!(err.contains("notify.webhook_url"), "{err}");
        assert!(err.contains("https is not supported"), "{err}");
    }

    #[test]
    fn the_event_file_name_is_the_sanitized_label() {
        let cfg =
            parse("[[folders]]\nname = \"Sony Alpha 7\"\ninput = \"a\"\nlibrary = \"b\"").unwrap();
        assert_eq!(cfg.folders[0].slug(), "Sony_Alpha_7");
    }

    #[test]
    fn the_old_staging_dir_key_is_gone() {
        assert!(
            parse("[staging]\ndir = \"/x\"\n[[folders]]\ninput = \"a\"\nlibrary = \"b\"").is_err()
        );
    }

    #[test]
    fn the_old_output_key_is_rejected() {
        let err = parse("[[folders]]\ninput = \"/in\"\noutput = \"/out\"").unwrap_err();
        assert!(err.to_string().contains("config") || format!("{err:?}").contains("output"));
    }
}
