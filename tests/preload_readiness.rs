#![cfg(all(unix, feature = "line"))]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    condition()
}

fn health_is_ready(addr: SocketAddr) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(100)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    if stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }

    let mut response = String::new();
    stream.read_to_string(&mut response).is_ok()
        && response.starts_with("HTTP/1.1 200")
        && response.ends_with("ok")
}

fn toml_string(path: &Path) -> String {
    format!("{:?}", path.display().to_string())
}

#[test]
fn health_is_not_published_until_persisted_sessions_are_preloaded() {
    let temp = tempfile::tempdir().unwrap();
    let openab_dir = temp.path().join(".openab");
    std::fs::create_dir_all(&openab_dir).unwrap();
    std::fs::write(
        openab_dir.join("thread_map.json"),
        r#"{"line:test-user":"saved-session"}"#,
    )
    .unwrap();

    let release = temp.path().join("release-load");
    let load_started = temp.path().join("load-started");
    let agent = temp.path().join("blocking-acp-agent.sh");
    std::fs::write(
        &agent,
        r#"#!/bin/sh
while IFS= read -r request; do
  case "$request" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"agentInfo":{"name":"fixture"},"agentCapabilities":{"loadSession":true}}}'
      ;;
    *'"method":"session/load"'*)
      : > "$2"
      while [ ! -f "$1" ]; do sleep 0.05; done
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"saved-session"}}'
      ;;
    *)
      exit 64
      ;;
  esac
done
"#,
    )
    .unwrap();

    let config = temp.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"[agent]
command = "/bin/sh"
args = [{agent}, {release}, {load_started}]
working_dir = {working_dir}

[pool]
max_sessions = 1
preload_persisted_sessions = true
"#,
            agent = toml_string(&agent),
            release = toml_string(&release),
            load_started = toml_string(&load_started),
            working_dir = toml_string(temp.path()),
        ),
    )
    .unwrap();

    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let listen_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_openab"))
        .args(["run", "--config", config.to_str().unwrap()])
        .env("HOME", temp.path())
        .env("GATEWAY_LISTEN", listen_addr.to_string())
        .env("LINE_CHANNEL_SECRET", "readiness-test-secret")
        .env("LINE_CHANNEL_ACCESS_TOKEN", "readiness-test-token")
        .env("LINE_ALLOW_ALL_USERS", "true")
        .env_remove("TELEGRAM_BOT_TOKEN")
        .env_remove("FEISHU_APP_ID")
        .env_remove("WECOM_CORP_ID")
        .env_remove("TEAMS_APP_ID")
        .env_remove("OPENAB_ACP_ENABLED")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _child = ChildGuard(child);

    assert!(
        wait_until(Duration::from_secs(5), || load_started.exists()),
        "startup never reached the persisted session/load gate"
    );
    assert!(
        !health_is_ready(listen_addr),
        "/health was published while persisted session preload was blocked"
    );

    std::fs::write(release, "release").unwrap();
    assert!(
        wait_until(Duration::from_secs(10), || health_is_ready(listen_addr)),
        "/health was not published after persisted session preload completed"
    );
}
