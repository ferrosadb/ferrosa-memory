use chrono::Utc;
use ferrosa_memory_core::control_store::{
    CommandInsert, ControlCommand, ControlCommandState, ControlCommandUpdate, ControlEventDraft,
    ControlStore, InMemoryControlStore,
};
use ferrosa_memory_core::types::TenantContext;
use serde_json::json;
use uuid::Uuid;

fn tenant() -> TenantContext {
    TenantContext {
        tenant_id: Uuid::new_v4(),
        session_origin: "mobile-control-test".to_owned(),
    }
}

#[tokio::test]
async fn mobile_control_cursor_blocks_never_reuse_and_replay_is_strictly_after() {
    let store = InMemoryControlStore::default();
    let ctx = tenant();
    let server = "server-fingerprint";

    let first = store
        .reserve_cursor_block(&ctx, server, 4)
        .await
        .expect("first block");
    let second = store
        .reserve_cursor_block(&ctx, server, 4)
        .await
        .expect("second block");
    assert_eq!((first.start, first.end), (1, 4));
    assert_eq!((second.start, second.end), (5, 8));

    for cursor in [first.start, first.end, second.end] {
        store
            .append_event(
                &ctx,
                server,
                ControlEventDraft {
                    cursor,
                    event_id: Uuid::now_v7(),
                    command_id: None,
                    kind: "heartbeat".to_owned(),
                    payload: json!({"cursor": cursor}),
                    created_at: Utc::now(),
                },
            )
            .await
            .expect("append event");
    }

    let page = store
        .events_after(&ctx, server, Some(first.start), 256)
        .await
        .expect("replay page");
    assert_eq!(page.high_water_cursor, second.end);
    assert_eq!(
        page.events
            .iter()
            .map(|event| event.cursor)
            .collect::<Vec<_>>(),
        vec![first.end, second.end]
    );
}

#[tokio::test]
async fn mobile_control_command_id_is_an_idempotency_key() {
    let store = InMemoryControlStore::default();
    let ctx = tenant();
    let server = "server-fingerprint";
    let command = ControlCommand {
        command_id: Uuid::now_v7(),
        command_type: "agent_instruct".to_owned(),
        request: json!({"instruction": "run tests"}),
        state: ControlCommandState::Queued,
        result: None,
        result_cursor: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };

    let first = store
        .put_command_if_absent(&ctx, server, &command)
        .await
        .expect("first insert");
    let duplicate = store
        .put_command_if_absent(&ctx, server, &command)
        .await
        .expect("duplicate insert");

    assert!(matches!(first, CommandInsert::Inserted(_)));
    assert!(matches!(duplicate, CommandInsert::Duplicate(_)));
}

#[tokio::test]
async fn command_state_transitions_are_validated_and_terminal_retries_are_idempotent() {
    let store = InMemoryControlStore::default();
    let ctx = tenant();
    let server = "server-fingerprint";
    let command = ControlCommand {
        command_id: Uuid::now_v7(),
        command_type: "agent_launch".to_owned(),
        request: json!({"instruction": "report proof of life"}),
        state: ControlCommandState::Queued,
        result: None,
        result_cursor: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    store
        .put_command_if_absent(&ctx, server, &command)
        .await
        .expect("insert command");

    let running = store
        .update_command(
            &ctx,
            server,
            command.command_id,
            ControlCommandUpdate {
                state: ControlCommandState::Running,
                result: None,
                result_cursor: None,
                updated_at: Utc::now(),
            },
        )
        .await
        .expect("queued to running");
    assert_eq!(running.state, ControlCommandState::Running);

    let completed_at = Utc::now();
    let completed = ControlCommandUpdate {
        state: ControlCommandState::Succeeded,
        result: Some(json!({"message": "alive", "thread_id": command.command_id})),
        result_cursor: Some(17),
        updated_at: completed_at,
    };
    let first = store
        .update_command(&ctx, server, command.command_id, completed.clone())
        .await
        .expect("running to succeeded");
    let retry = store
        .update_command(&ctx, server, command.command_id, completed)
        .await
        .expect("exact terminal retry");
    assert_eq!(first, retry);

    let illegal = store
        .update_command(
            &ctx,
            server,
            command.command_id,
            ControlCommandUpdate {
                state: ControlCommandState::Failed,
                result: Some(json!({"error": "rewrite"})),
                result_cursor: Some(18),
                updated_at: Utc::now(),
            },
        )
        .await;
    assert!(illegal.is_err(), "terminal state must not be rewritten");
}

#[tokio::test]
async fn replay_high_water_never_skips_an_undelivered_bounded_event() {
    let store = InMemoryControlStore::default();
    let ctx = tenant();
    let server = "server-fingerprint";
    let block = store
        .reserve_cursor_block(&ctx, server, 300)
        .await
        .expect("cursor block");
    for cursor in block.start..=257 {
        store
            .append_event(
                &ctx,
                server,
                ControlEventDraft {
                    cursor,
                    event_id: Uuid::now_v7(),
                    command_id: None,
                    kind: "heartbeat".to_owned(),
                    payload: json!({}),
                    created_at: Utc::now(),
                },
            )
            .await
            .expect("append event");
    }

    let first = store
        .events_after(&ctx, server, None, 256)
        .await
        .expect("first page");
    assert_eq!(first.events.len(), 256);
    assert_eq!(first.high_water_cursor, 256);

    let second = store
        .events_after(&ctx, server, Some(first.high_water_cursor), 256)
        .await
        .expect("second page");
    assert_eq!(second.events[0].cursor, 257);
    assert_eq!(second.high_water_cursor, block.end);
}

#[test]
fn migration_51_registers_only_additive_mobile_control_tables() {
    let migration = ferrosa_memory_core::migration::MIGRATIONS
        .iter()
        .find(|migration| migration.version == 51)
        .expect("migration 51 must be registered");
    let ddl = migration.ddl.to_ascii_lowercase();
    for table in [
        "mobile_control_cursor_state",
        "mobile_control_events",
        "mobile_control_commands",
    ] {
        assert!(ddl.contains(table), "migration 51 must create {table}");
    }
    assert!(!ddl.contains("drop "), "migration must be additive");

    let reservation = ferrosa_memory_core::migration::MIGRATIONS
        .iter()
        .find(|migration| migration.version == 52)
        .expect("migration 52 must be registered");
    let reservation_ddl = reservation.ddl.to_ascii_lowercase();
    assert!(reservation_ddl.contains("add reservation_token uuid"));
    assert!(
        !reservation_ddl.contains("drop "),
        "migration must be additive"
    );
}
