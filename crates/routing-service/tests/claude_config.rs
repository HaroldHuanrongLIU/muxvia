use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use muxvia_routing::{
    claude::{
        ClaudeCapability, ClaudeConfigCodec, ClaudeProbe, ClaudeRuntimeShadow, CommandClaudeProbe,
    },
    control::protocol::{
        ClaudeBlockingSelector, ClaudeHostManagedState, ClaudePreflightContext,
        ClaudeSelectorState, CompatibilityClassification,
    },
};
use tempfile::TempDir;

const UNRELATED: &str = include_str!("fixtures/claude/unrelated.json");
const SEMANTIC_VALUES: &str = include_str!("fixtures/claude/semantic-values.json");

fn context(cwd: &Path) -> ClaudePreflightContext {
    ClaudePreflightContext {
        claude_config_dir: None,
        selector_state: ClaudeSelectorState::Unset,
        blocking_selector: None,
        host_managed_state: ClaudeHostManagedState::Unmanaged,
        cwd: cwd.to_string_lossy().into_owned(),
    }
}

fn fixture(source: &str) -> (TempDir, ClaudeConfigCodec) {
    let home = TempDir::new().unwrap();
    let codec = ClaudeConfigCodec::for_user_home(home.path()).unwrap();
    fs::create_dir_all(codec.settings_path().parent().unwrap()).unwrap();
    fs::write(codec.settings_path(), source).unwrap();
    (home, codec)
}

fn secret_json_string_matches(
    value: &serde_json::Value,
    expected: &str,
) -> Result<(), &'static str> {
    if value.as_str() == Some(expected) {
        Ok(())
    } else {
        Err("secret JSON value mismatch")
    }
}

#[test]
fn absent_file_takeover_apply_owns_four_fields_and_restore_removes_it() {
    let home = TempDir::new().unwrap();
    let codec = ClaudeConfigCodec::for_user_home(home.path()).unwrap();
    let before = codec.inspect().unwrap();
    let desired = codec.desired_takeover(
        "claude-sonnet-test",
        "http://127.0.0.1:43124",
        "routing-secret",
    );

    codec.atomic_apply(&before, &desired).unwrap();

    let applied: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(codec.settings_path()).unwrap()).unwrap();
    let env = applied["env"].as_object().unwrap();
    secret_json_string_matches(&applied["env"]["ANTHROPIC_AUTH_TOKEN"], "routing-secret").unwrap();
    assert_eq!(
        applied["env"]["ANTHROPIC_BASE_URL"],
        "http://127.0.0.1:43124"
    );
    assert_eq!(applied["env"]["ANTHROPIC_MODEL"], "claude-sonnet-test");
    assert!(!env.contains_key("ANTHROPIC_API_KEY"));
    assert_eq!(env.len(), 3);
    codec.restore(&before, &desired).unwrap();
    assert!(!codec.settings_path().exists());
}

