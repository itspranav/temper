use std::sync::Arc;

use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_store_turso::TursoEventStore;

use temper_server::event_store::ServerEventStore;
use temper_server::registry::SpecRegistry;
use temper_server::state::ServerState;
use temper_spec::csdl::parse_csdl;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

fn build_state_with_turso(system_name: &str, store: TursoEventStore) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "tenant-a",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );

    let mut state = ServerState::from_registry(ActorSystem::new(system_name), registry);
    state.event_store = Some(Arc::new(ServerEventStore::Turso(store)));
    state
}

#[tokio::test]
async fn ensure_entity_loaded_returns_false_when_no_transition_table_exists() {
    let db_path =
        std::env::temp_dir().join(format!("temper-ensure-loaded-{}.db", uuid::Uuid::new_v4()));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let pid = "tenant-a:Order:ord-1";
    let envelope = PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Created".to_string(),
        payload: serde_json::json!({"id": "ord-1"}),
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: sim_now(),
            actor_id: pid.to_string(),
        },
    };
    store
        .append(pid, 0, &[envelope])
        .await
        .expect("append seed event");

    let mut state =
        ServerState::from_registry(ActorSystem::new("test-ensure-loaded"), SpecRegistry::new());
    state.event_store = Some(Arc::new(ServerEventStore::Turso(store)));

    let loaded = state
        .ensure_entity_loaded(&TenantId::new("tenant-a"), "Order", "ord-1")
        .await;
    assert!(
        !loaded,
        "entity should not be considered loaded when transition table is missing"
    );

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn ensure_entity_loaded_returns_true_for_indexed_entity_without_persistence() {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "tenant-a",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    let state = ServerState::from_registry(ActorSystem::new("test-ensure-loaded-inmem"), registry);

    let tenant = TenantId::new("tenant-a");
    let entity_type = "Order";
    let entity_id = "ord-memory";

    state
        .get_or_create_tenant_entity(
            &tenant,
            entity_type,
            entity_id,
            serde_json::json!({"Title": "in-memory"}),
        )
        .await
        .expect("create in-memory entity");

    let loaded = state
        .ensure_entity_loaded(&tenant, entity_type, entity_id)
        .await;
    assert!(
        loaded,
        "indexed in-memory entity should be considered loaded"
    );
}

#[tokio::test]
async fn delete_writes_tombstone_and_deleted_entity_stays_out_of_list_after_restart() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-delete-tombstone-list-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let tenant = TenantId::new("tenant-a");
    let entity_type = "Order";
    let entity_id = "ord-delete-list";
    let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");

    let state = build_state_with_turso("test-delete-tombstone-list-1", store.clone());
    state
        .get_or_create_tenant_entity(
            &tenant,
            entity_type,
            entity_id,
            serde_json::json!({"Title": "to-delete"}),
        )
        .await
        .expect("create entity");
    state
        .delete_tenant_entity(&tenant, entity_type, entity_id)
        .await
        .expect("delete entity");

    let events = store
        .read_events(&persistence_id, 0)
        .await
        .expect("read event journal");
    let last = events.last().expect("tombstone event exists");
    assert_eq!(last.event_type, "Deleted");
    assert_eq!(
        last.payload
            .get("action")
            .and_then(serde_json::Value::as_str),
        Some("Deleted")
    );
    assert_eq!(
        last.payload
            .get("to_status")
            .and_then(serde_json::Value::as_str),
        Some("Deleted")
    );

    let state_after_restart = build_state_with_turso("test-delete-tombstone-list-2", store);
    state_after_restart.populate_index_from_store(&tenant).await;
    let ids = state_after_restart
        .list_entity_ids_lazy(&tenant, entity_type)
        .await;
    assert!(
        !ids.iter().any(|id| id == entity_id),
        "deleted entity should not be listed after restart/index rebuild"
    );

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn ensure_entity_loaded_returns_false_for_deleted_entity() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-delete-tombstone-ensure-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let tenant = TenantId::new("tenant-a");
    let entity_type = "Order";
    let entity_id = "ord-delete-ensure";

    let state = build_state_with_turso("test-delete-tombstone-ensure-1", store.clone());
    state
        .get_or_create_tenant_entity(
            &tenant,
            entity_type,
            entity_id,
            serde_json::json!({"Title": "to-delete"}),
        )
        .await
        .expect("create entity");
    state
        .delete_tenant_entity(&tenant, entity_type, entity_id)
        .await
        .expect("delete entity");

    let state_after_restart = build_state_with_turso("test-delete-tombstone-ensure-2", store);
    let loaded = state_after_restart
        .ensure_entity_loaded(&tenant, entity_type, entity_id)
        .await;
    assert!(
        !loaded,
        "deleted entity should not be considered loadable from persistence"
    );
    assert!(
        !state_after_restart.entity_exists(&tenant, entity_type, entity_id),
        "deleted entity should not be indexed after ensure_entity_loaded"
    );

    let _ = std::fs::remove_file(db_path);
}
