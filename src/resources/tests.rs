use super::*;

#[tokio::test]
async fn memory_queries_work() {
    assert!(total_mb() > 0);
    assert!(available_mb() > 0);
    let _a = agent_slot().await;
    let _c = command_slot().await;
    assert!(
        wait_for_memory("test", Duration::from_millis(1))
            .await
            .is_ok()
            || available_mb() < limits().min_free
    );
}