#[cfg(unix)]
#[test]
fn apply_preserves_all_unrelated_json_values_and_existing_mode() {
    for source in [UNRELATED, SEMANTIC_VALUES] {
        let (_home, codec) = fixture(source);
        fs::set_permissions(codec.settings_path(), fs::Permissions::from_mode(0o640)).unwrap();
        let before_tree: serde_json::Value = serde_json::from_str(source).unwrap();
        let before = codec.inspect().unwrap();
        let desired = codec.desired_takeover("new-model", "http://127.0.0.1:9", "route-secret");

        codec.atomic_apply(&before, &desired).unwrap();

        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(codec.settings_path()).unwrap()).unwrap();
        for (key, value) in before_tree.as_object().unwrap() {
            if key != "env" {
                assert_eq!(after.get(key), Some(value));
            }
        }
        assert_eq!(
            after["env"]["OPERATOR_FLAG"],
            before_tree["env"]["OPERATOR_FLAG"]
        );
        assert_eq!(after["model"], before_tree["model"]);
        assert!(after["env"].get("ANTHROPIC_API_KEY").is_none());
        assert_eq!(
            fs::metadata(codec.settings_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }
}

#[test]
fn restore_reinstates_exact_prior_owned_values_and_preserves_later_unrelated_edits() {
    let (_home, codec) = fixture(UNRELATED);
    let before = codec.inspect().unwrap();
    let desired = codec.desired_takeover("new-model", "http://127.0.0.1:9", "route-secret");
    codec.atomic_apply(&before, &desired).unwrap();
    let mut live: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(codec.settings_path()).unwrap()).unwrap();
    live["operatorAfterApply"] = serde_json::json!({"keep": [1, 2, 3]});
    fs::write(
        codec.settings_path(),
        serde_json::to_vec_pretty(&live).unwrap(),
    )
    .unwrap();

    codec.restore(&before, &desired).unwrap();

    let restored: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(codec.settings_path()).unwrap()).unwrap();
    assert_eq!(
        restored["env"]["ANTHROPIC_BASE_URL"],
        "https://prior.example"
    );
    assert_eq!(restored["env"]["ANTHROPIC_AUTH_TOKEN"], "prior-token");
    assert!(restored["env"].get("ANTHROPIC_MODEL").is_none());
    assert_eq!(
        restored["operatorAfterApply"]["keep"],
        serde_json::json!([1, 2, 3])
    );
}

#[test]
fn invalid_json_or_non_object_root_or_env_fails_before_write() {
    for source in ["{not-json", "[]", r#"{"env": 7}"#] {
        let (_home, codec) = fixture(source);
        let bytes = fs::read(codec.settings_path()).unwrap();
        let error = codec.inspect().unwrap_err();
        assert!(matches!(
            error.code(),
            "invalid-configuration" | "configuration-collision"
        ));
        assert_eq!(fs::read(codec.settings_path()).unwrap(), bytes);
    }
}

