use std::net::IpAddr;

use url::{Host, Url};

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid-provider")]
pub struct InvalidProvider;

pub fn normalize_provider_base_url(input: &str) -> Result<String, InvalidProvider> {
    let mut url = Url::parse(input).map_err(|_| InvalidProvider)?;

    if url.cannot_be_a_base()
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(InvalidProvider);
    }

    match url.scheme() {
        "https" => {}
        "http" if is_loopback(url.host().expect("host checked above")) => {}
        _ => return Err(InvalidProvider),
    }

    let mut path = url.path().to_owned();
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    url.set_path(&path);
    Ok(url.into())
}

fn is_loopback(host: Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => IpAddr::V4(address).is_loopback(),
        Host::Ipv6(address) => IpAddr::V6(address).is_loopback(),
    }
}
