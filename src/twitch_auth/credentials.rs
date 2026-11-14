//! Encrypted credential storage for the single `twitch_bot_auth` row.
//!
//! Tokens exist here only as AES-256-GCM envelopes produced by `crypto`, and every write
//! is a revision compare-and-swap so a concurrent reauthorization is never overwritten.

use super::{
    TwitchAuth, UNUSABLE,
    crypto::{self, Credentials},
};
use anyhow::{Context, Result, anyhow};
use rusqlite::{OptionalExtension, params, params_from_iter, types::Value};

/// Stored identity, encrypted tokens, and lifecycle revision.
type RawGrant = (String, String, Vec<u8>, i64, i64, i64);

/// A row of `twitch_bot_auth`. Ciphertext is opaque to consumers; no tokens are exposed.
pub(super) struct StoredGrant {
    pub(super) user_id: String,
    pub(super) login: String,
    pub(super) ciphertext: Vec<u8>,
    pub(super) expires_at: i64,
    pub(super) validated_at: i64,
    pub(super) revision: i64,
}

impl TwitchAuth {
    pub(super) async fn load(&self) -> Result<Option<StoredGrant>> {
        let row: Option<RawGrant> = self
            .store
            .call(|connection| {
                Ok(connection
                    .query_row(
                        "SELECT user_id,login,ciphertext,expires_at,validated_at,revision \
                         FROM twitch_bot_auth WHERE singleton=1",
                        [],
                        |row| {
                            Ok((
                                row.get(0)?,
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                                row.get(5)?,
                            ))
                        },
                    )
                    .optional()?)
            })
            .await?;
        Ok(row.map(
            |(user_id, login, ciphertext, expires_at, validated_at, revision)| StoredGrant {
                user_id,
                login,
                ciphertext,
                expires_at,
                validated_at,
                revision,
            },
        ))
    }

    /// Decrypt the stored tokens; the user id is the associated data, so a moved or
    /// renamed row fails closed instead of yielding another account's tokens.
    pub(super) fn decrypt(&self, grant: &StoredGrant) -> Result<Credentials> {
        crypto::decrypt(&self.key, &grant.user_id, &grant.ciphertext)
            .context("decrypting stored Twitch bot credentials")
    }

    /// Revision compare-and-swap on the single row. `Ok(true)` when this revision won and
    /// the grant was still usable, `false` when another writer advanced the row first.
    async fn swap(&self, sql: &'static str, args: Vec<Value>) -> Result<bool> {
        self.store
            .call(move |connection| Ok(connection.execute(sql, params_from_iter(args))? == 1))
            .await
    }

    /// Store rotated tokens immediately. Device-grant refresh tokens are single use, so a
    /// successful exchange must never be dropped just because a later validation fails.
    /// `validated_at=0` forces validation on the next check.
    pub(super) async fn persist_refresh(
        &self,
        revision: i64,
        user_id: &str,
        expires_at: i64,
        credentials: &Credentials,
    ) -> Result<Option<i64>> {
        let ciphertext = crypto::encrypt(&self.key, user_id, credentials)?;
        let changed = self
            .swap(
                "UPDATE twitch_bot_auth \
                 SET ciphertext=?1,expires_at=?2,validated_at=0,revision=revision+1 \
                 WHERE singleton=1 AND revision=?3 AND validated_at<>?4",
                vec![
                    ciphertext.into(),
                    expires_at.into(),
                    revision.into(),
                    UNUSABLE.into(),
                ],
            )
            .await?;
        Ok(changed.then_some(revision + 1))
    }

    pub(super) async fn persist_validation(
        &self,
        revision: i64,
        login: &str,
        scopes: &[String],
        expires_at: i64,
    ) -> Result<Option<i64>> {
        let changed = self
            .swap(
                "UPDATE twitch_bot_auth \
                 SET login=?1,scopes_json=?2,expires_at=?3,validated_at=?4,revision=revision+1 \
                 WHERE singleton=1 AND revision=?5 AND validated_at<>?6",
                vec![
                    login.to_owned().into(),
                    serde_json::to_string(scopes)?.into(),
                    expires_at.into(),
                    chrono::Utc::now().timestamp().into(),
                    revision.into(),
                    UNUSABLE.into(),
                ],
            )
            .await?;
        Ok(changed.then_some(revision + 1))
    }

    /// Persist a definitive revocation without deleting evidence. Returns false when the
    /// row changed concurrently (for example a CLI reauthorization), in which case the
    /// caller reloads instead of overwriting fresher credentials.
    pub(super) async fn mark_unusable(&self, revision: i64) -> Result<bool> {
        self.swap(
            "UPDATE twitch_bot_auth SET validated_at=?1,revision=revision+1 \
             WHERE singleton=1 AND revision=?2 AND validated_at<>?1",
            vec![UNUSABLE.into(), revision.into()],
        )
        .await
    }

    /// Persist a newly authorized grant, refusing to replace a different bot account
    /// unless `replace` was requested.
    pub(super) async fn store_grant(
        &self,
        user_id: &str,
        login: &str,
        scopes: &[String],
        expires_at: i64,
        credentials: &Credentials,
        replace: bool,
    ) -> Result<()> {
        let ciphertext = crypto::encrypt(&self.key, user_id, credentials)?;
        let scopes_json = serde_json::to_string(scopes)?;
        let validated_at = chrono::Utc::now().timestamp();
        let (user_id, login) = (user_id.to_owned(), login.to_owned());
        self.store
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let existing: Option<String> = transaction
                    .query_row(
                        "SELECT user_id FROM twitch_bot_auth WHERE singleton=1",
                        [],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(existing) = existing
                    && existing != user_id
                    && !replace
                {
                    return Err(anyhow!(
                        "a different Twitch bot account is already authorized; \
                         re-run with --replace to replace it"
                    ));
                }
                transaction.execute(
                    "INSERT INTO twitch_bot_auth(singleton,user_id,login,ciphertext,expires_at,scopes_json,validated_at,revision) \
                     VALUES(1,?1,?2,?3,?4,?5,?6,1) \
                     ON CONFLICT(singleton) DO UPDATE SET \
                       user_id=excluded.user_id,login=excluded.login,ciphertext=excluded.ciphertext, \
                       expires_at=excluded.expires_at,scopes_json=excluded.scopes_json, \
                       validated_at=excluded.validated_at,revision=twitch_bot_auth.revision+1",
                    params![user_id, login, ciphertext, expires_at, scopes_json, validated_at],
                )?;
                transaction.commit()?;
                Ok(())
            })
            .await
    }
}
