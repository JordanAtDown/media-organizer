use crate::error::ConfigError;
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};
use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

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

/// Global default modes; a folder may override either one.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    /// Mode applied to each organized file. Omit to leave it untouched.
    pub file_mode: Option<Mode>,
    /// Mode applied to every directory from `output` down to the file's parent.
    pub dir_mode: Option<Mode>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StagingConfig {
    /// Defaults to `<output>/.media-organizer/staging`. Must be on the same
    /// filesystem as `output`, so the final commit is a metadata-only rename.
    pub dir: Option<PathBuf>,
    /// Safety valve bounding one commit burst on a huge first run (0 = unlimited).
    #[serde(default = "default_max_batch")]
    pub max_batch: usize,
}

impl Default for StagingConfig {
    fn default() -> Self {
        Self {
            dir: None,
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
}

impl Default for DedupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_existing: true,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FolderConfig {
    pub input: PathBuf,
    pub output: PathBuf,
    /// Label in logs and `/healthz`; defaults to the input path.
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
    pub file_mode: Option<Mode>,
    pub dir_mode: Option<Mode>,
}

impl FolderConfig {
    /// Label for logs and `/healthz`.
    pub fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| self.input.display().to_string())
    }

    /// Effective staging directory for this folder.
    pub fn staging_dir(&self, staging: &StagingConfig) -> PathBuf {
        staging
            .dir
            .clone()
            .unwrap_or_else(|| self.output.join(".media-organizer").join("staging"))
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
}

pub fn load(path: &Path) -> Result<Config, ConfigError> {
    parse(&std::fs::read_to_string(path)?)
}

pub fn parse(content: &str) -> Result<Config, ConfigError> {
    let config: Config = toml::from_str(content)?;
    validate(&config)?;
    Ok(config)
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError::Invalid(msg.into()))
}

fn validate(config: &Config) -> Result<(), ConfigError> {
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

    for folder in &config.folders {
        validate_folder(folder, &config.staging)?;
    }
    Ok(())
}

fn validate_folder(folder: &FolderConfig, staging: &StagingConfig) -> Result<(), ConfigError> {
    let label = folder.label();

    if folder.input == folder.output {
        return invalid(format!("{label}: input and output must differ"));
    }
    // A recursive scan of an input that contains the output would feed on its own output.
    if folder.recursive && folder.output.starts_with(&folder.input) {
        return invalid(format!(
            "{label}: output {} is inside the recursive input {}",
            folder.output.display(),
            folder.input.display()
        ));
    }
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

    check_same_filesystem(&label, &folder.staging_dir(staging), &folder.output)
}

/// The staging area must share `output`'s filesystem, or the commit stops being
/// a metadata-only rename. Compared only when both can be resolved to an
/// existing ancestor — a not-yet-created tree is checked again at startup.
fn check_same_filesystem(label: &str, staging: &Path, output: &Path) -> Result<(), ConfigError> {
    let (Some(a), Some(b)) = (device_of(staging), device_of(output)) else {
        return Ok(());
    };
    if a != b {
        return invalid(format!(
            "{label}: staging dir {} is not on the same filesystem as output {}",
            staging.display(),
            output.display()
        ));
    }
    Ok(())
}

/// `st_dev` of the nearest existing ancestor of `path`.
fn device_of(path: &Path) -> Option<u64> {
    path.ancestors()
        .find_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.dev())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const MINIMAL: &str = r#"
[[folders]]
input = "/in"
output = "/out"
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
        assert_eq!(f.on_conflict, OnConflict::Rename);
        assert!(cfg.dedup.enabled && cfg.dedup.check_existing);
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

    #[test]
    fn staging_defaults_under_output() {
        let cfg = parse(MINIMAL).unwrap();
        assert_eq!(
            cfg.folders[0].staging_dir(&cfg.staging),
            PathBuf::from("/out/.media-organizer/staging")
        );
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
    #[case("workers = 0\n[[folders]]\ninput = \"/in\"\noutput = \"/out\"")]
    #[case("log_level = \"verbose\"\n[[folders]]\ninput = \"/in\"\noutput = \"/out\"")]
    #[case("[health]\nbind = \"nonsense\"\n[[folders]]\ninput = \"/in\"\noutput = \"/out\"")]
    #[case("[[folders]]\ninput = \"/same\"\noutput = \"/same\"")]
    #[case("[[folders]]\ninput = \"/in\"\noutput = \"/in/sorted\"")]
    #[case("[[folders]]\ninput = \"/in\"\noutput = \"/out\"\nextensions = []")]
    #[case("[[folders]]\ninput = \"/in\"\noutput = \"/out\"\nextensions = [\".jpg\"]")]
    #[case("[[folders]]\ninput = \"/in\"\noutput = \"/out\"\nextensions = [\"JPG\"]")]
    fn invalid_configs_are_rejected(#[case] toml: &str) {
        assert!(parse(toml).is_err(), "accepted: {toml}");
    }

    #[test]
    fn output_inside_input_is_fine_when_not_recursive() {
        let toml = "[[folders]]\ninput = \"/in\"\noutput = \"/in/sorted\"\nrecursive = false";
        assert!(parse(toml).is_ok());
    }

    #[test]
    fn unknown_keys_are_rejected_everywhere() {
        assert!(parse(&with_folder("wokers = 4")).is_err());
        assert!(parse(&format!("{MINIMAL}oops = 1")).is_err());
        assert!(parse(&with_folder("[dedup]\nenable = true")).is_err());
    }

    #[test]
    fn missing_config_file_is_an_io_error() {
        assert!(matches!(
            load(Path::new("/nonexistent/config.toml")),
            Err(ConfigError::Io(_))
        ));
    }

    #[test]
    fn staging_on_another_filesystem_is_rejected() {
        let output = tempfile::tempdir().unwrap();
        // /proc is always a different filesystem from a regular temp dir.
        let toml = format!(
            "[staging]\ndir = \"/proc/staging\"\n[[folders]]\ninput = \"/in\"\noutput = \"{}\"",
            output.path().display()
        );
        let err = parse(&toml).unwrap_err().to_string();
        assert!(err.contains("same filesystem"), "{err}");
    }

    #[test]
    fn unresolvable_paths_skip_the_filesystem_check() {
        assert!(check_same_filesystem("x", Path::new("/a/b"), Path::new("/c/d")).is_ok());
    }
}
