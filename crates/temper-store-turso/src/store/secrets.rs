//! Tenant secret persistence for the Turso backend.

use libsql::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::TursoEventStore;

/// Encrypted secret row: (key_name, ciphertext, nonce).
type SecretRow = (String, Vec<u8>, Vec<u8>);

impl TursoEventStore {
    /// Upsert an encrypted secret for a tenant.
    ///
    /// If the secret already exists, the ciphertext and nonce are replaced.
    #[instrument(skip_all, fields(tenant, key_name, otel.name = "turso.upsert_secret"))]
    pub async fn upsert_secret(
        &self,
        tenant: &str,
        key_name: &str,
        ciphertext: &[u8],
        nonce: &[u8],
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO tenant_secrets (tenant, key_name, ciphertext, nonce, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, datetime('now'), datetime('now'))
             ON CONFLICT (tenant, key_name) DO UPDATE SET
                 ciphertext = excluded.ciphertext,
                 nonce = excluded.nonce,
                 updated_at = datetime('now')",
            params![tenant, key_name, ciphertext.to_vec(), nonce.to_vec()],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Delete a secret by tenant and key name.
    ///
    /// Returns `true` if a row was deleted, `false` if the secret did not exist.
    #[instrument(skip_all, fields(tenant, key_name, otel.name = "turso.delete_secret"))]
    pub async fn delete_secret(
        &self,
        tenant: &str,
        key_name: &str,
    ) -> Result<bool, PersistenceError> {
        let conn = self.configured_connection().await?;
        let affected = conn
            .execute(
                "DELETE FROM tenant_secrets WHERE tenant = ?1 AND key_name = ?2",
                params![tenant, key_name],
            )
            .await
            .map_err(storage_error)?;
        Ok(affected > 0)
    }

    /// Load all encrypted secrets for a tenant.
    ///
    /// Returns `(key_name, ciphertext, nonce)` tuples. Decryption is the
    /// caller's responsibility (via the SecretsVault).
    #[instrument(skip_all, fields(tenant, otel.name = "turso.load_tenant_secrets"))]
    pub async fn load_tenant_secrets(
        &self,
        tenant: &str,
    ) -> Result<Vec<SecretRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT key_name, ciphertext, nonce FROM tenant_secrets WHERE tenant = ?1",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;

        let mut results = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let key_name: String = row.get(0).map_err(storage_error)?;
            let ciphertext: Vec<u8> = row.get(1).map_err(storage_error)?;
            let nonce: Vec<u8> = row.get(2).map_err(storage_error)?;
            results.push((key_name, ciphertext, nonce));
        }
        Ok(results)
    }
}
