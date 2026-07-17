//! Chain-sync follower — real confirmation depth and rollback handling.
//!
//! A background task holds a persistent [`OgmiosSession`] running the
//! chain-sync protocol (`findIntersection` → `nextBlock` loop) and maintains
//! shared state: the current tip height, and a registry of *watched*
//! transaction ids with where (if anywhere) each was seen on chain.
//!
//! Writers register a tx id via [`ChainFollower::watch`] *before* submitting,
//! so the follower cannot miss the inclusion block. [`ChainFollower::status`]
//! then reports [`ConfirmationStatus::Confirmed`] with real depth
//! (`tip_height − inclusion_height + 1`), and a rollback past the inclusion
//! point reverts the tx to [`ConfirmationStatus::Pending`] until it reappears
//! on the new chain.
//!
//! The follower starts at the tip it finds at boot: it only observes blocks
//! from then on. Transactions submitted by an earlier process are unknown to
//! it — `status()` returns `None` for those and callers fall back to the
//! coarse UTxO-presence check.
//!
//! Scope note: watched entries are never evicted. A node watches its own
//! submissions only (a few per payment), so growth is bounded by write volume;
//! revisit if a node ever submits at high rate for months.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tracing::{debug, info, warn};

use super::ogmios::{OgmiosError, OgmiosSession};
use crate::ConfirmationStatus;

/// Recent block points retained for reconnect intersection. 32 points ≈ 10
/// minutes of preprod blocks — far deeper than any reconnect gap; if all are
/// rolled back or unknown we re-intersect at the current tip instead.
const RECENT_POINTS_CAP: usize = 32;
/// Liveness bound on `nextBlock`: it blocks server-side until a block exists,
/// and preprod gaps run seconds to a few minutes. No response for this long
/// means a dead connection — drop it and reconnect.
const NEXT_BLOCK_WAIT: Duration = Duration::from_secs(900);
/// Bound for non-blocking calls on the session (intersection, tip query).
const CALL_WAIT: Duration = Duration::from_secs(15);
const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

/// Where a watched transaction stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watch {
    /// Registered, not yet seen in a block (or rolled back out of one).
    Pending,
    /// Seen in the block at `height` / `slot` on the current chain.
    Included { height: u64, slot: u64 },
}

/// A block point as used for intersection negotiation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Point {
    slot: u64,
    id: String,
}

/// The follower's shared state. Pure — every transition is a plain method, so
/// the whole machine is unit-testable without a node or a socket.
#[derive(Debug, Default)]
struct FollowerState {
    tip_height: Option<u64>,
    /// Keyed by lowercase tx id.
    watched: HashMap<String, Watch>,
    /// Points of recently processed blocks, oldest first.
    recent_points: VecDeque<Point>,
}

/// A processed `nextBlock` step.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Forward(BlockSummary),
    /// Rollback to the given slot; `None` = rollback to origin.
    Backward(Option<u64>),
}

/// The parts of a chain-sync block the follower needs.
#[derive(Debug, PartialEq, Eq)]
struct BlockSummary {
    id: String,
    slot: u64,
    height: u64,
    tx_ids: Vec<String>,
}

impl FollowerState {
    fn watch(&mut self, tx_id: &str) {
        self.watched.entry(tx_id.to_lowercase()).or_insert(Watch::Pending);
    }

    fn unwatch(&mut self, tx_id: &str) {
        self.watched.remove(&tx_id.to_lowercase());
    }

    fn apply_forward(&mut self, block: BlockSummary, tip_height: Option<u64>) {
        for id in &block.tx_ids {
            if let Some(w) = self.watched.get_mut(&id.to_lowercase()) {
                info!(tx_id = %id, height = block.height, slot = block.slot, "watched tx included on chain");
                *w = Watch::Included { height: block.height, slot: block.slot };
            }
        }
        self.recent_points.push_back(Point { slot: block.slot, id: block.id });
        while self.recent_points.len() > RECENT_POINTS_CAP {
            self.recent_points.pop_front();
        }
        // The tip in a forward step is at least this block; prefer the
        // server-reported tip so depth is right while catching up.
        self.tip_height = Some(tip_height.unwrap_or(block.height).max(block.height));
    }

