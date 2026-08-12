use axum::http::HeaderMap;
use secrecy::{ExposeSecret, SecretString};
use subtle::{Choice, ConstantTimeEq};

pub const ROUTING_CREDENTIAL_HEADER: &str = "x-muxvia-routing-credential";
pub const ROUTING_CREDENTIAL_LEN: usize = 64;

pub fn routing_credential_matches(headers: &HeaderMap, expected: &SecretString) -> bool {
    let mut values = headers.get_all(ROUTING_CREDENTIAL_HEADER).iter();
    let candidate = values.next();
    let duplicate = values.next().is_some();
    let bytes = candidate.map_or(&[][..], |value| value.as_bytes());

    let mut padded_candidate = [0_u8; ROUTING_CREDENTIAL_LEN];
    for (slot, byte) in padded_candidate.iter_mut().zip(bytes.iter().copied()) {
        *slot = byte;
    }

    let expected_bytes = expected.expose_secret().as_bytes();
    let mut padded_expected = [0_u8; ROUTING_CREDENTIAL_LEN];
    for (slot, byte) in padded_expected
        .iter_mut()
        .zip(expected_bytes.iter().copied())
    {
        *slot = byte;
    }

    let shape_is_valid = Choice::from(
        (candidate.is_some()
            && !duplicate
            && bytes.len() == ROUTING_CREDENTIAL_LEN
            && padded_candidate.is_ascii()
            && expected_bytes.len() == ROUTING_CREDENTIAL_LEN
            && padded_expected.is_ascii()) as u8,
    );
    bool::from(padded_candidate.ct_eq(&padded_expected) & shape_is_valid)
}
