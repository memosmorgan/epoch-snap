use std::process::Command;

#[path = "support/seccomp.rs"]
mod seccomp;
use seccomp::deny_userfaultfd;

#[test]
fn doctor_exits_nonzero_when_userfaultfd_is_denied() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_epochsnap"));
    command.arg("doctor");
    deny_userfaultfd(&mut command);
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("userfaultfd(UFFD_USER_MODE_ONLY)"),
        "{stderr}"
    );
    assert!(stderr.contains("errno=1"), "{stderr}");
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains("available")
    );
}

#[test]
fn denied_wp_capture_does_not_fallback_or_poison() {
    const CHILD: &str = "EPOCHSNAP_DENIED_ARENA_CHILD";
    if std::env::var_os(CHILD).is_some() {
        use epochsnap::{Arena, CaptureMode, Error};
        let mut arena = Arena::new(1).unwrap();
        arena.store_word(0, 77).unwrap();
        for _ in 0..2 {
            assert_eq!(
                arena.checkpoint(CaptureMode::WriteProtected),
                Err(Error::System {
                    operation: "userfaultfd(UFFD_USER_MODE_ONLY)",
                    errno: libc::EPERM,
                })
            );
            assert_eq!(arena.load_word(0), Ok(77));
        }
        let epoch = arena.checkpoint(CaptureMode::Stopped).unwrap();
        assert_eq!(arena.checkpoint_words(epoch).unwrap()[0], 77);
        return;
    }
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "denied_wp_capture_does_not_fallback_or_poison",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    deny_userfaultfd(&mut command);
    let mut child = command.spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "denied arena capture: external deadline, child killed/reaped: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        // Parent watchdog only; no protocol assertion depends on this delay.
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
