//! Harness fleet awareness — the bridge from OUR session layer to the joined machine's `harness`
//! daemon, read-only and non-blocking.
//!
//! Every joined computer runs a local `harness` daemon on a fixed loopback control port
//! (`HARNESS_PORT_DEFAULT`, set by the harness `PORT` env). Its `GET /api/status` is the live,
//! authoritative view of that machine's agent fleet: the e2ee tunnel state (`connected`,
//! `deviceTransportConnected`), the stable `machineId` every pane traces to, and the registered
//! per-engine sessions (engine, pane, last-updated). That's the commander-bus data the client pulls
//! into status badges without rebuilding the parse — exactly the reuse ARCHITECTURE §6 calls for.
//!
//! This is deliberately a thin, best-effort reader: a missing/unjoined daemon degrades to
//! "unknown" instead of failing the terminal. Terminal bytes still flow through our own
//! `Transport`; the harness only feeds the *status chrome*.

use std::time::Duration;

use serde::Deserialize;

/// The harness daemon's fixed loopback control port (harness `config/env.ts` default).
pub const HARNESS_PORT_DEFAULT: u16 = 18473;
/// How long ago a session's last update may be before we call it idle.
const IDLE_AFTER_MS: u64 = 5 * 60 * 1000;

/// One registered agent session on this machine, as `/api/status` reports it.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct FleetSession {
    #[serde(default)]
    pub engine: String,
    #[serde(default, rename = "tmuxPane")]
    pub tmux_pane: String,
    #[serde(default, rename = "updatedAt")]
    pub updated_at: u64,
    #[serde(default)]
    pub name: String,
}

/// The whole-machine aggregate from `GET /api/status`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct FleetStatus {
    #[serde(default, rename = "machineId")]
    pub machine_id: String,
    #[serde(default)]
    pub connected: bool,
    #[serde(default)]
    pub device_transport_connected: bool,
    #[serde(default, rename = "sessions")]
    pub fleet: Vec<FleetSession>,
}

/// HTTP error or non-2xx — the reader swallows it and reports "unknown".
#[derive(Debug)]
pub struct HarnessUnreachable;

/// Best-effort reader of the local harness daemon's status.
pub struct HarnessClient {
    base: String,
}

impl HarnessClient {
    /// Point at a local (or, for a remote-attach loopback, any reachable) harness control port.
    pub fn on_port(port: u16) -> HarnessClient {
        HarnessClient { base: format!("http://127.0.0.1:{port}") }
    }

    /// On the default loopback port — the common case.
    pub fn local() -> HarnessClient {
        HarnessClient::on_port(HARNESS_PORT_DEFAULT)
    }

    /// Fetch `GET /api/status`. Returns None when the daemon is absent/unjoined or the JSON errs.
    pub fn status(&self) -> Result<FleetStatus, HarnessUnreachable> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(800))
            .build()
            .map_err(|_| HarnessUnreachable)?;
        let resp = client
            .get(format!("{}/api/status", self.base))
            .send()
            .map_err(|_| HarnessUnreachable)?;
        if !resp.status().is_success() {
            return Err(HarnessUnreachable);
        }
        resp.json().map_err(|_| HarnessUnreachable)
    }
}

impl FleetStatus {
    /// Cluster-wide busy/idle signal for a given engine on this machine: true when the harness
    /// reports a live (recently-updated) session for it. Unknown (daemon unreachable) is "false",
    /// i.e. no badge — screens never block on the harness.
    pub fn engine_is_live(&self, engine: &str) -> bool {
        let now_ms = now_unix_ms();
        self.fleet.iter().any(|s| {
            s.engine == engine && s.updated_at > 0 && now_ms.saturating_sub(s.updated_at) < IDLE_AFTER_MS
        })
    }

    /// Short one-real-word summary for the status line, e.g. "3 agents / tunnel up".
    pub fn summary(&self) -> String {
        let n = self.fleet.len();
        let tunnel = if self.connected { "tunnel up" } else { "tunnel down" };
        format!("{n} agent{} · {tunnel}", if n == 1 { "" } else { "s" })
    }
}

/// Non-test fakeable clock boundary (the real product uses wall clock; tests stub this).
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `engine_is_live` reads through the exact wire field (`updatedAt`, unix ms) the harness sends.
    #[test]
    fn parses_live_status_and_busy() {
        // updatedAt is unix ms measured NOW, so a "live" row is ~2s ago and "stale"/"never" are far back.
        let now = super::now_unix_ms();
        let raw = format!(r#"{{
            "machineId": "49130ea7541488a778132ed7476dbbc0",
            "connected": true,
            "deviceTransportConnected": true,
            "sessions": [
                {{ "id": "x", "sessionId": "y", "engine": "claude", "tmuxPane": "%7", "updatedAt": {} }},
                {{ "id": "z", "sessionId": "w", "engine": "codex", "tmuxPane": "%8", "updatedAt": 0 }}
            ]
        }}"#, now - 2000);
        let st: FleetStatus = serde_json::from_str(&raw).expect("wire JSON parses");
        assert_eq!(st.machine_id, "49130ea7541488a778132ed7476dbbc0");
        assert!(st.connected);
        assert_eq!(st.fleet.len(), 2);
        assert!(st.engine_is_live("claude"));
        // updatedAt 0 (never) must never read as live.
        assert!(!st.engine_is_live("codex"));
        assert!(!st.engine_is_live("nope"));
        assert_eq!(st.summary(), "2 agents · tunnel up");
    }
}

