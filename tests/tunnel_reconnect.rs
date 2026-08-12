// Live check of tunnel auto-reconnect: after the pane is killed out from under us, the client
// notices the connection dropped, and reconnect() re-attaches and round-trips bytes again on a
// fresh pane. Requires the same patched harness daemon as tunnel_live.rs (serves /api/pane-ws on
// HARNESS_PROBE_PORT, default 18500). Skips when absent so `cargo test` stays green unprovisioned.
use std::sync::Arc;
use std::time::Duration;

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use harness_terminal::session::{Listener, TermSize};
use harness_terminal::transport::{Transport, TunnelTransport};

#[test]
fn tunnel_reconnects_after_pane_is_killed() {
    let port: u16 = std::env::var("HARNESS_PROBE_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(18500);
    if !daemon_up(port) {
        eprintln!("skipping: no harness pane-relay daemon on :{port}");
        return;
    }

    // A distinct program from tunnel_live's ("\\$SHELL") so the two live tests (which run
    // concurrently against the same daemon) each own a unique pane/session and don't kill each other.
    // bash echoes input, which is all this test needs.
    let program = "bash";
    let sname = format!("auton-{}", program.replace('/', "-"));
    let size = TermSize {
        lines: 20,
        cols: 60,
    };
    let term: Arc<FairMutex<Term<Listener>>> = Arc::new(FairMutex::new(Term::new(
        Config::default(),
        &size,
        Listener::default(),
    )));
    let echo = harness_terminal::session::EchoCanceller::default();
    let mut tx = TunnelTransport::spawn(
        "127.0.0.1",
        port,
        program,
        size,
        Arc::clone(&term),
        Arc::new(echo),
    )
    .expect("tunnel spawn against live daemon");

    // 1. First pane round-trips a marker (baseline — the pane echoes what we type).
    tx.write(b"echo RECONNECT_M1\r");
    assert!(
        grid_has(&term, "RECONNECT_M1"),
        "baseline marker did not land before kill"
    );

    // 2. Kill the pane out from under the client. tmux emits %exit, the daemon relay tears down,
    //    and the client's connection thread flips alive() to false.
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &sname])
        .status();
    wait_for_dead(&tx);

    // 3. Reconnect re-attaches a fresh pane with the same identity.
    tx.reconnect().expect("reconnect should re-attach");
    tx.write(b"echo RECONNECT_M2\r");
    assert!(
        grid_has(&term, "RECONNECT_M2"),
        "post-reconnect marker did not round-trip"
    );

    // Cleanup.
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &sname])
        .status();
}

/// Wait (bounded) until the transport reports itself dead.
fn wait_for_dead(tx: &dyn Transport) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if !tx.alive() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("transport never reported itself dead after pane kill");
}

fn grid_has(term: &Arc<FairMutex<Term<Listener>>>, marker: &str) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        {
            let g = term.lock();
            use alacritty_terminal::grid::Dimensions;
            let lines = g.screen_lines();
            let cols = g.columns();
            let mut buf = String::new();
            for r in 0..lines {
                use alacritty_terminal::index::Column;
                for c in 0..cols {
                    buf.push(g.grid()[alacritty_terminal::index::Line(r as i32)][Column(c)].c);
                }
            }
            if buf.contains(marker) {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn daemon_up(port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
}
