use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

const COMMON_ADVANCED_OPTIONS: &[&str] = &[
    "--magic-ipv4-addr",
    "--magic-ipv6-addr",
    "--format",
    "--verbose",
    "--no-progress",
    "--json-events",
    "--relay",
    "--show-secret",
];

const SEND_ADVANCED_OPTIONS: &[&str] = &[
    "--ticket-type",
    "--max-upload-rate",
    "--max-receivers",
    "--max-files",
    "--max-total-size",
    "--max-import-memory",
    "--session-lifetime-seconds",
];

const RECEIVE_ADVANCED_OPTIONS: &[&str] = &[
    "--retry-limit",
    "--retry-backoff-ms",
    "--connect-timeout-ms",
    "--metadata-timeout-ms",
    "--download-idle-timeout-ms",
    "--cache-dir",
    "--cache-ttl-seconds",
];

/// Locate the Cargo-built CLI binary so these tests exercise real help output.
fn sendmer_bin() -> PathBuf {
    for variable in ["CARGO_BIN_EXE_sendmer", "CARGO_BIN_EXE_SENDMER"] {
        if let Some(path) = std::env::var_os(variable) {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                return path;
            }
            return Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
        }
    }

    let test_executable = std::env::current_exe().expect("integration-test executable path");
    let debug_dir = test_executable
        .parent()
        .and_then(Path::parent)
        .expect("integration tests run below the debug directory");
    let mut binary = debug_dir.join("sendmer");
    if cfg!(windows) {
        binary.set_extension("exe");
    }
    binary
}

/// Run one CLI help path and verify Clap exits through stdout without diagnostics.
fn cli_help(arguments: &[&str]) -> String {
    let Output {
        status,
        stdout,
        stderr,
    } = Command::new(sendmer_bin())
        .args(arguments)
        .env_remove("RUST_LOG")
        .output()
        .expect("run compiled sendmer CLI");
    assert!(status.success(), "help command failed: {arguments:?}");
    assert!(
        stderr.is_empty(),
        "help command wrote diagnostics to stderr: {arguments:?}: {}",
        String::from_utf8_lossy(&stderr)
    );
    let help = String::from_utf8(stdout).expect("CLI help should be UTF-8");
    assert!(!help.trim().is_empty(), "help output should use stdout");
    help
}

#[test]
fn send_short_help_hides_advanced_options_and_full_help_keeps_them() {
    let short_help = cli_help(&["send", "-h"]);
    assert!(short_help.contains("--manifest"));
    assert!(short_help.contains("<PATH>"));
    #[cfg(feature = "clipboard")]
    assert!(short_help.contains("--clipboard"));
    assert!(!short_help.contains("Advanced options"));
    for option in COMMON_ADVANCED_OPTIONS
        .iter()
        .chain(SEND_ADVANCED_OPTIONS.iter())
    {
        assert!(
            !short_help.contains(option),
            "short send help should hide {option}"
        );
    }

    let full_help = cli_help(&["send", "--help"]);
    assert!(full_help.contains("Advanced options"));
    for option in COMMON_ADVANCED_OPTIONS
        .iter()
        .chain(SEND_ADVANCED_OPTIONS.iter())
    {
        assert!(
            full_help.contains(option),
            "full send help should include {option}"
        );
    }
}

#[test]
fn receive_short_help_keeps_only_common_choices_and_full_help_keeps_advanced_options() {
    let short_help = cli_help(&["receive", "-h"]);
    assert!(short_help.contains("<TICKET>"));
    assert!(short_help.contains("--output-dir"));
    assert!(!short_help.contains("Advanced options"));
    for option in COMMON_ADVANCED_OPTIONS
        .iter()
        .chain(RECEIVE_ADVANCED_OPTIONS.iter())
    {
        assert!(
            !short_help.contains(option),
            "short receive help should hide {option}"
        );
    }

    let full_help = cli_help(&["receive", "--help"]);
    assert!(full_help.contains("Advanced options"));
    for option in COMMON_ADVANCED_OPTIONS
        .iter()
        .chain(RECEIVE_ADVANCED_OPTIONS.iter())
    {
        assert!(
            full_help.contains(option),
            "full receive help should include {option}"
        );
    }
}

#[test]
fn top_level_examples_and_receive_alias_help_are_available() {
    let top_level_help = cli_help(&["-h"]);
    assert!(top_level_help.contains("sendmer send <PATH>"));
    assert!(top_level_help.contains("sendmer receive <TICKET>"));
    assert!(top_level_help.contains("Use --help to show all options."));

    let alias_help = cli_help(&["recv", "-h"]);
    assert!(alias_help.contains("<TICKET>"));
    assert!(alias_help.contains("--output-dir"));
    assert!(alias_help.contains("sendmer receive <TICKET>"));
    assert!(alias_help.contains("Use --help to show all options."));
    for option in COMMON_ADVANCED_OPTIONS
        .iter()
        .chain(RECEIVE_ADVANCED_OPTIONS.iter())
    {
        assert!(
            !alias_help.contains(option),
            "receive alias short help should hide {option}"
        );
    }
}