    fn apply_backward(&mut self, rollback_slot: Option<u64>, tip_height: Option<u64>) {
        for (id, w) in self.watched.iter_mut() {
            if let Watch::Included { slot, .. } = *w {
                // Rolled back if included after the rollback point (rollback
                // to origin, MSRV-friendly spelling of `is_none_or`).
                let rolled_back = match rollback_slot {
                    Some(cut) => slot > cut,
                    None => true,
                };
                if rolled_back {
                    warn!(tx_id = %id, slot, ?rollback_slot, "watched tx rolled back off chain");
                    *w = Watch::Pending;
                }
            }
        }
        self.recent_points
            .retain(|p| rollback_slot.is_some_and(|cut| p.slot <= cut));
        if tip_height.is_some() {
            self.tip_height = tip_height;
        }
    }

    /// `None` = this tx was never watched here (caller should fall back).
    fn status(&self, tx_id: &str) -> Option<ConfirmationStatus> {
        match self.watched.get(&tx_id.to_lowercase())? {
            Watch::Pending => Some(ConfirmationStatus::Pending),
            Watch::Included { height, .. } => {
                let tip = self.tip_height.unwrap_or(*height).max(*height);
                let depth = (tip - height + 1).min(u32::MAX as u64) as u32;
                Some(ConfirmationStatus::Confirmed { depth })
            }
        }
    }

    /// Intersection candidates for (re)connect: recent points newest-first.
    fn intersection_points(&self) -> Vec<Point> {
        self.recent_points.iter().rev().cloned().collect()
    }
}

/// Handle to the follower. Cheap to clone via `Arc`; spawned once per
/// Ogmios endpoint and shared by every backend using that endpoint's wallet.
pub struct ChainFollower {
    state: Arc<Mutex<FollowerState>>,
}

impl ChainFollower {
    /// Start the background chain-sync task. Never fails at spawn time: a
    /// broken endpoint is retried with backoff, and `status()` simply returns
    /// `None` (callers fall back) until the follower is live.
    pub fn spawn(ogmios_url: String) -> Arc<Self> {
        let state = Arc::new(Mutex::new(FollowerState::default()));
        tokio::spawn(run(ogmios_url, Arc::clone(&state)));
        Arc::new(Self { state })
    }

    /// Register a tx id to track. Call *before* submitting the transaction so
    /// the inclusion block cannot arrive ahead of the registration.
    pub fn watch(&self, tx_id: &str) {
        self.lock().watch(tx_id);
    }

    /// Drop a registration (e.g. the submission it was minted for failed).
    pub fn unwatch(&self, tx_id: &str) {
        self.lock().unwatch(tx_id);
    }

