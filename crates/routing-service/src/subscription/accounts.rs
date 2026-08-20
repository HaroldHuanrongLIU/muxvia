use std::{collections::BTreeMap, fmt};

use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    config::managed_file::{FileIdentity, ManagedFile},
    home::MuxviaHome,
};

const DOCUMENT_VERSION: u32 = 1;
const PRIVATE_FILE_MODE: u32 = 0o600;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AccountAuthorizationState {
    #[default]
    Authorized,
    NeedsReauthorization,
}

#[derive(Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubscriptionAccountRecord {
    pub(crate) account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) email: Option<String>,
    pub(crate) refresh_token: String,
    pub(crate) authenticated_at: i64,
    #[serde(default)]
    pub(crate) state: AccountAuthorizationState,
}

impl fmt::Debug for SubscriptionAccountRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionAccountRecord")
            .field("account_id", &self.account_id)
            .field("email_present", &self.email.is_some())
            .field("refresh_token", &"<redacted>")
            .field("authenticated_at", &self.authenticated_at)
            .field("state", &self.state)
            .finish()
    }
}

#[derive(Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubscriptionAccountDocument {
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) accounts: BTreeMap<String, SubscriptionAccountRecord>,
    #[serde(default)]
    pub(crate) default_account_id: Option<String>,
}

impl Default for SubscriptionAccountDocument {
    fn default() -> Self {
        Self {
            version: DOCUMENT_VERSION,
            accounts: BTreeMap::new(),
            default_account_id: None,
        }
    }
}

impl fmt::Debug for SubscriptionAccountDocument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionAccountDocument")
            .field("version", &self.version)
            .field("account_ids", &self.accounts.keys().collect::<Vec<_>>())
            .field("default_account_id", &self.default_account_id)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SubscriptionAccountFileSnapshot {
    pub(crate) document: SubscriptionAccountDocument,
    identity: FileIdentity,
}

#[derive(Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StagedSubscriptionAccountMutation {
    version: u32,
    pub(crate) intent_id: Uuid,
    pub(crate) action_id: Uuid,
    pub(crate) operation: String,
    pub(crate) before: SubscriptionAccountDocument,
    pub(crate) desired: SubscriptionAccountDocument,
    pub(crate) before_sha256: String,
    pub(crate) desired_sha256: String,
    #[serde(skip)]
    identity: Option<FileIdentity>,
}

impl fmt::Debug for StagedSubscriptionAccountMutation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StagedSubscriptionAccountMutation")
            .field("version", &self.version)
            .field("intent_id", &self.intent_id)
            .field("action_id", &self.action_id)
            .field("operation", &self.operation)
            .field("before_sha256", &self.before_sha256)
            .field("desired_sha256", &self.desired_sha256)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SubscriptionAccountStoreError {
    #[error("invalid-subscription-account-file")]
    InvalidDocument,
    #[error("subscription-account-write-failed")]
    WriteFailed,
}

pub(crate) struct SubscriptionAccountStore {
    file: ManagedFile,
    staged: ManagedFile,
}

impl SubscriptionAccountStore {
    pub(crate) fn open(home: &MuxviaHome) -> Result<Self, SubscriptionAccountStoreError> {
        let file =
            ManagedFile::in_configuration_home(home.root(), "state", "subscription-accounts.json")
                .map_err(|_| SubscriptionAccountStoreError::WriteFailed)?;
        let staged = ManagedFile::in_configuration_home(
            home.root(),
            "state",
            "subscription-accounts.pending.json",
        )
        .map_err(|_| SubscriptionAccountStoreError::WriteFailed)?;
        Ok(Self { file, staged })
    }

    pub(crate) fn read(
        &self,
    ) -> Result<SubscriptionAccountFileSnapshot, SubscriptionAccountStoreError> {
        let contents = self
            .file
            .read()
            .map_err(|_| SubscriptionAccountStoreError::WriteFailed)?;
        if contents.identity.exists()
            && contents.identity.mode().map(|mode| mode & 0o777) != Some(PRIVATE_FILE_MODE)
        {
            return Err(SubscriptionAccountStoreError::InvalidDocument);
        }
        let document = if contents.identity.exists() {
            serde_json::from_slice(&contents.bytes)
                .map_err(|_| SubscriptionAccountStoreError::InvalidDocument)?
        } else {
            SubscriptionAccountDocument::default()
        };
        validate_document(&document)?;
        Ok(SubscriptionAccountFileSnapshot {
            document,
            identity: contents.identity,
        })
    }

