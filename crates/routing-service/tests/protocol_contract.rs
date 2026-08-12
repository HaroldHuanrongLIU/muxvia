use std::path::PathBuf;

use muxvia_routing::control::{
    framing::{FrameError, read_frame, write_frame},
    protocol::{ClientFrame, ControlProblem, TargetAction, TargetView},
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn fixture(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../protocol/fixtures")
        .join(name);
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn fixtures_round_trip_as_their_protocol_types() {
    let hello = fixture("hello.json");
    let client: ClientFrame = serde_json::from_value(hello.clone()).unwrap();
    assert_eq!(serde_json::to_value(client).unwrap(), hello);

    let target_view = fixture("initial-target-view.json");
    let view: TargetView = serde_json::from_value(target_view.clone()).unwrap();
    assert_eq!(serde_json::to_value(view).unwrap(), target_view);

    let save_provider = fixture("save-provider.json");
    let action: TargetAction = serde_json::from_value(save_provider.clone()).unwrap();
    assert_eq!(serde_json::to_value(action).unwrap(), save_provider);
}

#[tokio::test]
async fn framing_rejects_invalid_utf8() {
    let (mut writer, mut reader) = tokio::io::duplex(32);
    writer.write_all(&[0, 0, 0, 1, 0xff]).await.unwrap();
    drop(writer);

    assert_eq!(
        read_frame(&mut reader).await.unwrap_err(),
        FrameError::InvalidUtf8
    );
}

#[tokio::test]
async fn framing_rejects_invalid_json() {
    let (mut writer, mut reader) = tokio::io::duplex(32);
    writer.write_all(&[0, 0, 0, 1, b'{']).await.unwrap();
    drop(writer);

    assert_eq!(
        read_frame(&mut reader).await.unwrap_err(),
        FrameError::InvalidJson
    );
}

#[tokio::test]
async fn framing_rejects_partial_eof() {
    let (mut writer, mut reader) = tokio::io::duplex(32);
    writer.write_all(&[0, 0, 0, 4, b'{', b'}']).await.unwrap();
    drop(writer);

    assert_eq!(
        read_frame(&mut reader).await.unwrap_err(),
        FrameError::UnexpectedEof
    );
}

#[tokio::test]
async fn framing_writes_a_big_endian_length_prefix() {
    let (mut writer, mut reader) = tokio::io::duplex(64);
    let value = serde_json::json!({ "type": "hello" });
    write_frame(&mut writer, &value).await.unwrap();
    drop(writer);

    let mut encoded = Vec::new();
    reader.read_to_end(&mut encoded).await.unwrap();
    assert_eq!(&encoded[..4], &(encoded.len() as u32 - 4).to_be_bytes());
}

#[test]
fn target_projections_do_not_serialize_secrets() {
    let view: TargetView = serde_json::from_value(fixture("initial-target-view.json")).unwrap();
    let problem = ControlProblem {
        code: "invalid-action".into(),
        message: "The action cannot be completed.".into(),
    };
    let serialized = format!(
        "{}{}",
        serde_json::to_string(&view).unwrap(),
        serde_json::to_string(&problem).unwrap()
    );

    assert!(!serialized.contains("provider-secret-must-not-escape"));
    assert!(!serialized.contains("routing-secret-must-not-escape"));
}
