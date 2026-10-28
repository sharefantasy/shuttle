#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};

    fn make_executable(path: &std::path::Path, body: &str) {
        fs::write(path, body).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn busy_event_stream_cannot_starve_managed_rss_guard() {
        let temp = tempfile::tempdir().unwrap();
        let fake_pi = temp.path().join("busy-pi.sh");
        let fake_ps = temp.path().join("sample-ps.sh");
        let sample_count = temp.path().join("ps-count");

        make_executable(
            &fake_pi,
            r#"#!/bin/sh
i=0
while [ "$i" -lt 1000 ]; do
  printf '%s\n' '{"type":"message_update","usage":{"totalTokens":1}}'
  i=$((i + 1))
  sleep 0.002
done
"#,
        );
        make_executable(
            &fake_ps,
            r#"#!/bin/sh
n=0
[ -f "$SHUTTLE_TEST_PS_COUNT" ] && n=$(sed -n '1p' "$SHUTTLE_TEST_PS_COUNT")
n=$((n + 1))
printf '%s\n' "$n" > "$SHUTTLE_TEST_PS_COUNT"
if [ "$n" -eq 1 ]; then rss=1; else rss=1048576; fi
# This helper's parent is the shuttle supervisor, so PPID is the exact PID that the
# production sampler must include in addition to the isolated child PGID.
printf '%s %s %s\n' "$PPID" 99999 "$rss"
"#,
        );

        let output = Command::new(env!("CARGO_BIN_EXE_shuttle"))
            .args([
                "run",
                "busy stream",
                "--root",
                temp.path().to_str().unwrap(),
            ])
            .env("HOME", temp.path())
            .env("PI_BIN", &fake_pi)
            .env("SHUTTLE_PS_BIN", &fake_ps)
            .env("SHUTTLE_TEST_PS_COUNT", &sample_count)
            .env("SHUTTLE_RESOURCE_SAMPLE_MS", "5")
            .env("SHUTTLE_MAX_PROC_RSS_BYTES", "1024")
            .env("SHUTTLE_SOFT_TIMEOUT_MS", "5000")
            .env("SHUTTLE_STALL_TIMEOUT_MS", "5000")
            .output()
            .expect("spawn shuttle");

        assert_eq!(output.status.code(), Some(122));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("\"termination\":\"resource_limit\""),
            "{stderr}"
        );
        assert!(
            stderr.contains("\"limit_reason\":\"resource_proc_rss\""),
            "{stderr}"
        );
        let samples: u64 = fs::read_to_string(sample_count)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            samples >= 2,
            "guard must resample a busy stream, got {samples}"
        );
    }

    #[test]
    fn termination_signal_reaps_the_supervised_pi_process_group() {
        let temp = tempfile::tempdir().unwrap();
        let fake_pi = temp.path().join("signal-pi.sh");
        let child_pid = temp.path().join("child-pid");
        make_executable(
            &fake_pi,
            r#"#!/bin/sh
printf '%s\n' "$$" > "$SHUTTLE_TEST_CHILD_PID"
while :; do sleep 1; done
"#,
        );

        let shuttle = Command::new(env!("CARGO_BIN_EXE_shuttle"))
            .args([
                "run",
                "signal cleanup",
                "--root",
                temp.path().to_str().unwrap(),
            ])
            .env("HOME", temp.path())
            .env("PI_BIN", &fake_pi)
            .env("SHUTTLE_TEST_CHILD_PID", &child_pid)
            .env("SHUTTLE_RESOURCE_SAMPLE_MS", "50")
            .env("SHUTTLE_SOFT_TIMEOUT_MS", "10000")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn shuttle");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !child_pid.is_file() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(child_pid.is_file(), "pi did not publish its pid");
        let pi_pid: i32 = fs::read_to_string(&child_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        unsafe {
            libc::kill(shuttle.id() as i32, libc::SIGTERM);
        }
        let output = shuttle.wait_with_output().expect("wait for shuttle");

        assert_eq!(output.status.code(), Some(130));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("\"termination\":\"interrupted\""),
            "{stderr}"
        );
        let gone = unsafe { libc::kill(pi_pid, 0) } == -1;
        assert!(gone, "pi child {pi_pid} survived shuttle termination");
    }
}
