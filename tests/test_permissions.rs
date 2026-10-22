//! Modes exist because the NFS export is `all_squash` and Nextcloud answers 403
//! for anything its squashed identity cannot read. `set_permissions` ignores the
//! umask, so these assertions are stable on every machine.

mod common;

use common::*;
use std::os::unix::fs::PermissionsExt;

fn chmod(p: &std::path::Path, mode: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn configured_modes_apply_to_the_file_and_to_every_directory_down_from_output() {
    let env = Env::new();
    let src = env.put("IMG-20250626-WA0001.jpg", &jpeg_without_exif());
    chmod(&src, 0o600); // arrives restrictive, as many upload apps leave it
    let mut scanner = env.scanner(
        "[permissions]\nfile_mode = \"0666\"\ndir_mode = \"0777\"",
        "",
    );

    env.scan(&mut scanner);

    assert_eq!(mode_of(&env.out("2025/06/IMG-20250626-WA0001.jpg")), 0o666);
    assert_eq!(mode_of(&env.out("2025/06")), 0o777);
    // The year directory too: create_dir_all made both levels under the umask.
    assert_eq!(mode_of(&env.out("2025")), 0o777);
    assert_eq!(mode_of(env.output.path()), 0o777);
}

#[test]
fn a_file_that_went_through_exif_injection_still_gets_the_configured_mode() {
    let env = Env::new();
    // No EXIF + a dated name: the date is injected into a staged *copy*, which
    // tempfile creates at 0600 — it must not leak through to the destination.
    env.put("IMG-20250626-WA0001.jpg", &jpeg_without_exif());
    let mut scanner = env.scanner("[permissions]\nfile_mode = \"0664\"", "");

    let sum = env.scan(&mut scanner);

    assert_eq!(sum.exif_injected, 1);
    assert_eq!(mode_of(&env.out("2025/06/IMG-20250626-WA0001.jpg")), 0o664);
}

#[test]
fn folder_modes_override_the_global_ones() {
    let env = Env::new();
    env.put("IMG-20250626-WA0001.jpg", &jpeg_without_exif());
    let mut scanner = env.scanner(
        "[permissions]\nfile_mode = \"0644\"\ndir_mode = \"0755\"",
        "file_mode = \"0666\"\ndir_mode = \"0777\"",
    );

    env.scan(&mut scanner);

    assert_eq!(mode_of(&env.out("2025/06/IMG-20250626-WA0001.jpg")), 0o666);
    assert_eq!(mode_of(&env.out("2025/06")), 0o777);
}

#[test]
fn without_configured_modes_no_chmod_is_issued() {
    let env = Env::new();
    let src = env.put("IMG-20250626-WA0001.jpg", b"not a real jpeg");
    chmod(&src, 0o640);
    chmod(env.output.path(), 0o750);
    let mut scanner = env.scanner("", "write_exif_from_filename = false");

    env.scan(&mut scanner);

    assert_eq!(
        mode_of(&env.out("2025/06/IMG-20250626-WA0001.jpg")),
        0o640,
        "the source's own mode is kept"
    );
    assert_eq!(
        mode_of(env.output.path()),
        0o750,
        "the output root is not touched"
    );
}

#[test]
fn the_chain_stops_at_output_and_never_climbs_above_it() {
    let env = Env::new();
    let parent = env.output.path().parent().unwrap().to_path_buf();
    let before = mode_of(&parent);
    env.put("IMG-20250626-WA0001.jpg", &jpeg_without_exif());
    let mut scanner = env.scanner("[permissions]\ndir_mode = \"0777\"", "");

    env.scan(&mut scanner);

    assert_eq!(mode_of(&parent), before);
}

#[test]
fn modes_are_applied_on_replay_after_a_crash_too() {
    // The modes travel in the journal, so a replayed entry is not left restrictive.
    use media_organizer::staging::Staging;
    let env = Env::new();
    let cfg = env.config(
        "[permissions]\nfile_mode = \"0666\"\ndir_mode = \"0777\"",
        "",
    );
    let folder = &cfg.folders[0];
    let staging = Staging::new(
        folder.staging_dir(&cfg.staging),
        folder.output.clone(),
        folder.file_mode(&cfg.permissions),
        folder.dir_mode(&cfg.permissions),
    )
    .unwrap();
    let src = env.put("IMG-20250626-WA0001.jpg", &jpeg_without_exif());
    let c = media_organizer::plan::Candidate {
        path: src,
        size: 10,
        mtime: 1,
    };
    let media_organizer::plan::PlanOutcome::Planned(plan) =
        media_organizer::plan::plan_file(&c, folder)
    else {
        panic!("expected a plan");
    };
    let resolved = media_organizer::resolve::Resolved {
        final_dest: env.out("2025/06/IMG-20250626-WA0001.jpg"),
        plan,
        overwrite: false,
    };
    let entries = staging.prepare(vec![resolved]).prepared;
    staging.write_journal(&entries).unwrap();
    drop(staging);

    let mut scanner = env.scanner_with(cfg, false);
    env.scan(&mut scanner);

    assert_eq!(mode_of(&env.out("2025/06/IMG-20250626-WA0001.jpg")), 0o666);
    assert_eq!(mode_of(&env.out("2025")), 0o777);
}
