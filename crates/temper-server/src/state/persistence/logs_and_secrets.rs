use temper_store_turso::TursoTrajectoryInsert;

use super::super::trajectory::TrajectoryEntry;
use super::super::{DesignTimeEvent, ServerState};
use super::MetadataBackend;

impl ServerState {
    /// Broadcast and persist a design-time event to Turso.
    pub async fn emit_design_time_event(&self, event: DesignTimeEvent) -> Result<(), String> {
        // Persist to Turso.
        if let Some(turso) = self.persistent_store() {
            turso
                .insert_design_time_event(
                    &event.kind,
                    &event.entity_type,
                    &event.tenant,
                    &event.summary,
                    event.level.as_deref(),
                    event.passed,
                    event.step_number.map(i64::from),
                    event.total_steps.map(i64::from),
                )
                .await
                .map_err(|e| {
                    format!(
                        "failed to persist design-time event {} for {}/{}: {e}",
                        event.kind, event.tenant, event.entity_type
                    )
                })?;
        }
        // Broadcast via SSE (keep for real-time UI).
        let _ = self.design_time_tx.send(event);
        Ok(())
    }

    /// Persist a trajectory entry to Turso (single source of truth).
    pub async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String> {
        let Some(turso) = self.persistent_store() else {
            return Ok(());
        };
        turso
            .persist_trajectory(TursoTrajectoryInsert {
                tenant: &entry.tenant,
                entity_type: &entry.entity_type,
                entity_id: &entry.entity_id,
                action: &entry.action,
                success: entry.success,
                from_status: entry.from_status.as_deref(),
                to_status: entry.to_status.as_deref(),
                error: entry.error.as_deref(),
                agent_id: entry.agent_id.as_deref(),
                session_id: entry.session_id.as_deref(),
                authz_denied: entry.authz_denied,
                denied_resource: entry.denied_resource.as_deref(),
                denied_module: entry.denied_module.as_deref(),
                source: entry.source.as_ref().map(|s| match s {
                    super::super::TrajectorySource::Entity => "Entity",
                    super::super::TrajectorySource::Platform => "Platform",
                    super::super::TrajectorySource::Authz => "Authz",
                }),
                spec_governed: entry.spec_governed,
                created_at: &entry.timestamp,
            })
            .await
            .map_err(|e| {
                format!(
                    "failed to persist trajectory entry for {}/{}/{} action {}: {e}",
                    entry.tenant, entry.entity_type, entry.entity_id, entry.action
                )
            })?;
        Ok(())
    }

    /// Persist a pending decision to the storage backend (Turso only for now).
    pub async fn persist_pending_decision(
        &self,
        decision: &super::super::PendingDecision,
    ) -> Result<(), String> {
        let Some(store) = self.event_store.as_ref() else {
            return Ok(());
        };

        if let Some(turso) = store.platform_turso_store() {
            let status_str = match decision.status {
                super::super::DecisionStatus::Pending => "pending",
                super::super::DecisionStatus::Approved => "approved",
                super::super::DecisionStatus::Denied => "denied",
                super::super::DecisionStatus::Expired => "expired",
            };
            let data_json = serde_json::to_string(decision)
                .map_err(|e| format!("failed to serialize decision {}: {e}", decision.id))?;
            turso
                .upsert_pending_decision(&decision.id, &decision.tenant, status_str, &data_json)
                .await
                .map_err(|e| {
                    format!(
                        "failed to persist pending decision {} in turso: {e}",
                        decision.id
                    )
                })?;
        }

        Ok(())
    }

