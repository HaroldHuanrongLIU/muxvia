use axum::http::{HeaderMap, header};
use secrecy::{ExposeSecret, SecretString};
use subtle::{Choice, ConstantTimeEq};

pub const ROUTING_CREDENTIAL_HEADER: &str = "x-muxvia-routing-credential";
pub const ROUTING_CREDENTIAL_LEN: usize = 64;

pub fn routing_credential_matches(headers: &HeaderMap, expected: &SecretString) -> bool {
    let mut values = headers.get_all(ROUTING_CREDENTIAL_HEADER).iter();
    let candidate = values.next();
    let duplicate = values.next().is_some();
    let bytes = candidate.map_or(&[][..], |value| value.as_bytes());

    let padded_candidate = normalize_credential(bytes, |_| {});

    let expected_bytes = expected.expose_secret().as_bytes();
    let padded_expected = normalize_credential(expected_bytes, |_| {});

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

pub fn bearer_routing_credential_matches(headers: &HeaderMap, expected: &SecretString) -> bool {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let candidate = values.next();
    let duplicate = values.next().is_some();
    let bytes = candidate.map_or(&[][..], |value| value.as_bytes());
    let token = bytes.strip_prefix(b"Bearer ").unwrap_or(&[]);

    let padded_candidate = normalize_credential(token, |_| {});
    let expected_bytes = expected.expose_secret().as_bytes();
    let padded_expected = normalize_credential(expected_bytes, |_| {});

    let shape_is_valid = Choice::from(
        (candidate.is_some()
            && !duplicate
            && token.len() == ROUTING_CREDENTIAL_LEN
            && padded_candidate.is_ascii()
            && expected_bytes.len() == ROUTING_CREDENTIAL_LEN
            && padded_expected.is_ascii()) as u8,
    );
    bool::from(padded_candidate.ct_eq(&padded_expected) & shape_is_valid)
}

pub(crate) fn routing_credential_value_matches(
    candidate: &SecretString,
    expected: &SecretString,
) -> bool {
    let candidate_bytes = candidate.expose_secret().as_bytes();
    let expected_bytes = expected.expose_secret().as_bytes();
    let padded_candidate = normalize_credential(candidate_bytes, |_| {});
    let padded_expected = normalize_credential(expected_bytes, |_| {});
    let shape_is_valid = Choice::from(
        (candidate_bytes.len() == ROUTING_CREDENTIAL_LEN
            && expected_bytes.len() == ROUTING_CREDENTIAL_LEN
            && padded_candidate.is_ascii()
            && padded_expected.is_ascii()) as u8,
    );
    bool::from(padded_candidate.ct_eq(&padded_expected) & shape_is_valid)
}

fn normalize_credential(
    bytes: &[u8],
    mut observe_index: impl FnMut(usize),
) -> [u8; ROUTING_CREDENTIAL_LEN] {
    let mut normalized = [0_u8; ROUTING_CREDENTIAL_LEN];
    // This explicit fixed range is a credential-comparison security invariant.
    #[allow(clippy::needless_range_loop)]
    for index in 0..ROUTING_CREDENTIAL_LEN {
        observe_index(index);
        normalized[index] = bytes.get(index).copied().unwrap_or(0);
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::{ROUTING_CREDENTIAL_LEN, normalize_credential};

    #[test]
    fn credential_normalization_visits_all_fixed_indices_for_every_input_length() {
        for bytes in [
            &[][..],
            &b"short"[..],
            &[b'v'; ROUTING_CREDENTIAL_LEN][..],
            &[b'l'; ROUTING_CREDENTIAL_LEN + 1][..],
        ] {
            let mut visited = Vec::new();
            let _ = normalize_credential(bytes, |index| visited.push(index));
            assert_eq!(visited, (0..ROUTING_CREDENTIAL_LEN).collect::<Vec<_>>());
        }
    }
}
