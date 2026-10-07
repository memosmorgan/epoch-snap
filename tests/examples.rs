use std::{
    fs,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Command, Stdio},
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

#[path = "support/seccomp.rs"]
mod seccomp;

fn example(name: &str) -> PathBuf {
    static BUILD: OnceLock<()> = OnceLock::new();
    BUILD.get_or_init(|| {
        let mut build = Command::new(env!("CARGO"));
        build.args(["build", "--examples"]);
        if !cfg!(debug_assertions) {
            build.arg("--release");
        }
        assert!(build.status().unwrap().success());
    });
    let tests = std::env::current_exe().unwrap();
    tests
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples")
        .join(name)
}

#[test]
#[ignore = "requires actual userfaultfd WP; failure is not a skip"]
fn wp_examples_replay_and_record_worker_costs() {
    let output = run("rewind", &["--mode", "wp", "--arena-mib", "1"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("full-state equality: true"));
    let output = run(
        "bench",
        &["--arena-mib", "1", "--workload", "sparse", "--trials", "1"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let csv = String::from_utf8(output.stdout).unwrap();
    let mut lines = csv.lines();
    let header: Vec<_> = lines.next().unwrap().split(',').collect();
    let rows: Vec<Vec<_>> = lines.map(|line| line.split(',').collect()).collect();
    assert_eq!(rows.len(), 3);
    let wp = rows.iter().find(|row| row[0] == "wp").unwrap();
    let field = |key| wp[header.iter().position(|&name| name == key).unwrap()];
    for key in [
        "worker_elapsed_ns",
        "worker_cpu_ns",
        "checkpoint_call_ns",
        "capture_complete_ns",
    ] {
        assert!(
            field(key).parse::<u64>().unwrap() > 0,
            "missing WP metric {key}"
        );
    }
    assert_eq!(field("copied_bytes"), "1048576");
    assert_eq!(
        field("fault_pages").parse::<u64>().unwrap() + field("scan_pages").parse::<u64>().unwrap(),
        256
    );
}

fn run(name: &str, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(example(name));
    command.args(args);
    run_command(command)
}

fn run_command(mut command: Command) -> std::process::Output {
    command.process_group(0);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            terminate_tree(&mut child).unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "example deadline: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn terminate_tree(child: &mut std::process::Child) -> std::io::Result<std::process::ExitStatus> {
    let group = i32::try_from(child.id()).expect("Linux PID fits i32");
    // SAFETY: every caller created this child with process_group(0), making its
    // PID the group ID. The unreaped child reserves that ID through this call.
    // A negative PID sends SIGKILL to the whole private group, including bench's
    // trial process and all worker threads; no parent/global group is targeted.
    if unsafe { libc::kill(-group, libc::SIGKILL) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    child.wait()
}

#[test]
fn external_deadline_terminates_the_entire_example_process_tree() {
    let path = std::env::temp_dir().join(format!(
        "epochsnap-descendant-m4-{}.pid",
        std::process::id()
    ));
    assert!(!path.exists());
    let mut command = Command::new("sh");
    command
        .args([
            "-c",
            "sleep 60 & printf '%s' \"$!\" > \"$1\"; wait",
            "fixture",
        ])
        .arg(&path)
        .process_group(0);
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !path.exists() {
        if Instant::now() >= deadline {
            terminate_tree(&mut child).unwrap();
            panic!("fixture did not acknowledge descendant creation");
        }
        thread::sleep(Duration::from_millis(5));
    }
    let descendant: u32 = loop {
        let pid = fs::read_to_string(&path).unwrap();
        if let Ok(pid) = pid.parse() {
            break pid;
        }
        assert!(Instant::now() < deadline);
        thread::yield_now();
    };
    terminate_tree(&mut child).unwrap();
    let status = std::path::PathBuf::from(format!("/proc/{descendant}/status"));
    loop {
        let alive = fs::read_to_string(&status).is_ok_and(|s| {
            !s.lines()
                .any(|line| line.starts_with("State:") && line.contains("Z (zombie)"))
        });
        if !alive {
            break;
        }
        if Instant::now() >= deadline {
            // Fixture cleanup even when the implementation kills only the parent.
            // SAFETY: this negative PID is the fixture's reserved process group.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            panic!("deadline left the descendant alive");
        }
        thread::yield_now();
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn examples_fail_usefully_when_wp_is_denied_without_substituting_stopped() {
    for (name, args) in [
        (
            "bench",
            vec!["--arena-mib", "1", "--trials", "1", "--modes", "wp"],
        ),
        ("rewind", vec!["--arena-mib", "1"]),
    ] {
        let mut command = Command::new(example(name));
        command.args(args);
        seccomp::deny_userfaultfd(&mut command);
        let output = run_command(command);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("userfaultfd(UFFD_USER_MODE_ONLY)"),
            "{stderr}"
        );
        assert!(stderr.contains("errno=1"), "{stderr}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("full-state equality: true"));
        assert!(!stdout.lines().any(|line| line.starts_with("wp,")));
    }
}

#[test]
fn benchmark_rejects_invalid_cli_before_creating_output() {
    let path =
        std::env::temp_dir().join(format!("epochsnap-invalid-m4-{}.csv", std::process::id()));
    assert!(!path.exists());
    for args in [
        vec!["--arena-mib", "0"],
        vec!["--arena-mib", "18446744073709551615"],
        vec!["--arena-mib", "-1"],
        vec!["--workload", "unknown"],
        vec!["--trials", "0"],
        vec!["--operations", "0"],
        vec!["--operations", "64", "--checkpoint-at", "64"],
        vec!["--modes", "none,unknown"],
        vec!["--seed"],
        vec!["--unknown", "1"],
    ] {
        let mut args = args;
        args.extend(["--output", path.to_str().unwrap()]);
        let output = run("bench", &args);
        assert!(!output.status.success(), "accepted {args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("bench:"));
        assert!(!path.exists(), "invalid CLI created an output file");
    }
}

#[test]
fn small_benchmark_records_comparable_trials_and_real_costs_without_uffd() {
    let path = std::env::temp_dir().join(format!("epochsnap-m4-{}.csv", std::process::id()));
    let output = run(
        "bench",
        &[
            "--arena-mib",
            "1",
            "--workload",
            "sequential",
            "--seed",
            "7",
            "--trials",
            "2",
            "--modes",
            "none,stopped",
            "--output",
            path.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let csv = fs::read_to_string(&path).unwrap();
    fs::remove_file(path).unwrap();
    let mut lines = csv.lines();
    let header: Vec<_> = lines.next().unwrap().split(',').collect();
    let rows: Vec<Vec<_>> = lines.map(|line| line.split(',').collect()).collect();
    assert_eq!(rows.len(), 4);
    for row in &rows {
        assert_eq!(row.len(), header.len());
        let field = |name| row[header.iter().position(|&key| key == name).unwrap()];
        for key in [
            "arena_init_ns",
            "workload_init_ns",
            "application_ns",
            "validation_ns",
            "arena_drop_ns",
            "trial_total_ns",
            "peak_rss_kib",
            "process_cpu_ns",
        ] {
            assert!(field(key).parse::<u64>().unwrap() > 0, "missing cost {key}");
        }
        assert_eq!(field("arena_bytes"), "1048576");
        assert_eq!(field("poll_batch_ops"), "64");
        assert_eq!(
            field("state_hash"),
            rows[0][header.iter().position(|&key| key == "state_hash").unwrap()]
        );
        assert_eq!(field("worker_cpu_ns"), "NA");
        if field("mode") == "none" {
            for key in [
                "checkpoint_call_ns",
                "capture_complete_ns",
                "restore_ns",
                "copied_bytes",
                "first_write_pending_pages",
            ] {
                assert_eq!(field(key), "NA", "fabricated no-capture value {key}");
            }
        } else {
            assert!(field("checkpoint_call_ns").parse::<u64>().unwrap() > 0);
            assert!(field("restore_ns").parse::<u64>().unwrap() > 0);
            assert_eq!(field("copied_bytes"), "1048576");
            assert_eq!(field("fault_pages"), "0");
            assert_eq!(field("scan_pages"), "256");
            assert_eq!(field("first_write_pending_pages"), "0");
            assert_eq!(field("first_write_p99_ns"), "NA");
        }
    }
}

#[test]
fn stopped_demo_rewinds_and_compares_every_slot() {
    let output = run("rewind", &["--mode", "stopped", "--arena-mib", "1"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("restored step=128"), "{stdout}");
    assert!(stdout.contains("full-state equality: true"), "{stdout}");
    assert!(stdout.contains("same-process, in-memory"), "{stdout}");
}
