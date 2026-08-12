// Live end-to-end check of the harness pane-relay tunnel. Requires a patched `harness` daemon
// (one that serves /api/pane-ws) on HARNESS_PROBE_PORT, which defaults to 18500. Skips cleanly when
// the daemon is absent so `cargo test` stays green on machines without a dev daemon.
use std::sync::Arc;
use std::time::Duration;

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use autonomous_term::session::{Listener, TermSize};
use autonomous_term::transport::{Transport, TunnelTransport};

#[test]
fn tunnel_relays_pane_bytes_into_grid() {
    let port: u16 = std::env::var("HARNESS_PROBE_PORT")
        .ok().and_then(|v| v.parse().ok())
        .unwrap_or(18500);
    // Refuse to run if the daemon isn't up, so this never fails CI on unprovisioned machines.
    if !daemon_up(port) {
        eprintln!("skipping: no harness pane-relay daemon on :{port}");
        return;
    }

    let size = TermSize { lines: 20, cols: 60 };
    let term: Arc<FairMutex<Term<Listener>>> = Arc::new(FairMutex::new(Term::new(Config::default(), &size, Listener)));
    let tx = TunnelTransport::spawn("127.0.0.1", port, "\\$SHELL", size, Arc::clone(&term))
        .expect("tunnel spawn against live daemon");

    // Type a command that echoes a unique marker, then Enter.
    let marker = "TUNNEL_E2E_MARKER";
    tx.write(format!("echo {marker}\r").as_bytes());
    let _ = tx;

    // Poll the grid until the marker shows up (the pane echoes what we type + the shell echoes output).
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
                drop(g);
                assert!(buf.contains(marker), "marker should be visible");
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Cleanup: kill the pane we spawned.
    let _ = std::process::Command::new("tmux").args(["kill-session", "-t", "auton-\\$SHELL"]).status();
    panic!("marker never appeared in the grid — tunnel did not relay pane bytes");
}

fn daemon_up(port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", port))
        .and_then(|s| { let _ = s; Ok(()) })
        .is_ok()
}
