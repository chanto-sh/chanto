#[test]
fn chanto_is_not_dumpable_and_still_reads_what_it_needs_from_proc() {
    super::hide_process_memory();
    // SAFETY: PR_GET_DUMPABLE takes no pointers.
    let zero: libc::c_ulong = 0;
    assert_eq!(
        unsafe { libc::prctl(libc::PR_GET_DUMPABLE, zero, zero, zero, zero) },
        0
    );
    // What Chanto reads about itself: process stats and its own path.
    assert!(std::fs::read("/proc/self/stat").is_ok());
    let pid = std::process::id();
    assert!(std::fs::read(format!("/proc/{pid}/status")).is_ok());
    assert!(std::env::current_exe().is_ok());
    // The owner-only files are root's now, which is the point.
    assert!(std::fs::read("/proc/self/environ").is_err());
}
