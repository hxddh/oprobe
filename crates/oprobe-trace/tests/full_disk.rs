//! A save onto a disk without room for it loses nothing (its own test binary: it lowers the
//! process's file-size limit, which would fail the saves of tests running beside it).

/// 9.3.4 took the rows and reset the window, then failed while writing: `save_and_reset` on
/// a full disk returned the error with the period gone. Room is now reserved first.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_save_without_room_keeps_the_window() {
    use opendal_core::Operator;
    use opendal_core::services::Memory;

    let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
    let op = Operator::new(Memory::default()).unwrap().layer(layer);
    for i in 0..50 {
        op.write(&format!("k{i}"), vec![1u8; 64]).await.unwrap();
    }
    let path =
        std::env::temp_dir().join(format!("oprobe-trace-full-{}.oprobe", std::process::id()));
    // A disk with 4 KiB to spare: writing past the limit fails (EFBIG) instead of raising
    // SIGXFSZ, which is ignored.
    let set_limit = |bytes: libc::rlim_t| {
        let lim = libc::rlimit {
            rlim_cur: bytes,
            rlim_max: libc::RLIM_INFINITY,
        };
        // SAFETY: plain libc calls with valid arguments.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &lim) }, 0);
    };
    // SAFETY: ignoring a signal.
    unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) };
    set_limit(4096);
    let full = handle.save_and_reset(&path);
    let full_save = handle.save(&path);
    set_limit(libc::RLIM_INFINITY);
    assert!(full.is_err(), "the reservation fails");
    assert!(full_save.is_err());
    assert_eq!(handle.snapshot().summary.ops, 50, "the window was kept");
    handle.save_and_reset(&path).unwrap();
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(doc["snapshot"]["summary"]["ops_count"], 50);
    assert_eq!(
        doc["rows"]["ops"].as_array().unwrap().len(),
        50,
        "the rows were kept"
    );
    let _ = std::fs::remove_file(&path);
}
