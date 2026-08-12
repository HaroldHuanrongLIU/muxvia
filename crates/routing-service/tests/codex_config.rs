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
    codec.desired("gpt-test", "http://127.0.0.1:43123/v1", "route-secret")
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

#[allow(dead_code)]
fn problem_is_secret_free(problem: &CodexProblem, secret: &str) {
    assert!(!format!("{problem:?}\n{problem}").contains(secret));
}