#[cfg(unix)]
#[test]
fn configuration_home_symlink_is_canonicalized_but_final_file_symlink_is_rejected() {
    use std::os::unix::fs::symlink;
    let home = TempDir::new().unwrap();
    let actual = home.path().join("actual-claude");
    fs::create_dir_all(&actual).unwrap();
    fs::write(actual.join("settings.json"), "{}").unwrap();
    symlink(&actual, home.path().join(".claude")).unwrap();
    let codec = ClaudeConfigCodec::for_user_home(home.path()).unwrap();
    assert_eq!(
        codec.settings_path(),
        fs::canonicalize(&actual).unwrap().join("settings.json")
    );
    codec.inspect().unwrap();

    fs::remove_file(codec.settings_path()).unwrap();
    let outside = home.path().join("outside.json");
    fs::write(&outside, r#"{"sentinel":"untouched"}"#).unwrap();
    symlink(&outside, codec.settings_path()).unwrap();
    assert_eq!(
        codec.inspect().unwrap_err().code(),
        "configuration-write-failed"
    );
    assert_eq!(
        fs::read_to_string(outside).unwrap(),
        r#"{"sentinel":"untouched"}"#
    );
}

#[test]
fn identity_races_never_replace_operator_changes() {
    let home = TempDir::new().unwrap();
    let plain = ClaudeConfigCodec::for_user_home(home.path()).unwrap();
    fs::create_dir_all(plain.settings_path().parent().unwrap()).unwrap();
    fs::write(plain.settings_path(), "{}").unwrap();
    let before = plain.inspect().unwrap();
    let target = plain.settings_path().to_owned();
    let codec = ClaudeConfigCodec::for_user_home_with_pre_rename_hook(
        home.path(),
        Arc::new(move |_| fs::write(&target, r#"{"operatorChanged":true}"#)),
    )
    .unwrap();

    let error = codec
        .atomic_apply(
            &before,
            &codec.desired_takeover("model", "http://127.0.0.1:9", "route-secret"),
        )
        .unwrap_err();
    assert_eq!(error.code(), "configuration-write-failed");
    assert_eq!(
        fs::read_to_string(codec.settings_path()).unwrap(),
        r#"{"operatorChanged":true}"#
    );
    assert!(!format!("{error:?}\n{error}").contains("route-secret"));
}

#[test]
fn substituted_temporary_path_is_never_installed() {
    let (_home, plain) = fixture("{}");
    let before = plain.inspect().unwrap();
    let home = plain
        .settings_path()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned();
    let codec = ClaudeConfigCodec::for_user_home_with_pre_rename_hook(
        &home,
        Arc::new(move |temporary| {
            fs::rename(temporary, temporary.with_extension("parked"))?;
            fs::write(
                temporary,
                r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"attacker-substitute"}}"#,
            )
        }),
    )
    .unwrap();

    let error = codec
        .atomic_apply(
            &before,
            &codec.desired_takeover("model", "http://127.0.0.1:9", "routing-secret"),
        )
        .unwrap_err();

    assert_eq!(error.code(), "configuration-write-failed");
    assert_eq!(fs::read_to_string(codec.settings_path()).unwrap(), "{}");
    let diagnostic = format!("{error:?}\n{error}");
    let credential_is_redacted = !diagnostic.contains("routing-secret");
    let substitute_is_redacted = !diagnostic.contains("attacker-substitute");
    assert!(
        credential_is_redacted,
        "Claude configuration error rendered credential bytes"
    );
    assert!(
        substitute_is_redacted,
        "Claude configuration error rendered substitute bytes"
    );
}

#[test]
fn third_state_drift_blocks_restore_without_exposing_secrets() {
    let (_home, codec) = fixture("{}");
    let before = codec.inspect().unwrap();
    let desired = codec.desired_takeover("model", "http://127.0.0.1:9", "route-secret");
    codec.atomic_apply(&before, &desired).unwrap();
    fs::write(
        codec.settings_path(),
        r#"{"env":{"ANTHROPIC_MODEL":"operator-model"}}"#,
    )
    .unwrap();

    let error = codec.restore(&before, &desired).unwrap_err();
    assert_eq!(error.code(), "recovery-required");
    assert!(!format!("{error:?}\n{error}").contains("route-secret"));
    assert!(!format!("{desired:?}").contains("route-secret"));
}

#[test]
fn every_documented_selector_and_host_managed_mode_blocks_without_mutation() {
    let selectors = [
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
        "CLAUDE_CODE_USE_MANTLE",
        "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    ];
    for selector in selectors {
        for value in [
            serde_json::json!(1),
            serde_json::json!(true),
            serde_json::json!("unexpected"),
        ] {
            let source = serde_json::json!({"env": {selector: value}}).to_string();
            let (home, codec) = fixture(&source);
            let before = fs::read(codec.settings_path()).unwrap();
            let error = codec.preflight(&context(home.path())).unwrap_err();
            assert_eq!(error.code(), "provider-mode-active");
            assert_eq!(fs::read(codec.settings_path()).unwrap(), before);
        }
    }
    for value in [
        serde_json::json!(1),
        serde_json::json!(true),
        serde_json::json!("unknown"),
    ] {
        let source =
            serde_json::json!({"env": {"CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST": value}}).to_string();
        let (home, codec) = fixture(&source);
        assert_eq!(
            codec.preflight(&context(home.path())).unwrap_err().code(),
            "provider-mode-active"
        );
    }
}

#[test]
fn documented_disabled_and_empty_selector_values_do_not_block() {
    for value in [
        serde_json::json!(0),
        serde_json::json!(false),
        serde_json::json!(""),
    ] {
        let source = serde_json::json!({"env": {
            "CLAUDE_CODE_USE_BEDROCK": value,
            "CLAUDE_CODE_USE_VERTEX": value,
            "CLAUDE_CODE_USE_FOUNDRY": value,
            "CLAUDE_CODE_USE_MANTLE": value,
            "CLAUDE_CODE_USE_ANTHROPIC_AWS": value,
            "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST": value
        }})
        .to_string();
        let (home, codec) = fixture(&source);
        codec.preflight(&context(home.path())).unwrap();
    }
}

#[test]
fn normalized_context_blocks_nondefault_home_active_or_unknown_provider_modes() {
    let home = TempDir::new().unwrap();
    let codec = ClaudeConfigCodec::for_user_home(home.path()).unwrap();
    let mut cases = Vec::new();
    let mut nondefault = context(home.path());
    nondefault.claude_config_dir =
        Some(home.path().join("elsewhere").to_string_lossy().into_owned());
    cases.push((nondefault, "unsupported-configuration-home"));
    for selector_state in [
        ClaudeSelectorState::Enabled,
        ClaudeSelectorState::UnknownNonempty,
    ] {
        let mut active = context(home.path());
        active.selector_state = selector_state;
        active.blocking_selector = Some(ClaudeBlockingSelector::Vertex);
        cases.push((active, "provider-mode-active"));
    }
    for host_managed_state in [
        ClaudeHostManagedState::Managed,
        ClaudeHostManagedState::Unknown,
    ] {
        let mut active = context(home.path());
        active.host_managed_state = host_managed_state;
        active.blocking_selector = Some(ClaudeBlockingSelector::HostManaged);
        cases.push((active, "provider-mode-active"));
    }
    for (context, expected) in cases {
        assert_eq!(codec.preflight(&context).unwrap_err().code(), expected);
        assert!(!codec.settings_path().exists());
    }
}

#[test]
fn inconsistent_normalized_context_is_rejected_without_configuration_access() {
    let home = TempDir::new().unwrap();
    let codec = ClaudeConfigCodec::for_user_home(home.path()).unwrap();
    for (selector_state, blocking_selector) in [
        (ClaudeSelectorState::Enabled, None),
        (ClaudeSelectorState::UnknownNonempty, None),
        (
            ClaudeSelectorState::Unset,
            Some(ClaudeBlockingSelector::Vertex),
        ),
    ] {
        let mut invalid = context(home.path());
        invalid.selector_state = selector_state;
        invalid.blocking_selector = blocking_selector;
        let error = codec.preflight(&invalid).unwrap_err();
        assert_eq!(error.code(), "preflight-context-required");
        assert!(!codec.settings_path().exists());
    }
}

#[test]
fn non_directory_user_home_is_reported_as_unsupported() {
    let root = TempDir::new().unwrap();
    let not_a_home = root.path().join("not-a-home");
    fs::write(&not_a_home, "operator file").unwrap();

    let error = match ClaudeConfigCodec::for_user_home(&not_a_home) {
        Ok(_) => panic!("non-directory user home was accepted"),
        Err(error) => error,
    };

    assert_eq!(error.code(), "unsupported-configuration-home");
    assert_eq!(fs::read_to_string(not_a_home).unwrap(), "operator file");
}

#[test]
fn managed_shared_and_local_owned_shadows_block_with_source_but_unrelated_values_do_not() {
    let home = TempDir::new().unwrap();
    let project = home.path().join("project");
    fs::create_dir_all(project.join(".claude")).unwrap();
    let managed = home.path().join("managed-settings.json");
    let codec =
        ClaudeConfigCodec::for_user_home_with_managed_settings(home.path(), vec![managed.clone()])
            .unwrap();
    for path in [
        managed,
        project.join(".claude/settings.json"),
        project.join(".claude/settings.local.json"),
    ] {
        fs::write(&path, include_str!("fixtures/claude/shadow.json")).unwrap();
        let error = codec.preflight(&context(&project)).unwrap_err();
        assert_eq!(error.code(), "shadowing-configuration");
        let canonical_path = fs::canonicalize(&path).unwrap();
        assert_eq!(error.path(), Some(canonical_path.as_path()));
        fs::write(&path, r#"{"env":{"UNRELATED":"keep"},"permissions":{}}"#).unwrap();
        codec.preflight(&context(&project)).unwrap();
    }
}

#[test]
fn preflight_reports_new_process_boundary_and_unobservable_runtime_shadows() {
    let home = TempDir::new().unwrap();
    let codec = ClaudeConfigCodec::for_user_home(home.path()).unwrap();
    let report = codec.preflight(&context(home.path())).unwrap();
    assert!(report.restart_required);
    assert_eq!(
        report.unobservable_shadows,
        [
            ClaudeRuntimeShadow::SettingsFlag,
            ClaudeRuntimeShadow::ModelFlag,
            ClaudeRuntimeShadow::InteractiveModel,
            ClaudeRuntimeShadow::ResumedSession,
            ClaudeRuntimeShadow::ExternalEnvironment,
        ]
    );
}

#[cfg(unix)]
fn fake_claude(temp: &TempDir, version: &str, help: &str, exit: i32) -> (PathBuf, PathBuf) {
    let log = temp.path().join("args.log");
    let executable = temp.path().join("claude-fixture");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\ncase \"$1\" in\n --version) printf '%s\\n' '{}' ; exit {} ;;\n --help) printf '%s\\n' '{}' ; exit {} ;;\n *) exit 91 ;;\nesac\n",
            log.display(), version, exit, help, exit
        ),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    (executable, log)
}

#[cfg(unix)]
#[test]
fn command_probe_uses_only_read_only_version_and_help_surfaces() {
    let temp = TempDir::new().unwrap();
    let (executable, log) = fake_claude(
        &temp,
        "2.1.37 (Claude Code)",
        "Usage: claude [options] [command]\n--settings <file>\n--model <model>",
        0,
    );
    assert!(matches!(
        CommandClaudeProbe.probe(&executable).unwrap(),
        ClaudeCapability::Tested { .. }
    ));
    assert_eq!(fs::read_to_string(log).unwrap(), "--version\n--help\n");
}

#[cfg(unix)]
#[test]
fn reconciliation_probe_projects_only_exact_version_and_closed_classification() {
    let temp = TempDir::new().unwrap();
    let (executable, _) = fake_claude(
        &temp,
        "2.1.37 (Claude Code)",
        "Usage: claude [options]\n--settings <file>\n--model <model>",
        0,
    );
    let capability = CommandClaudeProbe.probe(&executable).unwrap();

    assert_eq!(capability.version(), "2.1.37 (Claude Code)");
    assert_eq!(
        capability.classification(),
        CompatibilityClassification::Tested
    );
    assert_eq!(
        format!("{capability:?}"),
        "Tested { version: \"2.1.37 (Claude Code)\" }"
    );
}

#[cfg(unix)]
#[test]
fn reconciliation_probe_rejects_malformed_missing_contradictory_and_non_utf8_output() {
    for (case, version_command, help_command) in [
        (
            "dot-only version",
            "printf '. (Claude Code)\\n'",
            "printf 'Usage: claude [options]\\n--settings <file>\\n--model <model>\\n'",
        ),
        (
            "missing version",
            "printf ''",
            "printf 'Usage: claude [options]\\n--settings <file>\\n--model <model>\\n'",
        ),
        (
            "contradictory help",
            "printf '2.1.37 (Claude Code)\\n'",
            "printf 'Usage: codex [options]\\n--settings <file>\\n--model <model>\\n'",
        ),
        (
            "missing --settings capability",
            "printf '2.1.37 (Claude Code)\\n'",
            "printf 'Usage: claude [options]\\n--model <model>\\nmissing-settings-probe-output-sentinel\\n'",
        ),
        (
            "missing --model capability",
            "printf '2.1.37 (Claude Code)\\n'",
            "printf 'Usage: claude [options]\\n--settings <file>\\nmissing-model-probe-output-sentinel\\n'",
        ),
        (
            "missing both capability markers",
            "printf '2.1.37 (Claude Code)\\n'",
            "printf 'Usage: claude [options]\\nmissing-markers-probe-output-sentinel\\n'",
        ),
        (
            "multiline raw version",
            "printf '2.1.37 (Claude Code)\\nraw-probe-output-sentinel\\n'",
            "printf 'Usage: claude [options]\\n--settings <file>\\n--model <model>\\n'",
        ),
        (
            "non-UTF-8 version",
            "printf '\\377\\n'",
            "printf 'Usage: claude [options]\\n--settings <file>\\n--model <model>\\n'",
        ),
    ] {
        let temp = TempDir::new().unwrap();
        let executable = temp.path().join("claude-incompatible-fixture");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n --version) {version_command} ;;\n --help) {help_command} ;;\n *) exit 91 ;;\nesac\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

        let error = match CommandClaudeProbe.probe(&executable) {
            Ok(capability) => panic!("{case} was accepted: {capability:?}"),
            Err(error) => error,
        };
        assert_eq!(error.code(), "incompatible-target-cli", "{case}");
        let diagnostic = format!("{error:?}\n{error}");
        for sentinel in [
            "raw-probe-output-sentinel",
            "missing-settings-probe-output-sentinel",
            "missing-model-probe-output-sentinel",
            "missing-markers-probe-output-sentinel",
        ] {
            assert!(!diagnostic.contains(sentinel));
        }
    }
}

