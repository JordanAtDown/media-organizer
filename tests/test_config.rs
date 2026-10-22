//! Configuration loading and validation, through the public API.

mod common;

use media_organizer::config::{self, Mode, OnConflict};
use media_organizer::error::ConfigError;
use std::io::Write;

fn load(toml: &str) -> Result<config::Config, ConfigError> {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(toml.as_bytes()).unwrap();
    config::load(f.path())
}

const FOLDER: &str = "[[folders]]\ninput = \"/in\"\noutput = \"/out\"\n";

#[test]
fn a_full_config_loads_every_section() {
    let cfg = load(
        r#"
poll_interval_secs = 15
workers = 4
stability_secs = 0
state_dir = "/state"
log_level = "debug"

[permissions]
file_mode = "0666"
dir_mode = "0777"

[staging]
max_batch = 50

[dedup]
enabled = true
check_existing = false

[no_date_cache]
enabled = true
ttl_days = 30

[health]
bind = "127.0.0.1:9000"
startup_grace_secs = 30
stale_intervals = 5

[[folders]]
name = "phone"
input = "/in"
output = "/out"
on_conflict = "skip"
use_mtime_fallback = false
write_exif_from_mtime = true
extensions = ["jpg", "mp4"]
"#,
    )
    .unwrap();

    assert_eq!(cfg.workers, 4);
    assert_eq!(cfg.permissions.file_mode, Some(Mode(0o666)));
    assert_eq!(cfg.staging.max_batch, 50);
    assert!(!cfg.dedup.check_existing);
    assert_eq!(cfg.no_date_cache.ttl_days, 30);
    assert_eq!(cfg.health.bind, "127.0.0.1:9000");
    let f = &cfg.folders[0];
    assert_eq!(f.label(), "phone");
    assert_eq!(f.on_conflict, OnConflict::Skip);
    assert!(!f.use_mtime_fallback && f.write_exif_from_mtime);
}

#[test]
fn the_effective_config_printed_by_validate_config_is_itself_valid() {
    let cfg = load(&format!("[permissions]\nfile_mode = \"0666\"\n{FOLDER}")).unwrap();
    let printed = toml::to_string_pretty(&cfg).unwrap();
    let again = config::parse(&printed).unwrap();
    assert_eq!(again.permissions.file_mode, Some(Mode(0o666)));
    assert_eq!(again.folders[0].pattern, cfg.folders[0].pattern);
}

#[test]
fn counter_token_is_rejected_with_the_migration_hint() {
    let err = load(&format!(
        "{FOLDER}pattern = \"{{stem}}_{{counter}}{{ext}}\""
    ))
    .unwrap_err();
    let msg = match err {
        ConfigError::Invalid(m) => m,
        other => panic!("unexpected {other:?}"),
    };
    assert!(msg.contains("{counter} n'existe plus"), "{msg}");
    assert!(msg.contains("on_conflict"), "{msg}");
}

#[test]
fn an_integer_mode_is_rejected_because_toml_would_read_it_as_decimal() {
    assert!(load(&format!("{FOLDER}file_mode = 777")).is_err());
    assert!(load(&format!("{FOLDER}file_mode = \"0777\"")).is_ok());
}

#[test]
fn output_inside_a_recursive_input_is_rejected() {
    let toml = "[[folders]]\ninput = \"/data\"\noutput = \"/data/sorted\"";
    assert!(load(toml).is_err());
    assert!(load(&format!("{toml}\nrecursive = false")).is_ok());
}

#[test]
fn staging_on_another_filesystem_than_output_is_rejected() {
    let out = tempfile::tempdir().unwrap();
    // /proc is never on the same device as a regular temporary directory.
    let toml = format!(
        "[staging]\ndir = \"/proc/staging\"\n[[folders]]\ninput = \"/in\"\noutput = \"{}\"",
        out.path().display()
    );
    let msg = load(&toml).unwrap_err().to_string();
    assert!(msg.contains("same filesystem"), "{msg}");
}

#[test]
fn a_typo_in_any_section_fails_loudly() {
    for toml in [
        format!("workerz = 4\n{FOLDER}"),
        format!("[dedup]\nenable = true\n{FOLDER}"),
        format!("[health]\nbnd = \"x\"\n{FOLDER}"),
        format!("{FOLDER}recursiv = true"),
    ] {
        assert!(load(&toml).is_err(), "accepted: {toml}");
    }
}

#[test]
fn a_missing_input_folder_is_not_a_config_error() {
    // NFS may mount after the container starts; this must not crash-loop.
    assert!(load("[[folders]]\ninput = \"/not/mounted/yet\"\noutput = \"/out\"").is_ok());
}

#[test]
fn defaults_keep_the_synology_clutter_out() {
    let cfg = load(FOLDER).unwrap();
    for d in ["@eaDir", "#recycle", "#snapshot", "@SynoEAStream"] {
        assert!(cfg.folders[0].excluded_dirs.iter().any(|e| e == d), "{d}");
    }
}

#[test]
fn the_shipped_example_config_is_valid_and_documents_every_default() {
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
    let cfg = config::load(&example).expect("config.example.toml must stay valid");
    let f = &cfg.folders[0];

    // The example mirrors the built-in defaults, so it cannot silently drift.
    let defaults = config::parse(FOLDER).unwrap();
    assert_eq!(cfg.poll_interval_secs, defaults.poll_interval_secs);
    assert_eq!(cfg.workers, defaults.workers);
    assert_eq!(cfg.stability_secs, defaults.stability_secs);
    assert_eq!(cfg.staging, defaults.staging);
    assert_eq!(cfg.dedup, defaults.dedup);
    assert_eq!(cfg.health, defaults.health);
    assert_eq!(f.pattern, defaults.folders[0].pattern);
    assert_eq!(f.extensions, defaults.folders[0].extensions);
    assert_eq!(f.excluded_dirs, defaults.folders[0].excluded_dirs);
    assert_eq!(f.use_mtime_fallback, defaults.folders[0].use_mtime_fallback);
    assert_eq!(
        f.write_exif_from_mtime,
        defaults.folders[0].write_exif_from_mtime
    );
}