    pub(crate) fn replace(
        &self,
        expected: &SubscriptionAccountFileSnapshot,
        desired: &SubscriptionAccountDocument,
    ) -> Result<(), SubscriptionAccountStoreError> {
        validate_document(desired)?;
        let mut bytes = serde_json::to_vec_pretty(desired)
            .map_err(|_| SubscriptionAccountStoreError::InvalidDocument)?;
        bytes.push(b'\n');
        self.file
            .replace(&expected.identity, &bytes, false)
            .map_err(|_| SubscriptionAccountStoreError::WriteFailed)?;
        let installed = self.read()?;
        if installed.document != *desired {
            return Err(SubscriptionAccountStoreError::WriteFailed);
        }
        Ok(())
    }

    pub(crate) fn stage_mutation(
        &self,
        intent_id: Uuid,
        action_id: Uuid,
        operation: &str,
        before: &SubscriptionAccountDocument,
        desired: &SubscriptionAccountDocument,
    ) -> Result<StagedSubscriptionAccountMutation, SubscriptionAccountStoreError> {
        validate_document(before)?;
        validate_document(desired)?;
        if operation.is_empty() || self.read_staged_mutation()?.is_some() {
            return Err(SubscriptionAccountStoreError::InvalidDocument);
        }
        let contents = self
            .staged
            .read()
            .map_err(|_| SubscriptionAccountStoreError::WriteFailed)?;
        let mutation = StagedSubscriptionAccountMutation {
            version: DOCUMENT_VERSION,
            intent_id,
            action_id,
            operation: operation.to_owned(),
            before: before.clone(),
            desired: desired.clone(),
            before_sha256: document_sha256(before)?,
            desired_sha256: document_sha256(desired)?,
            identity: None,
        };
        let mut bytes = serde_json::to_vec_pretty(&mutation)
            .map_err(|_| SubscriptionAccountStoreError::InvalidDocument)?;
        bytes.push(b'\n');
        self.staged
            .replace(&contents.identity, &bytes, false)
            .map_err(|_| SubscriptionAccountStoreError::WriteFailed)?;
        self.read_staged_mutation()?
            .ok_or(SubscriptionAccountStoreError::WriteFailed)
    }

    pub(crate) fn read_staged_mutation(
        &self,
    ) -> Result<Option<StagedSubscriptionAccountMutation>, SubscriptionAccountStoreError> {
        let contents = self
            .staged
            .read()
            .map_err(|_| SubscriptionAccountStoreError::WriteFailed)?;
        if !contents.identity.exists() {
            return Ok(None);
        }
        if contents.identity.mode().map(|mode| mode & 0o777) != Some(PRIVATE_FILE_MODE) {
            return Err(SubscriptionAccountStoreError::InvalidDocument);
        }
        let mut mutation: StagedSubscriptionAccountMutation =
            serde_json::from_slice(&contents.bytes)
                .map_err(|_| SubscriptionAccountStoreError::InvalidDocument)?;
        validate_staged_mutation(&mutation)?;
        mutation.identity = Some(contents.identity);
        Ok(Some(mutation))
    }

    pub(crate) fn clear_staged_mutation(
        &self,
        intent_id: Uuid,
    ) -> Result<(), SubscriptionAccountStoreError> {
        let Some(staged) = self.read_staged_mutation()? else {
            return Ok(());
        };
        if staged.intent_id != intent_id {
            return Err(SubscriptionAccountStoreError::InvalidDocument);
        }
        self.staged
            .replace(
                staged
                    .identity
                    .as_ref()
                    .ok_or(SubscriptionAccountStoreError::InvalidDocument)?,
                &[],
                true,
            )
            .map_err(|_| SubscriptionAccountStoreError::WriteFailed)
    }
}

