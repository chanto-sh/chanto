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

fn proc(pid: u32, parent: Option<u32>, mb: u64) -> crate::procs::Proc {
    crate::procs::Proc {
        pid,
        parent,
        memory: mb * 1_048_576,
        name: format!("p{pid}"),
    }
}

#[test]
fn the_watchdog_judges_only_chantos_descendants() {
    const MB: u64 = 1_048_576;
    let me = 100;
    // me → 101 → 102 (a grandchild), me → 103; 200 and 201 aren't ours.
    let table = || {
        vec![
            proc(100, Some(1), 50),
            proc(101, Some(100), 300),
            proc(102, Some(101), 900),
            proc(103, Some(100), 100),
            proc(200, Some(1), 5000),
            proc(201, Some(200), 4000),
        ]
    };
    let mem = |p: &crate::procs::Proc| p.memory;
    // 1300 MB of a 1600 MB machine is over a 70% limit: the biggest
    // descendant goes, not the bigger process that isn't Chanto's.
    let v = judge(me, table(), 1600 * MB, 70, false, mem);
    assert_eq!(v.used, 1300 * MB);
    assert_eq!(v.kids, 3);
    assert_eq!(v.victim, Some((102, 900 * MB, "p102".into())));
    // Under the limit: nothing.
    let v = judge(me, table(), 16000 * MB, 70, false, mem);
    assert_eq!(v.victim, None);
    // Nearly out of memory: over 512 MB is enough.
    let v = judge(me, table(), 16000 * MB, 70, true, mem);
    assert_eq!(v.victim.map(|(pid, _, _)| pid), Some(102));
    // But not when the descendants use little.
    let small = vec![proc(100, Some(1), 50), proc(101, Some(100), 100)];
    assert_eq!(judge(me, small, 16000 * MB, 70, true, mem).victim, None);
    // A cycle in the table (pids reused mid-scan) doesn't hang the look.
    let cycle = vec![proc(300, Some(301), 10), proc(301, Some(300), 10)];
    let v = judge(me, cycle, 1600 * MB, 70, false, mem);
    assert_eq!((v.kids, v.victim), (0, None));
}
