use std::fmt;

use async_trait::async_trait;
use secrecy::SecretString;

use crate::control::protocol::SubscriptionProviderBinding;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum SubscriptionAccountResolution {
    #[error("subscription-account-unavailable")]
    Unavailable,
    #[error("subscription-account-needs-reauthorization")]
    NeedsReauthorization,
}

pub(crate) struct ResolvedSubscriptionAccess {
    account_id: String,
    access_token: SecretString,
}

impl ResolvedSubscriptionAccess {
    pub(crate) fn new(account_id: String, access_token: SecretString) -> Self {
        Self {
            account_id,
            access_token,
        }
    }

    pub(crate) fn account_id(&self) -> &str {
        &self.account_id
    }

    pub(crate) fn access_token(&self) -> &SecretString {
        &self.access_token
    }
}

impl fmt::Debug for ResolvedSubscriptionAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedSubscriptionAccess")
            .field("account_id", &"<redacted>")
            .field("access_token", &"<redacted>")
            .finish()
    }
}

#[async_trait]
pub(crate) trait SubscriptionAccountResolver: Send + Sync {
    async fn resolve_subscription_account(
        &self,
        binding: &SubscriptionProviderBinding,
    ) -> Result<ResolvedSubscriptionAccess, SubscriptionAccountResolution>;
}
