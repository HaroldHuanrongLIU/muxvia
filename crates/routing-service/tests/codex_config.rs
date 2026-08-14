use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use muxvia_routing::codex::{
    CodexCapability, CodexConfigCodec, CodexProbe, CodexProblem, CommandCodexProbe,
};
use tempfile::TempDir;

const ORIGINAL: &str = r#"# keep this comment
approval_policy = "on-request"
operator_models = ["keep-a", "keep-b"] # keep this array
model = "old-model"
model_provider = "old-provider"

[model_providers.existing]
name = "Existing"
base_url = "https://existing.test/v1"
wire_api = "responses"

[features]
web_search = true
"#;

fn fixture() -> (TempDir, CodexConfigCodec) {
    let home = TempDir::new().unwrap();
    let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
    fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
    fs::write(codec.config_path(), ORIGINAL).unwrap();
    (home, codec)
}

fn desired(codec: &CodexConfigCodec) -> muxvia_routing::codex::DesiredCodexState {
    codec.desired_takeover("gpt-test", "http://127.0.0.1:43123/v1", "route-secret")
}

fn desired_direct(codec: &CodexConfigCodec) -> muxvia_routing::codex::DesiredCodexState {
    codec.desired_direct(
        "model-a",
        "https://provider.example/api/v1",
        "provider-secret",
    )
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
struct FileFingerprint {
    bytes: Vec<u8>,
    mode: u32,
    size: u64,
    modified: std::time::SystemTime,
}

#[cfg(unix)]
fn fingerprint(path: &Path) -> FileFingerprint {
    let metadata = fs::metadata(path).unwrap();
    FileFingerprint {
        bytes: fs::read(path).unwrap(),
        mode: metadata.permissions().mode() & 0o777,
        size: metadata.len(),
        modified: metadata.modified().unwrap(),
    }
}

#[cfg(unix)]
#[test]
fn direct_apply_writes_exact_owned_semantics_and_never_touches_auth_json() {
    let (_home, codec) = fixture();
    fs::set_permissions(codec.config_path(), fs::Permissions::from_mode(0o640)).unwrap();
    let auth_path = codec.config_path().with_file_name("auth.json");
    fs::write(&auth_path, br#"{"sentinel":"operator-auth"}\n"#).unwrap();
    fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o640)).unwrap();
    let auth_before = fingerprint(&auth_path);
    let before = codec.inspect().unwrap();
    let direct = desired_direct(&codec);

    codec.atomic_apply(&before, &direct).unwrap();

    let document = fs::read_to_string(codec.config_path())
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    assert_eq!(document["model"].as_str(), Some("model-a"));
    assert_eq!(document["model_provider"].as_str(), Some("muxvia_codex"));
    let provider = &document["model_providers"]["muxvia_codex"];
    assert_eq!(provider["name"].as_str(), Some("Muxvia Direct"));
    assert_eq!(
        provider["base_url"].as_str(),
        Some("https://provider.example/api/v1")
    );
    assert_eq!(provider["wire_api"].as_str(), Some("responses"));
    assert_eq!(
        provider["http_headers"]["Authorization"].as_str(),
        Some("Bearer provider-secret")
    );
    assert_eq!(provider["supports_websockets"].as_bool(), Some(false));
    assert_eq!(document["approval_policy"].as_str(), Some("on-request"));
    assert_eq!(
        document["operator_models"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(toml_edit::Value::as_str)
            .collect::<Vec<_>>(),
        ["keep-a", "keep-b"]
    );
    assert_eq!(
        document["model_providers"]["existing"]["base_url"].as_str(),
        Some("https://existing.test/v1")
    );
    assert_eq!(document["features"]["web_search"].as_bool(), Some(true));
    assert_eq!(
        fs::metadata(codec.config_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
    assert_eq!(fingerprint(&auth_path), auth_before);
    assert!(!format!("{direct:?}").contains("provider-secret"));
}

#[test]
fn direct_managed_inspection_requires_the_committed_expected_state() {
    let (_home, codec) = fixture();
    let before = codec.inspect().unwrap();
    let direct = desired_direct(&codec);
    codec.atomic_apply(&before, &direct).unwrap();

    assert_eq!(
        codec.inspect().unwrap_err().code(),
        "configuration-collision"
    );
    let managed = codec.inspect_managed(&direct).unwrap();
    assert!(!format!("{managed:?}").contains("provider-secret"));
    let error = codec.inspect_managed(&desired(&codec)).unwrap_err();
    assert_eq!(error.code(), "configuration-collision");
    assert!(!format!("{error:?}\n{error}").contains("provider-secret"));
}

#[test]
fn takeover_managed_inspection_rejects_direct_expected_state() {
    let (_home, codec) = fixture();
    let before = codec.inspect().unwrap();
    let takeover = desired(&codec);
    codec.atomic_apply(&before, &takeover).unwrap();

    codec.inspect_managed(&takeover).unwrap();
    assert_eq!(
        codec
            .inspect_managed(&desired_direct(&codec))
            .unwrap_err()
            .code(),
        "configuration-collision"
    );
}

#[cfg(unix)]
#[test]
fn direct_restore_preserves_exact_prior_items_unrelated_edits_and_auth_json() {
    let home = TempDir::new().unwrap();
    let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
    fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
    let original = "model=7 # keep numeric comment\nmodel_provider = [\"legacy\", 2] # keep array comment\napproval_policy = \"never\"\n";
    fs::write(codec.config_path(), original).unwrap();
    let auth_path = codec.config_path().with_file_name("auth.json");
    fs::write(&auth_path, b"operator-auth-sentinel\n").unwrap();
    fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o600)).unwrap();
    let auth_before = fingerprint(&auth_path);
    let before = codec.inspect().unwrap();
    let direct = desired_direct(&codec);
    codec.atomic_apply(&before, &direct).unwrap();
    let rendered = fs::read_to_string(codec.config_path()).unwrap();
    fs::write(
        codec.config_path(),
        format!("operator_runtime_edit = true\n{rendered}"),
    )
    .unwrap();

    codec.restore(&before, &direct).unwrap();

    assert_eq!(
        fs::read_to_string(codec.config_path()).unwrap(),
        format!("operator_runtime_edit = true\n{original}")
    );
    assert_eq!(fingerprint(&auth_path), auth_before);
}

#[test]
fn direct_restore_removes_owned_fields_when_they_were_absent() {
    let home = TempDir::new().unwrap();
    let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
    fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
    fs::write(codec.config_path(), "approval_policy = \"never\"\n").unwrap();
    let before = codec.inspect().unwrap();
    let direct = desired_direct(&codec);

    codec.atomic_apply(&before, &direct).unwrap();
    codec.restore(&before, &direct).unwrap();

    assert_eq!(
        fs::read_to_string(codec.config_path()).unwrap(),
        "approval_policy = \"never\"\n"
    );
}

#[test]
fn desired_write_changes_only_muxvia_owned_fields_and_preserves_format_and_mode() {
    let (_home, codec) = fixture();
    #[cfg(unix)]
    {
        fs::set_permissions(codec.config_path(), fs::Permissions::from_mode(0o640)).unwrap();
    }
    let before = codec.inspect().unwrap();
    codec.atomic_apply(&before, &desired(&codec)).unwrap();
    codec.verify(&before, &desired(&codec)).unwrap();

    let rendered = fs::read_to_string(codec.config_path()).unwrap();
    assert!(rendered.contains("# keep this comment"));
    assert!(rendered.contains("approval_policy = \"on-request\""));
    assert!(rendered.contains("[model_providers.existing]"));
    assert!(rendered.contains("[features]"));
    assert!(rendered.contains("model = \"gpt-test\""));
    assert!(rendered.contains("model_provider = \"muxvia_codex\""));
    assert!(rendered.contains("[model_providers.muxvia_codex]"));
    assert!(rendered.contains("name = \"Muxvia\""));
    assert!(rendered.contains("base_url = \"http://127.0.0.1:43123/v1\""));
    assert!(rendered.contains("wire_api = \"responses\""));
    assert!(rendered.contains("\"X-Muxvia-Routing-Credential\" = \"route-secret\""));
    assert!(rendered.contains("supports_websockets = false"));
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(codec.config_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
}

#[test]
fn restore_removes_owned_fields_that_were_previously_absent() {
    let home = TempDir::new().unwrap();
    let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
    fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
    fs::write(
        codec.config_path(),
        "# unrelated\napproval_policy = \"never\"\n",
    )
    .unwrap();
    let before = codec.inspect().unwrap();
    let desired = desired(&codec);
    codec.atomic_apply(&before, &desired).unwrap();
    codec.restore(&before, &desired).unwrap();

    assert_eq!(
        fs::read_to_string(codec.config_path()).unwrap(),
        "# unrelated\napproval_policy = \"never\"\n"
    );
}

#[test]
fn restore_preserves_unrelated_edits_made_after_apply() {
    let (_home, codec) = fixture();
    let before = codec.inspect().unwrap();
    let desired = desired(&codec);
    codec.atomic_apply(&before, &desired).unwrap();
    let mut rendered = fs::read_to_string(codec.config_path()).unwrap();
    rendered.push_str("\noperator_runtime_edit = true\n");
    fs::write(codec.config_path(), rendered).unwrap();

    codec.restore(&before, &desired).unwrap();

    let restored = fs::read_to_string(codec.config_path()).unwrap();
    assert!(restored.contains("model = \"old-model\""));
    assert!(restored.contains("model_provider = \"old-provider\""));
    assert!(restored.contains("operator_runtime_edit = true"));
}

#[test]
fn restore_round_trips_numeric_array_and_owned_comments_exactly() {
    let home = TempDir::new().unwrap();
    let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
    fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
    let original = "model=7 # keep numeric comment\nmodel_provider = [\"legacy\", 2] # keep array comment\napproval_policy = \"never\"\n";
    fs::write(codec.config_path(), original).unwrap();
    let before = codec.inspect().unwrap();
    let desired = desired(&codec);

    codec.atomic_apply(&before, &desired).unwrap();
    codec.restore(&before, &desired).unwrap();

    assert_eq!(fs::read_to_string(codec.config_path()).unwrap(), original);
}

#[test]
fn restore_round_trips_inline_table_and_owned_comment_exactly() {
    let home = TempDir::new().unwrap();
    let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
    fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
    let original = "model = { kind = \"legacy\", attempts = 2 } # keep inline comment\nmodel_provider = \"legacy\" # keep string comment\n";
    fs::write(codec.config_path(), original).unwrap();
    let before = codec.inspect().unwrap();
    let desired = desired(&codec);

    codec.atomic_apply(&before, &desired).unwrap();
    codec.restore(&before, &desired).unwrap();

    assert_eq!(fs::read_to_string(codec.config_path()).unwrap(), original);
}

#[test]
fn table_shaped_owned_key_rejects_before_write() {
    let home = TempDir::new().unwrap();
    let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
    fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
    let original = "[model]\nkind = \"operator-owned-table\"\n";
    fs::write(codec.config_path(), original).unwrap();

    let error = codec.inspect().unwrap_err();

    assert_eq!(error.code(), "configuration-collision");
    assert_eq!(fs::read_to_string(codec.config_path()).unwrap(), original);
}

#[test]
fn missing_config_creates_private_parent_and_file() {
    let home = TempDir::new().unwrap();
    let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
    let before = codec.inspect().unwrap();
    codec.atomic_apply(&before, &desired(&codec)).unwrap();
    #[cfg(unix)]
    {
        assert_eq!(
            fs::metadata(codec.config_path().parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(codec.config_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[cfg(unix)]
#[test]
fn symlinked_config_is_rejected_without_following_it() {
    use std::os::unix::fs::symlink;
    let home = TempDir::new().unwrap();
    let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
    fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
    let target = home.path().join("outside.toml");
    fs::write(&target, "model = \"untouched\"\n").unwrap();
    symlink(&target, codec.config_path()).unwrap();

    let error = codec.inspect().unwrap_err();
    assert_eq!(error.code(), "configuration-write-failed");
    assert_eq!(
        fs::read_to_string(target).unwrap(),
        "model = \"untouched\"\n"
    );
}

#[test]
fn non_muxvia_reserved_provider_is_a_collision() {
    let home = TempDir::new().unwrap();
    let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
    fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
    fs::write(
        codec.config_path(),
        "[model_providers.muxvia_codex]\nname = \"Someone else\"\n",
    )
    .unwrap();

    assert_eq!(
        codec.inspect().unwrap_err().code(),
        "configuration-collision"
    );
}

#[test]
fn forged_muxvia_name_and_endpoint_are_still_a_collision() {
    for body in [
        "name = \"Muxvia\"\nbase_url = \"https://evil.example/v1\"\n",
        "name = \"Muxvia\"\nbase_url = \"http://127.0.0.1:43123/v1\"\nwire_api = \"chat\"\n",
    ] {
        let home = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
        fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
        fs::write(
            codec.config_path(),
            format!("[model_providers.muxvia_codex]\n{body}"),
        )
        .unwrap();

        assert_eq!(
            codec.inspect().unwrap_err().code(),
            "configuration-collision"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlinked_default_codex_directory_is_canonicalized() {
    use std::os::unix::fs::symlink;
    let home = TempDir::new().unwrap();
    let actual = home.path().join("actual-codex");
    fs::create_dir_all(&actual).unwrap();
    fs::write(actual.join("config.toml"), "approval_policy = \"never\"\n").unwrap();
    symlink(&actual, home.path().join(".codex")).unwrap();

    let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
    assert_eq!(
        codec.config_path(),
        fs::canonicalize(actual).unwrap().join("config.toml")
    );
    codec.inspect().unwrap();
}

#[cfg(unix)]
#[test]
fn command_probe_runs_only_version_and_help() {
    let temp = TempDir::new().unwrap();
    let log = temp.path().join("args.log");
    let executable = temp.path().join("codex-fixture");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\ncase \"$1\" in\n  --version) printf 'codex-cli 0.106.0\\n' ;;\n  --help) printf 'Usage: codex [OPTIONS] [PROMPT]\\n' ;;\n  *) exit 91 ;;\nesac\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

    let capability = CommandCodexProbe.probe(&executable).unwrap();
    assert!(matches!(capability, CodexCapability::Tested { .. }));
    assert_eq!(fs::read_to_string(log).unwrap(), "--version\n--help\n");
}

#[cfg(unix)]
#[test]
fn command_probe_maps_nonzero_status_to_incompatible_target_cli() {
    let temp = TempDir::new().unwrap();
    let executable = temp.path().join("codex-fixture");
    fs::write(&executable, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let error = CommandCodexProbe.probe(&executable).unwrap_err();
    assert_eq!(error.code(), "incompatible-target-cli");
}

#[cfg(unix)]
#[test]
fn unknown_compatible_probe_version_returns_a_warning() {
    let temp = TempDir::new().unwrap();
    let executable = temp.path().join("codex-fixture");
    fs::write(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'codex-cli 99.0.0' ;;\n --help) echo 'Usage: codex [OPTIONS]' ;;\n *) exit 91 ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

    let capability = CommandCodexProbe.probe(&executable).unwrap();
    match capability {
        CodexCapability::UnknownCompatible { version, warning } => {
            assert_eq!(version, "codex-cli 99.0.0");
            assert!(warning.contains("99.0.0"));
        }
        other => panic!("unexpected capability: {other:?}"),
    }
}

#[test]
fn verification_detects_owned_and_unrelated_changes_without_rendering_secrets() {
    let (_home, codec) = fixture();
    let before = codec.inspect().unwrap();
    let desired = desired(&codec);
    codec.atomic_apply(&before, &desired).unwrap();
    let mut rendered = fs::read_to_string(codec.config_path()).unwrap();
    rendered.push_str("\nunrelated_after_apply = true\n");
    fs::write(codec.config_path(), rendered).unwrap();

    let error = codec.verify(&before, &desired).unwrap_err();
    let displayed = format!("{error:?}\n{error}");
    assert_eq!(error.code(), "configuration-write-failed");
    assert!(!displayed.contains("route-secret"));
}

#[test]
fn identity_change_before_rename_does_not_overwrite_changed_target() {
    let home = TempDir::new().unwrap();
    let plain = CodexConfigCodec::for_user_home(home.path()).unwrap();
    fs::create_dir_all(plain.config_path().parent().unwrap()).unwrap();
    fs::write(plain.config_path(), ORIGINAL).unwrap();
    let path = plain.config_path().to_owned();
    let calls = Arc::new(Mutex::new(0));
    let hook_calls = Arc::clone(&calls);
    let codec = CodexConfigCodec::for_user_home_with_pre_rename_hook(
        home.path(),
        Arc::new(move |_temporary: &Path| {
            *hook_calls.lock().unwrap() += 1;
            fs::write(&path, "operator_changed = true\n")
        }),
    )
    .unwrap();
    let before = codec.inspect().unwrap();

    let error = codec.atomic_apply(&before, &desired(&codec)).unwrap_err();
    assert_eq!(error.code(), "configuration-write-failed");
    assert_eq!(*calls.lock().unwrap(), 1);
    assert_eq!(
        fs::read_to_string(codec.config_path()).unwrap(),
        "operator_changed = true\n"
    );
    assert!(!format!("{error:?}\n{error}").contains("route-secret"));
}

#[test]
fn exchange_mismatch_with_successful_rollback_preserves_operator_target() {
    let (_home, plain) = fixture();
    let before = plain.inspect().unwrap();
    let codec = CodexConfigCodec::for_user_home_with_exchange_hook(
        plain.config_path().parent().unwrap().parent().unwrap(),
        Arc::new(|temporary, target| {
            fs::write(temporary, "operator-displaced = true\n")?;
            fs::write(target, "muxvia-transient = true\n")?;
            Ok(false)
        }),
    )
    .unwrap();

    let error = codec.atomic_apply(&before, &desired(&codec)).unwrap_err();

    assert_eq!(error.code(), "configuration-write-failed");
    assert_eq!(
        fs::read_to_string(codec.config_path()).unwrap(),
        "operator-displaced = true\n"
    );
    assert_eq!(
        fs::read_dir(codec.config_path().parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".muxvia-"))
            .count(),
        0
    );
}

#[test]
fn exchange_mismatch_with_failed_rollback_retains_displaced_operator_artifact() {
    let (_home, plain) = fixture();
    let before = plain.inspect().unwrap();
    let codec = CodexConfigCodec::for_user_home_with_exchange_hook(
        plain.config_path().parent().unwrap().parent().unwrap(),
        Arc::new(|temporary, _target| {
            fs::write(temporary, "operator-displaced = true\n")?;
            Ok(true)
        }),
    )
    .unwrap();

    let error = codec.atomic_apply(&before, &desired(&codec)).unwrap_err();

    assert_eq!(error.code(), "recovery-required");
    assert!(!format!("{error:?}\n{error}").contains("route-secret"));
    let artifacts = fs::read_dir(codec.config_path().parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".muxvia-"))
        .collect::<Vec<_>>();
    assert!(artifacts.iter().any(|entry| {
        entry.file_type().is_ok_and(|kind| kind.is_file())
            && fs::read_to_string(entry.path())
                .is_ok_and(|contents| contents == "operator-displaced = true\n")
    }));
}

#[cfg(unix)]
#[test]
fn direct_existing_mode_is_preserved_even_under_restrictive_umask() {
    let home = TempDir::new().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--ignored")
        .arg("--exact")
        .arg("umask_subprocess_helper")
        .env("MUXVIA_UMASK_TEST_HOME", home.path())
        .status()
        .unwrap();

    assert!(status.success());
    assert_eq!(
        fs::metadata(home.path().join(".codex/config.toml"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
}

#[cfg(unix)]
#[test]
#[ignore = "invoked in an isolated subprocess by the umask regression test"]
fn umask_subprocess_helper() {
    static UMASK_LOCK: Mutex<()> = Mutex::new(());
    struct RestoreUmask(libc::mode_t);
    impl Drop for RestoreUmask {
        fn drop(&mut self) {
            unsafe { libc::umask(self.0) };
        }
    }

    let _guard = UMASK_LOCK.lock().unwrap();
    let home = Path::new(&std::env::var_os("MUXVIA_UMASK_TEST_HOME").unwrap()).to_owned();
    let codec = CodexConfigCodec::for_user_home(&home).unwrap();
    fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
    fs::write(codec.config_path(), ORIGINAL).unwrap();
    fs::set_permissions(codec.config_path(), fs::Permissions::from_mode(0o640)).unwrap();
    let before = codec.inspect().unwrap();
    let previous = unsafe { libc::umask(0o077) };
    let _restore = RestoreUmask(previous);

    codec
        .atomic_apply(&before, &desired_direct(&codec))
        .unwrap();
}

#[test]
fn absent_target_created_at_commit_is_not_replaced() {
    let home = TempDir::new().unwrap();
    let plain = CodexConfigCodec::for_user_home(home.path()).unwrap();
    let before = plain.inspect().unwrap();
    let target = plain.config_path().to_owned();
    let codec = CodexConfigCodec::for_user_home_with_pre_rename_hook(
        home.path(),
        Arc::new(move |_| fs::write(&target, "operator_created = true\n")),
    )
    .unwrap();

    assert!(codec.atomic_apply(&before, &desired(&codec)).is_err());
    assert_eq!(
        fs::read_to_string(codec.config_path()).unwrap(),
        "operator_created = true\n"
    );
}

#[cfg(unix)]
#[test]
fn parent_replaced_by_symlink_at_commit_never_writes_outside() {
    use std::os::unix::fs::symlink;
    let home = TempDir::new().unwrap();
    let outside = home.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let plain = CodexConfigCodec::for_user_home(home.path()).unwrap();
    let before = plain.inspect().unwrap();
    let parent = plain.config_path().parent().unwrap().to_owned();
    let parked = home.path().join("parked-codex");
    let outside_for_hook = outside.clone();
    let codec = CodexConfigCodec::for_user_home_with_pre_rename_hook(
        home.path(),
        Arc::new(move |_| {
            fs::rename(&parent, &parked)?;
            symlink(&outside_for_hook, &parent)
        }),
    )
    .unwrap();

    assert!(codec.atomic_apply(&before, &desired(&codec)).is_err());
    assert!(!outside.join("config.toml").exists());
}

#[allow(dead_code)]
fn problem_is_secret_free(problem: &CodexProblem, secret: &str) {
    assert!(!format!("{problem:?}\n{problem}").contains(secret));
}