    /// Upsert an encrypted secret in the persistence backend.
    pub async fn upsert_secret(
        &self,
        tenant: &str,
        key_name: &str,
        ciphertext: &[u8],
        nonce: &[u8],
    ) -> Result<(), String> {
        let Some(backend) = self.metadata_backend() else {
            return Ok(());
        };

        match backend {
            MetadataBackend::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO tenant_secrets (tenant, key_name, ciphertext, nonce, created_at, updated_at) \
                     VALUES ($1, $2, $3, $4, now(), now()) \
                     ON CONFLICT (tenant, key_name) DO UPDATE SET \
                         ciphertext = EXCLUDED.ciphertext, \
                         nonce = EXCLUDED.nonce, \
                         updated_at = now()",
                )
                .bind(tenant)
                .bind(key_name)
                .bind(ciphertext)
                .bind(nonce)
                .execute(pool)
                .await
                .map_err(|e| format!("failed to upsert secret {tenant}/{key_name}: {e}"))?;
                Ok(())
            }
            MetadataBackend::Turso(turso) => {
                turso
                    .upsert_secret(tenant, key_name, ciphertext, nonce)
                    .await
                    .map_err(|e| format!("failed to upsert secret {tenant}/{key_name}: {e}"))?;
                Ok(())
            }
            MetadataBackend::Redis => Err(Self::redis_ephemeral_error("Secret persistence")),
        }
    }

    /// Delete a secret from the persistence backend.
    pub async fn delete_secret(&self, tenant: &str, key_name: &str) -> Result<bool, String> {
        let Some(backend) = self.metadata_backend() else {
            return Ok(false);
        };

        match backend {
            MetadataBackend::Postgres(pool) => {
                let result =
                    sqlx::query("DELETE FROM tenant_secrets WHERE tenant = $1 AND key_name = $2")
                        .bind(tenant)
                        .bind(key_name)
                        .execute(pool)
                        .await
                        .map_err(|e| format!("failed to delete secret {tenant}/{key_name}: {e}"))?;
                Ok(result.rows_affected() > 0)
            }
            MetadataBackend::Turso(turso) => turso
                .delete_secret(tenant, key_name)
                .await
                .map_err(|e| format!("failed to delete secret {tenant}/{key_name}: {e}")),
            MetadataBackend::Redis => Err(Self::redis_ephemeral_error("Secret deletion")),
        }
    }

    /// Load all secrets for a tenant from persistence, decrypt, and cache.
    pub async fn load_tenant_secrets(&self, tenant: &str) -> Result<usize, String> {
        let Some(vault) = self.secrets_vault.as_ref() else {
            return Ok(0);
        };
        let Some(backend) = self.metadata_backend() else {
            return Ok(0);
        };

        let rows: Vec<(String, Vec<u8>, Vec<u8>)> = match backend {
            MetadataBackend::Postgres(pool) => sqlx::query_as(
                "SELECT key_name, ciphertext, nonce FROM tenant_secrets WHERE tenant = $1",
            )
            .bind(tenant)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("failed to load secrets for tenant {tenant}: {e}"))?,
            MetadataBackend::Turso(turso) => turso
                .load_tenant_secrets(tenant)
                .await
                .map_err(|e| format!("failed to load secrets for tenant {tenant}: {e}"))?,
            MetadataBackend::Redis => return Err(Self::redis_ephemeral_error("Secret loading")),
        };

        let mut count = 0;
        for (key_name, ciphertext, nonce) in &rows {
            match vault.decrypt(ciphertext, nonce) {
                Ok(plaintext) => {
                    let value = String::from_utf8(plaintext)
                        .map_err(|e| format!("secret {key_name} is not valid UTF-8: {e}"))?;
                    vault.cache_secret(tenant, key_name, value)?;
                    count += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        tenant,
                        key_name,
                        error = %e,
                        "failed to decrypt secret, skipping"
                    );
                }
            }
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use temper_runtime::ActorSystem;
    use temper_store_turso::TursoEventStore;

    use crate::event_store::ServerEventStore;
    use crate::registry::SpecRegistry;
    use crate::secrets::vault::SecretsVault;
    use crate::state::ServerState;

    fn make_state() -> ServerState {
        let system = ActorSystem::new("test-secrets-persistence");
        ServerState::from_registry(system, SpecRegistry::new())
            .with_secrets_vault(SecretsVault::new(&[7u8; 32]))
    }

    #[tokio::test]
    async fn turso_secret_round_trip() {
        let db_path =
            std::env::temp_dir().join(format!("temper-secrets-{}.db", uuid::Uuid::new_v4())); // determinism-ok: test-only temp file
        let db_url = format!("file:{}", db_path.display());
        let store = TursoEventStore::new(&db_url, None)
            .await
            .expect("create local turso db");

        let mut state = make_state();
        state.event_store = Some(Arc::new(ServerEventStore::Turso(store)));

        let vault = state.secrets_vault.as_ref().expect("vault configured");
        let (ciphertext, nonce) = vault.encrypt(b"secret-value").expect("encrypt");

        // Upsert should succeed.
        state
            .upsert_secret("tenant-a", "API_KEY", &ciphertext, &nonce)
            .await
            .expect("turso secret upsert should succeed");

        // Load should decrypt and cache.
        let count = state
            .load_tenant_secrets("tenant-a")
            .await
            .expect("turso secret load should succeed");
        assert_eq!(count, 1);

        // Verify the cached value.
        let cached = vault.get_secret("tenant-a", "API_KEY");
        assert_eq!(cached.as_deref(), Some("secret-value"));

        // Delete should succeed.
        let deleted = state
            .delete_secret("tenant-a", "API_KEY")
            .await
            .expect("turso secret delete should succeed");
        assert!(deleted, "should have deleted one row");

        // Delete again returns false.
        let deleted_again = state
            .delete_secret("tenant-a", "API_KEY")
            .await
            .expect("turso secret delete should succeed");
        assert!(!deleted_again, "no row to delete");

        let _ = std::fs::remove_file(db_path); // determinism-ok: test-only cleanup
    }
}