fn document_sha256(
    document: &SubscriptionAccountDocument,
) -> Result<String, SubscriptionAccountStoreError> {
    let bytes =
        serde_json::to_vec(document).map_err(|_| SubscriptionAccountStoreError::InvalidDocument)?;
    Ok(digest(&SHA256, &bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(""))
}

fn validate_staged_mutation(
    mutation: &StagedSubscriptionAccountMutation,
) -> Result<(), SubscriptionAccountStoreError> {
    if mutation.version != DOCUMENT_VERSION
        || mutation.operation.is_empty()
        || mutation.before_sha256 != document_sha256(&mutation.before)?
        || mutation.desired_sha256 != document_sha256(&mutation.desired)?
    {
        return Err(SubscriptionAccountStoreError::InvalidDocument);
    }
    Ok(())
}

fn validate_document(
    document: &SubscriptionAccountDocument,
) -> Result<(), SubscriptionAccountStoreError> {
    if document.version != DOCUMENT_VERSION
        || document.accounts.iter().any(|(identity, account)| {
            identity.is_empty()
                || account.account_id != *identity
                || account.refresh_token.is_empty()
        })
        || document
            .default_account_id
            .as_ref()
            .is_some_and(|identity| !document.accounts.contains_key(identity))
    {
        return Err(SubscriptionAccountStoreError::InvalidDocument);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use tempfile::TempDir;

    use super::{
        AccountAuthorizationState, SubscriptionAccountDocument, SubscriptionAccountRecord,
        SubscriptionAccountStore,
    };
    use crate::home::MuxviaHome;

    fn fixture() -> (TempDir, MuxviaHome) {
        let temp = TempDir::new().expect("temporary home");
        let home = MuxviaHome::from_user_home(temp.path());
        (temp, home)
    }

    fn desired_document(refresh_token: &str) -> SubscriptionAccountDocument {
        SubscriptionAccountDocument {
            version: 1,
            accounts: BTreeMap::from([(
                "account-primary".to_owned(),
                SubscriptionAccountRecord {
                    account_id: "account-primary".to_owned(),
                    email: Some("operator@example.test".to_owned()),
                    refresh_token: refresh_token.to_owned(),
                    authenticated_at: 1_700_000_000,
                    state: AccountAuthorizationState::Authorized,
                },
            )]),
            default_account_id: Some("account-primary".to_owned()),
        }
    }

    #[test]
    fn account_file_is_atomic_private_reopenable_and_access_token_free() {
        let (_temp, home) = fixture();
        let store = SubscriptionAccountStore::open(&home).expect("open account store");
        let empty = store.read().expect("read empty account store");
        assert!(
            empty.document.accounts.is_empty(),
            "new account store was not empty"
        );

        let refresh = "SUBSCRIPTION_REFRESH_SECRET_11711";
        let access = "SUBSCRIPTION_ACCESS_SECRET_11712";
        let desired = desired_document(refresh);
        store
            .replace(&empty, &desired)
            .expect("replace account document");

        let bytes = fs::read(home.subscription_accounts_path()).expect("read account file");
        let text = String::from_utf8(bytes).expect("account file utf8");
        assert!(text.contains(refresh), "refresh token was not persisted");
        assert!(!text.contains(access), "access token was persisted");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(home.subscription_accounts_path())
                .expect("account file metadata")
                .permissions()
                .mode()
                & 0o777;
            assert!(mode == 0o600, "account file mode was not private");
        }

        let reopened = SubscriptionAccountStore::open(&home)
            .expect("reopen account store")
            .read()
            .expect("read reopened account store");
        assert!(
            reopened.document == desired,
            "reopened account document did not match the committed document"
        );
        let diagnostic = format!("{:?}", reopened.document);
        assert!(
            !diagnostic.contains(refresh),
            "account Debug leaked refresh token"
        );
        assert!(
            !diagnostic.contains(access),
            "account Debug leaked access token sentinel"
        );
    }

    #[test]
    fn account_file_rejects_identity_mismatch_corruption_and_stale_replacement() {
        let (_temp, home) = fixture();
        let store = SubscriptionAccountStore::open(&home).expect("open account store");
        let empty = store.read().expect("read empty account store");
        let first = desired_document("SUBSCRIPTION_REFRESH_SECRET_11721");
        store
            .replace(&empty, &first)
            .expect("write first account document");
        let captured = store.read().expect("capture account document");

        fs::write(
            home.subscription_accounts_path(),
            br#"{"version":1,"accounts":{"map-key":{"account_id":"different","refresh_token":"SUBSCRIPTION_REFRESH_SECRET_11722","authenticated_at":1,"state":"authorized"}},"default_account_id":null}"#,
        )
        .expect("write corrupt account file");
        let error = store.read().expect_err("identity mismatch was accepted");
        let diagnostic = format!("{error:?}");
        assert!(
            !diagnostic.contains("different"),
            "parse error exposed account content"
        );
        assert!(
            !diagnostic.contains("SUBSCRIPTION_REFRESH_SECRET_11722"),
            "parse error exposed refresh token"
        );

        let second = desired_document("SUBSCRIPTION_REFRESH_SECRET_11723");
        let write_error = store
            .replace(&captured, &second)
            .expect_err("stale replacement overwrote external content");
        let write_diagnostic = format!("{write_error:?}");
        assert!(
            !write_diagnostic.contains("SUBSCRIPTION_REFRESH_SECRET_11723"),
            "write error exposed desired refresh token"
        );
        let unchanged = fs::read_to_string(home.subscription_accounts_path())
            .expect("read externally replaced account file");
        assert!(
            unchanged.contains("different"),
            "stale replacement changed external content"
        );
    }
}