#[cfg(unix)]
#[test]
fn command_probe_classifies_unknown_compatible_and_incompatible_fake_versions() {
    let unknown = TempDir::new().unwrap();
    let (unknown_executable, _) = fake_claude(
        &unknown,
        "99.0.0 (Claude Code)",
        "Usage: claude [options] [command]\n--settings <file>\n--model <model>",
        0,
    );
    match CommandClaudeProbe.probe(&unknown_executable).unwrap() {
        ClaudeCapability::UnknownCompatible { version, warning } => {
            assert_eq!(version, "99.0.0 (Claude Code)");
            assert!(warning.contains("99.0.0"));
        }
        other => panic!("unexpected capability: {other:?}"),
    }

    let incompatible = TempDir::new().unwrap();
    let (incompatible_executable, _) = fake_claude(&incompatible, "2.1.37", "not a CLI", 0);
    assert_eq!(
        CommandClaudeProbe
            .probe(&incompatible_executable)
            .unwrap_err()
            .code(),
        "incompatible-target-cli"
    );
}

#[cfg(unix)]
#[test]
fn missing_file_is_private_even_under_restrictive_umask() {
    let home = TempDir::new().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--ignored")
        .arg("--exact")
        .arg("claude_umask_subprocess_helper")
        .env("MUXVIA_CLAUDE_UMASK_TEST_HOME", home.path())
        .status()
        .unwrap();

    assert!(status.success());
    assert_eq!(
        fs::metadata(home.path().join(".claude/settings.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[test]
#[ignore = "invoked in an isolated subprocess by the umask regression test"]
fn claude_umask_subprocess_helper() {
    static UMASK_LOCK: Mutex<()> = Mutex::new(());
    struct RestoreUmask(libc::mode_t);
    impl Drop for RestoreUmask {
        fn drop(&mut self) {
            unsafe { libc::umask(self.0) };
        }
    }
    let _guard = UMASK_LOCK.lock().unwrap();
    let home = Path::new(&std::env::var_os("MUXVIA_CLAUDE_UMASK_TEST_HOME").unwrap()).to_owned();
    let codec = ClaudeConfigCodec::for_user_home(&home).unwrap();
    let before = codec.inspect().unwrap();
    let previous = unsafe { libc::umask(0o077) };
    let _restore = RestoreUmask(previous);
    codec
        .atomic_apply(
            &before,
            &codec.desired_takeover("model", "http://127.0.0.1:9", "secret"),
        )
        .unwrap();
}
