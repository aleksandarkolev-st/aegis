use std::env;

use aegis_relay::repository::{
    InboundReceiptClaim, PgRepository, RelayRepository, RepositoryError,
};
use uuid::Uuid;

#[tokio::test]
async fn postgres_pairing_rotation_and_inbound_receipts_are_idempotent() {
    let Ok(database_url) = env::var("RELAY_TEST_DATABASE_URL") else {
        eprintln!("skipping PostgreSQL relay test: RELAY_TEST_DATABASE_URL is unset");
        return;
    };
    let repository = PgRepository::connect(&database_url).await.unwrap();
    let installation_id = Uuid::new_v4();
    let actor_id = format!("smoke-{}", Uuid::new_v4().simple());

    let first = repository
        .provision_installation(&actor_id, Some(installation_id))
        .await
        .unwrap();
    let second = repository
        .provision_installation(&actor_id, Some(installation_id))
        .await
        .unwrap();
    assert_eq!(first.user_id, second.user_id);
    assert_eq!(first.installation_id, installation_id);
    assert_eq!(second.installation_id, installation_id);
    assert_ne!(first.pairing_code, second.pairing_code);
    let second_actor = repository
        .provision_installation("second-actor", Some(installation_id))
        .await
        .unwrap();
    assert_eq!(second_actor.user_id, first.user_id);
    assert_eq!(second_actor.installation_id, installation_id);
    let colliding_installation_id = Uuid::new_v4();
    assert!(matches!(
        repository
            .provision_installation(&actor_id, Some(colliding_installation_id))
            .await,
        Err(RepositoryError::ActorCollision)
    ));
    let collision_left_installation =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM installations WHERE id = $1)")
            .bind(colliding_installation_id)
            .fetch_one(repository.pool())
            .await
            .unwrap();
    assert!(!collision_left_installation);
    drop(repository);
    let repository = PgRepository::connect(&database_url).await.unwrap();

    let suffix = Uuid::new_v4().as_u128() % 10_000_000_000;
    let sender_id = format!("+4915{suffix:010}");
    assert!(
        repository
            .redeem_pairing("whatsapp", &sender_id, &first.pairing_code)
            .await
            .unwrap()
            .is_none(),
        "rotated pairing code must no longer redeem"
    );
    let binding = repository
        .redeem_pairing("whatsapp", &sender_id, &second.pairing_code)
        .await
        .unwrap()
        .expect("current pairing code should redeem");
    assert_eq!(binding.installation_id, installation_id);
    assert_eq!(binding.actor_id, actor_id);
    let second_sender_id = format!("+4916{suffix:010}");
    let second_binding = repository
        .redeem_pairing("whatsapp", &second_sender_id, &second_actor.pairing_code)
        .await
        .unwrap()
        .expect("second actor's code should redeem under the same installation");
    assert_eq!(second_binding.user_id, first.user_id);
    assert_eq!(second_binding.actor_id, "second-actor");
    assert_eq!(
        repository
            .targets_for_installation(installation_id, &actor_id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        repository
            .targets_for_installation(installation_id, "second-actor")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        repository
            .targets_for_installation(installation_id, "unpaired-actor")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        repository
            .redeem_pairing("whatsapp", &sender_id, &second.pairing_code)
            .await
            .unwrap()
            .is_none(),
        "pairing code is one time"
    );

    let external_message_id = format!("smoke-{}", Uuid::new_v4());
    assert_eq!(
        repository
            .claim_inbound_receipt("whatsapp", &sender_id, &external_message_id)
            .await
            .unwrap(),
        InboundReceiptClaim::Claimed
    );
    assert_eq!(
        repository
            .claim_inbound_receipt("whatsapp", &sender_id, &external_message_id)
            .await
            .unwrap(),
        InboundReceiptClaim::InProgress
    );
    sqlx::query(
        "UPDATE delivery_receipts SET updated_at = now() - interval '31 seconds' WHERE direction = 'inbound' AND channel = 'whatsapp' AND sender_id = $1 AND external_message_id = $2",
    )
    .bind(&sender_id)
    .bind(&external_message_id)
    .execute(repository.pool())
    .await
    .unwrap();
    assert_eq!(
        repository
            .claim_inbound_receipt("whatsapp", &sender_id, &external_message_id)
            .await
            .unwrap(),
        InboundReceiptClaim::Claimed,
        "an expired processing lease can be reclaimed"
    );
    repository
        .complete_inbound_receipt(
            "whatsapp",
            &sender_id,
            &external_message_id,
            "published",
            Some(installation_id),
            Some(binding.id),
        )
        .await
        .unwrap();
    assert_eq!(
        repository
            .claim_inbound_receipt("whatsapp", &sender_id, &external_message_id)
            .await
            .unwrap(),
        InboundReceiptClaim::Duplicate
    );

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(first.user_id)
        .execute(repository.pool())
        .await
        .unwrap();
}