    /// Confirmation status of a watched tx; `None` if this tx was never
    /// watched by this process (caller decides the fallback).
    pub fn status(&self, tx_id: &str) -> Option<ConfirmationStatus> {
        self.lock().status(tx_id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FollowerState> {
        self.state.lock().expect("follower state mutex poisoned")
    }
}

async fn run(url: String, state: Arc<Mutex<FollowerState>>) {
    loop {
        if let Err(e) = follow(&url, &state).await {
            warn!(url = %url, error = %e, "chain follower disconnected; reconnecting");
        }
        tokio::time::sleep(RECONNECT_BACKOFF).await;
    }
}

/// One connection lifetime: connect, negotiate an intersection, then follow
/// the chain until the connection errors.
async fn follow(url: &str, state: &Arc<Mutex<FollowerState>>) -> Result<(), OgmiosError> {
    let mut session = OgmiosSession::connect(url).await?;

    // Prefer re-intersecting where we left off; a fresh (or fully rolled
    // back) follower intersects at the current tip — it only ever needs to
    // see blocks from now on, since txs are watched before submission.
    let mut candidates = state.lock().expect("follower state mutex poisoned").intersection_points();
    if candidates.is_empty() {
        let tip = session
            .request("queryNetwork/tip", Value::Object(Default::default()), CALL_WAIT)
            .await?;
        let (slot, id) = (
            tip.get("slot").and_then(Value::as_u64),
            tip.get("id").and_then(Value::as_str),
        );
        match (slot, id) {
            (Some(slot), Some(id)) => candidates.push(Point { slot, id: id.to_string() }),
            _ => return Err(OgmiosError::Protocol(format!("tip: unexpected shape {tip}"))),
        }
    }

    let points: Vec<Value> = candidates
        .iter()
        .map(|p| json!({ "slot": p.slot, "id": p.id }))
        .collect();
    let intersection = session
        .request("findIntersection", json!({ "points": points }), CALL_WAIT)
        .await?;
    info!(url = %url, ?intersection, "chain follower intersected");

    loop {
        let v = session
            .request("nextBlock", Value::Object(Default::default()), NEXT_BLOCK_WAIT)
            .await?;
        let tip_height = parse_tip_height(&v);
        match parse_step(&v) {
            Ok(Step::Forward(block)) => {
                debug!(height = block.height, slot = block.slot, txs = block.tx_ids.len(), "block");
                state
                    .lock()
                    .expect("follower state mutex poisoned")
                    .apply_forward(block, tip_height);
            }
            Ok(Step::Backward(slot)) => {
                state
                    .lock()
                    .expect("follower state mutex poisoned")
                    .apply_backward(slot, tip_height);
            }
            // A block we can't parse (unexpected era shape) can't be scanned
            // for watched txs, but the stream itself is healthy — log and
            // keep following rather than tearing the connection down.
            Err(e) => warn!(error = %e, "skipping unparseable nextBlock response"),
        }
    }
}

/// Tip height from a `nextBlock` response (`"tip": {"height": …}` — absent
/// when the tip is `"origin"`).
fn parse_tip_height(v: &Value) -> Option<u64> {
    v.get("tip")?.get("height")?.as_u64()
}

/// Decode one `nextBlock` result into a [`Step`].
fn parse_step(v: &Value) -> Result<Step, String> {
    match v.get("direction").and_then(Value::as_str) {
        Some("forward") => {
            let block = v.get("block").ok_or("forward without block")?;
            let id = block
                .get("id")
                .and_then(Value::as_str)
                .ok_or("block missing id")?
                .to_string();
            let slot = block.get("slot").and_then(Value::as_u64).ok_or("block missing slot")?;
            let height = block
                .get("height")
                .and_then(Value::as_u64)
                .ok_or("block missing height")?;
            let tx_ids = block
                .get("transactions")
                .and_then(Value::as_array)
                .map(|txs| {
                    txs.iter()
                        .filter_map(|t| t.get("id").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            Ok(Step::Forward(BlockSummary { id, slot, height, tx_ids }))
        }
        Some("backward") => match v.get("point") {
            Some(Value::String(s)) if s == "origin" => Ok(Step::Backward(None)),
            Some(point) => {
                let slot = point
                    .get("slot")
                    .and_then(Value::as_u64)
                    .ok_or("backward point missing slot")?;
                Ok(Step::Backward(Some(slot)))
            }
            None => Err("backward without point".into()),
        },
        other => Err(format!("unknown direction {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(height: u64, slot: u64, tx_ids: &[&str]) -> BlockSummary {
        BlockSummary {
            id: format!("block-{height}"),
            slot,
            height,
            tx_ids: tx_ids.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn unwatched_tx_is_unknown() {
        let s = FollowerState::default();
        assert_eq!(s.status("tx-1"), None);
    }

    #[test]
    fn watched_tx_is_pending_until_seen() {
        let mut s = FollowerState::default();
        s.watch("TX-1");
        assert_eq!(s.status("tx-1"), Some(ConfirmationStatus::Pending));
        s.apply_forward(block(100, 1000, &["tx-other"]), Some(100));
        assert_eq!(s.status("tx-1"), Some(ConfirmationStatus::Pending));
    }

    #[test]
    fn inclusion_reports_depth_from_tip() {
        let mut s = FollowerState::default();
        s.watch("tx-1");
        s.apply_forward(block(100, 1000, &["tx-1"]), Some(100));
        assert_eq!(s.status("tx-1"), Some(ConfirmationStatus::Confirmed { depth: 1 }));
        // Five more blocks on top → depth 6. Tx ids are matched
        // case-insensitively in both directions.
        s.apply_forward(block(105, 1050, &[]), Some(105));
        assert_eq!(s.status("TX-1"), Some(ConfirmationStatus::Confirmed { depth: 6 }));
    }

    #[test]
    fn tip_ahead_of_processed_block_counts_toward_depth() {
        // While catching up, the server-reported tip is ahead of the block
        // just processed — depth must use the real tip.
        let mut s = FollowerState::default();
        s.watch("tx-1");
        s.apply_forward(block(100, 1000, &["tx-1"]), Some(110));
        assert_eq!(s.status("tx-1"), Some(ConfirmationStatus::Confirmed { depth: 11 }));
    }

    #[test]
    fn rollback_past_inclusion_reverts_to_pending() {
        let mut s = FollowerState::default();
        s.watch("tx-1");
        s.apply_forward(block(100, 1000, &["tx-1"]), Some(100));
        s.apply_backward(Some(999), Some(99));
        assert_eq!(s.status("tx-1"), Some(ConfirmationStatus::Pending));
        // Re-included on the new chain → confirmed again.
        s.apply_forward(block(100, 1001, &["tx-1"]), Some(100));
        assert_eq!(s.status("tx-1"), Some(ConfirmationStatus::Confirmed { depth: 1 }));
    }

    #[test]
    fn rollback_before_inclusion_keeps_confirmation() {
        let mut s = FollowerState::default();
        s.watch("tx-1");
        s.apply_forward(block(100, 1000, &["tx-1"]), Some(100));
        s.apply_forward(block(101, 1010, &[]), Some(101));
        // Rollback to the inclusion slot itself: the inclusion block survives.
        s.apply_backward(Some(1000), Some(100));
        assert_eq!(s.status("tx-1"), Some(ConfirmationStatus::Confirmed { depth: 1 }));
    }

    #[test]
    fn rollback_to_origin_reverts_everything() {
        let mut s = FollowerState::default();
        s.watch("tx-1");
        s.apply_forward(block(100, 1000, &["tx-1"]), Some(100));
        s.apply_backward(None, None);
        assert_eq!(s.status("tx-1"), Some(ConfirmationStatus::Pending));
        assert!(s.intersection_points().is_empty());
    }

    #[test]
    fn unwatch_forgets_the_tx() {
        let mut s = FollowerState::default();
        s.watch("tx-1");
        s.unwatch("TX-1");
        assert_eq!(s.status("tx-1"), None);
    }

    #[test]
    fn recent_points_are_capped_and_newest_first_for_intersection() {
        let mut s = FollowerState::default();
        for h in 0..(RECENT_POINTS_CAP as u64 + 10) {
            s.apply_forward(block(h, h * 10, &[]), Some(h));
        }
        let pts = s.intersection_points();
        assert_eq!(pts.len(), RECENT_POINTS_CAP);
        assert_eq!(pts[0].slot, (RECENT_POINTS_CAP as u64 + 9) * 10, "newest first");
        assert!(pts[0].slot > pts[pts.len() - 1].slot);
    }

    #[test]
    fn parse_forward_step() {
        let v = serde_json::json!({
            "direction": "forward",
            "block": {
                "type": "praos",
                "id": "abc",
                "slot": 1000,
                "height": 100,
                "transactions": [{ "id": "tx-1" }, { "id": "tx-2" }]
            },
            "tip": { "slot": 1050, "id": "def", "height": 105 }
        });
        assert_eq!(parse_step(&v), Ok(Step::Forward(block(100, 1000, &["tx-1", "tx-2"]))
            .into_forward_with_id("abc")));
        assert_eq!(parse_tip_height(&v), Some(105));
    }

    #[test]
    fn parse_forward_step_without_transactions_field() {
        let v = serde_json::json!({
            "direction": "forward",
            "block": { "id": "abc", "slot": 1000, "height": 100 },
            "tip": { "slot": 1000, "id": "abc", "height": 100 }
        });
        match parse_step(&v) {
            Ok(Step::Forward(b)) => assert!(b.tx_ids.is_empty()),
            other => panic!("expected forward, got {other:?}"),
        }
    }

    #[test]
    fn parse_backward_step_to_point_and_origin() {
        let v = serde_json::json!({
            "direction": "backward",
            "point": { "slot": 999, "id": "abc" },
            "tip": { "slot": 1050, "id": "def", "height": 105 }
        });
        assert_eq!(parse_step(&v), Ok(Step::Backward(Some(999))));

        let v = serde_json::json!({
            "direction": "backward",
            "point": "origin",
            "tip": "origin"
        });
        assert_eq!(parse_step(&v), Ok(Step::Backward(None)));
        assert_eq!(parse_tip_height(&v), None);
    }

    #[test]
    fn parse_rejects_malformed_steps() {
        assert!(parse_step(&serde_json::json!({ "direction": "sideways" })).is_err());
        assert!(parse_step(&serde_json::json!({ "direction": "forward" })).is_err());
        assert!(parse_step(&serde_json::json!({
            "direction": "forward",
            "block": { "id": "abc", "slot": 1000 } // no height
        }))
        .is_err());
    }

    impl Step {
        /// Test helper: `block()` fabricates ids as `block-{height}`; real
        /// parses carry the chain's id. Rebuild the expectation with it.
        fn into_forward_with_id(self, id: &str) -> Step {
            match self {
                Step::Forward(mut b) => {
                    b.id = id.to_string();
                    Step::Forward(b)
                }
                other => other,
            }
        }
    }
}
