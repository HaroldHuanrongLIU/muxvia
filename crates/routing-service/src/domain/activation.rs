use secrecy::SecretString;
use uuid::Uuid;

use crate::control::protocol::{ProviderAuthentication, ProviderProtocol, Target};

pub struct ActivatedSnapshot {
    pub id: Uuid,
    pub target: Target,
    pub provider_id: Uuid,
    pub base_url: String,
    pub model: String,
    pub protocol: ProviderProtocol,
    pub authentication: ProviderAuthentication,
    pub provider_credential: SecretString,
    pub epoch: Uuid,
}
