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
//! Without a store the follower starts at the tip it finds at boot and only
//! observes blocks from then on: transactions submitted by an earlier process
//! are unknown to it (`status()` returns `None` and callers fall back to the
//! coarse UTxO-presence check), and so are payments that arrived while the
//! process was down. With a [`FollowerStore`] it resumes from the points it
//! last processed, replaying the blocks it missed, and the payments it has
//! recorded survive a restart.
//!
//! Scope note: watched entries are never evicted. A node watches its own
//! submissions only (a few per payment), so growth is bounded by write volume;
//! revisit if a node ever submits at high rate for months.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tracing::{debug, info, warn};

use super::ogmios::{OgmiosError, OgmiosSession};
use crate::settlement::IncomingPayment;
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
    /// Addresses whose incoming payments are recorded (the node's own wallet),
    /// so a payment can be checked even after the node has spent it.
    watched_addresses: HashSet<String>,
    /// Payments to a watched address seen since the follower started, keyed by
    /// lowercase tx id.
    payments: HashMap<String, Payment>,
}

/// Lovelace one transaction paid to one watched address, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Payment {
    address: String,
    lovelace: u64,
    height: u64,
    slot: u64,
}

/// A block point as a store keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPoint {
    pub slot: u64,
    pub id: String,
}

/// A payment to a watched address as a store keeps it: what one transaction
/// paid to the address in total, and the block it was seen in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPayment {
    /// Lowercase transaction id.
    pub tx_id: String,
    pub address: String,
    pub lovelace: u64,
    pub height: u64,
    pub slot: u64,
}

/// What a store hands back at start-up.
#[derive(Debug, Clone, Default)]
pub struct FollowerSnapshot {
    /// Oldest first, as the follower processed them.
    pub points: Vec<StoredPoint>,
    pub payments: Vec<StoredPayment>,
}

/// Durable memory for the follower: the points it last processed and the
/// payments it has recorded. Implemented by the node over its database; the
/// follower calls it after every block and never lets a store error stop the
/// stream.
#[async_trait::async_trait]
pub trait FollowerStore: Send + Sync {
    async fn load(&self) -> crate::Result<FollowerSnapshot>;
    /// Replace the stored points with these (oldest first); empty forgets them.
    async fn save_points(&self, points: &[StoredPoint]) -> crate::Result<()>;
    /// Insert or replace the payment for `payment.tx_id`.
    async fn save_payment(&self, payment: &StoredPayment) -> crate::Result<()>;
    /// A rollback: drop payments seen after `slot` (`None` = all of them).
    async fn drop_payments_after(&self, slot: Option<u64>) -> crate::Result<()>;
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
    /// Every output of every tx: (tx id, address, lovelace).
    outputs: Vec<(String, String, u64)>,
}

impl FollowerState {
    fn watch(&mut self, tx_id: &str) {
        self.watched.entry(tx_id.to_lowercase()).or_insert(Watch::Pending);
    }

    fn unwatch(&mut self, tx_id: &str) {
        self.watched.remove(&tx_id.to_lowercase());
    }

    /// Restore what a store kept from an earlier process.
    fn seed(&mut self, points: Vec<StoredPoint>, payments: Vec<StoredPayment>) {
        self.recent_points = points
            .into_iter()
            .map(|p| Point { slot: p.slot, id: p.id })
            .collect();
        while self.recent_points.len() > RECENT_POINTS_CAP {
            self.recent_points.pop_front();
        }
        for p in payments {
            self.payments.insert(
                p.tx_id.to_lowercase(),
                Payment { address: p.address, lovelace: p.lovelace, height: p.height, slot: p.slot },
            );
        }
    }

    /// Drop the remembered points, so the next intersection starts at the tip
    /// (the server did not recognise any of them).
    fn forget_points(&mut self) {
        self.recent_points.clear();
    }

    /// Apply a forward step; returns the payments to watched addresses this
    /// block added to, one per transaction, with their running totals.
    fn apply_forward(&mut self, block: BlockSummary, tip_height: Option<u64>) -> Vec<StoredPayment> {
        let mut recorded: Vec<String> = Vec::new();
        for (tx_id, address, lovelace) in &block.outputs {
            if self.watched_addresses.contains(address) {
                let key = tx_id.to_lowercase();
                let p = self.payments.entry(key.clone()).or_insert(Payment {
                    address: address.clone(),
                    lovelace: 0,
                    height: block.height,
                    slot: block.slot,
                });
                p.lovelace += lovelace;
                if !recorded.contains(&key) {
                    recorded.push(key);
                }
                info!(tx_id = %tx_id, %address, lovelace, height = block.height, "payment to a watched address");
            }
        }
        let seen = recorded
            .into_iter()
            .filter_map(|key| {
                let p = self.payments.get(&key)?;
                Some(StoredPayment {
                    tx_id: key.clone(),
                    address: p.address.clone(),
                    lovelace: p.lovelace,
                    height: p.height,
                    slot: p.slot,
                })
            })
            .collect();
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
        seen
    }

    /// The remembered points as a store keeps them (oldest first).
    fn stored_points(&self) -> Vec<StoredPoint> {
        self.recent_points
            .iter()
            .map(|p| StoredPoint { slot: p.slot, id: p.id.clone() })
            .collect()
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
        self.payments
            .retain(|_, p| rollback_slot.is_some_and(|cut| p.slot <= cut));
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

    /// What `tx_id` paid to `address`, if the follower saw it; `None` if it
    /// did not (not on chain, rolled back, or before the follower started).
    fn payment(&self, tx_id: &str, address: &str) -> Option<IncomingPayment> {
        let p = self.payments.get(&tx_id.to_lowercase())?;
        if p.address != address {
            return None;
        }
        let tip = self.tip_height.unwrap_or(p.height).max(p.height);
        Some(IncomingPayment {
            lovelace: p.lovelace,
            depth: (tip - p.height + 1).min(u32::MAX as u64) as u32,
        })
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
        tokio::spawn(run(ogmios_url, Arc::clone(&state), None));
        Arc::new(Self { state })
    }

    /// Like [`Self::spawn`], but resume from what `store` kept: the stored
    /// payments are known before the first block arrives, and the chain is
    /// followed again from the last stored point. A store that fails to load
    /// is logged and the follower starts at the tip, as without a store.
    pub async fn spawn_with_store(ogmios_url: String, store: Arc<dyn FollowerStore>) -> Arc<Self> {
        let mut state = FollowerState::default();
        match store.load().await {
            Ok(snapshot) => {
                info!(
                    points = snapshot.points.len(),
                    payments = snapshot.payments.len(),
                    "chain follower resuming from its store"
                );
                state.seed(snapshot.points, snapshot.payments);
            }
            Err(e) => warn!(error = %e, "chain follower store unreadable; starting at the tip"),
        }
        let state = Arc::new(Mutex::new(state));
        tokio::spawn(run(ogmios_url, Arc::clone(&state), Some(store)));
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

    /// Record every payment to `address` from now on (see [`Self::payment`]).
    pub fn watch_address(&self, address: &str) {
        self.lock().watched_addresses.insert(address.to_string());
    }

    /// What `tx_id` paid to `address` (a watched address) and how deep it is;
    /// `None` if the follower has not seen it since it started.
    pub fn payment(&self, tx_id: &str, address: &str) -> Option<IncomingPayment> {
        self.lock().payment(tx_id, address)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FollowerState> {
        self.state.lock().expect("follower state mutex poisoned")
    }
}

async fn run(url: String, state: Arc<Mutex<FollowerState>>, store: Option<Arc<dyn FollowerStore>>) {
    loop {
        if let Err(e) = follow(&url, &state, store.as_deref()).await {
            warn!(url = %url, error = %e, "chain follower disconnected; reconnecting");
        }
        tokio::time::sleep(RECONNECT_BACKOFF).await;
    }
}

/// Ogmios's JSON-RPC code when none of the offered points is on its chain.
const INTERSECTION_NOT_FOUND: i64 = 1000;

/// Negotiate where to continue from: the remembered points, or the current
/// tip when there are none or the server knows none of them (a store older
/// than the node's retention, or a fully rolled-back chain).
async fn intersect(
    session: &mut OgmiosSession,
    state: &Arc<Mutex<FollowerState>>,
    store: Option<&dyn FollowerStore>,
) -> Result<Value, OgmiosError> {
    let candidates = state.lock().expect("follower state mutex poisoned").intersection_points();
    if !candidates.is_empty() {
        let points: Vec<Value> = candidates
            .iter()
            .map(|p| json!({ "slot": p.slot, "id": p.id }))
            .collect();
        match session
            .request("findIntersection", json!({ "points": points }), CALL_WAIT)
            .await
        {
            Ok(intersection) => return Ok(intersection),
            Err(OgmiosError::Rpc { code: INTERSECTION_NOT_FOUND, .. }) => {
                warn!(
                    offered = candidates.len(),
                    "chain follower: none of the remembered points is on the node's chain; starting at the tip"
                );
                state.lock().expect("follower state mutex poisoned").forget_points();
                if let Some(store) = store {
                    if let Err(e) = store.save_points(&[]).await {
                        warn!(error = %e, "chain follower store: forgetting points failed");
                    }
                }
            }
            Err(e) => return Err(e),
        }
    }
    let tip = session
        .request("queryNetwork/tip", Value::Object(Default::default()), CALL_WAIT)
        .await?;
    let (slot, id) = (
        tip.get("slot").and_then(Value::as_u64),
        tip.get("id").and_then(Value::as_str),
    );
    let point = match (slot, id) {
        (Some(slot), Some(id)) => json!({ "slot": slot, "id": id }),
        _ => return Err(OgmiosError::Protocol(format!("tip: unexpected shape {tip}"))),
    };
    session
        .request("findIntersection", json!({ "points": [point] }), CALL_WAIT)
        .await
}

/// One connection lifetime: connect, negotiate an intersection, then follow
/// the chain until the connection errors.
async fn follow(
    url: &str,
    state: &Arc<Mutex<FollowerState>>,
    store: Option<&dyn FollowerStore>,
) -> Result<(), OgmiosError> {
    let mut session = OgmiosSession::connect(url).await?;

    // Prefer re-intersecting where we left off; a fresh (or fully rolled
    // back) follower intersects at the current tip — it only ever needs to
    // see blocks from now on, since txs are watched before submission.
    let intersection = intersect(&mut session, state, store).await?;
    info!(url = %url, ?intersection, "chain follower intersected");

    loop {
        let v = session
            .request("nextBlock", Value::Object(Default::default()), NEXT_BLOCK_WAIT)
            .await?;
        let tip_height = parse_tip_height(&v);
        match parse_step(&v) {
            Ok(Step::Forward(block)) => {
                debug!(height = block.height, slot = block.slot, txs = block.tx_ids.len(), "block");
                let (seen, points) = {
                    let mut s = state.lock().expect("follower state mutex poisoned");
                    let seen = s.apply_forward(block, tip_height);
                    (seen, s.stored_points())
                };
                if let Some(store) = store {
                    for p in &seen {
                        if let Err(e) = store.save_payment(p).await {
                            warn!(tx_id = %p.tx_id, error = %e, "chain follower store: saving a payment failed");
                        }
                    }
                    if let Err(e) = store.save_points(&points).await {
                        warn!(error = %e, "chain follower store: saving points failed");
                    }
                }
            }
            Ok(Step::Backward(slot)) => {
                let points = {
                    let mut s = state.lock().expect("follower state mutex poisoned");
                    s.apply_backward(slot, tip_height);
                    s.stored_points()
                };
                if let Some(store) = store {
                    if let Err(e) = store.drop_payments_after(slot).await {
                        warn!(error = %e, "chain follower store: dropping rolled-back payments failed");
                    }
                    if let Err(e) = store.save_points(&points).await {
                        warn!(error = %e, "chain follower store: saving points failed");
                    }
                }
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
            let txs = block
                .get("transactions")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let tx_ids = txs
                .iter()
                .filter_map(|t| t.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect();
            let outputs = txs
                .iter()
                .filter_map(|t| Some((t.get("id")?.as_str()?, t.get("outputs")?.as_array()?)))
                .flat_map(|(tx_id, outs)| {
                    outs.iter().filter_map(move |o| {
                        let address = o.get("address")?.as_str()?;
                        let lovelace = o.get("value")?.get("ada")?.get("lovelace")?.as_u64()?;
                        Some((tx_id.to_string(), address.to_string(), lovelace))
                    })
                })
                .collect();
            Ok(Step::Forward(BlockSummary { id, slot, height, tx_ids, outputs }))
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
            outputs: Vec::new(),
        }
    }

    #[test]
    fn payments_to_a_watched_address_are_recorded_and_rolled_back() {
        let mut s = FollowerState::default();
        s.watched_addresses.insert("addr_me".into());
        let mut b = block(100, 1000, &["tx-pay"]);
        b.outputs = vec![
            ("tx-pay".into(), "addr_me".into(), 61_943_319),
            ("tx-pay".into(), "addr_change".into(), 9_932_012_283),
            ("tx-other".into(), "addr_else".into(), 5),
        ];
        s.apply_forward(b, Some(100));
        s.apply_forward(block(101, 1020, &[]), Some(101));
        assert_eq!(
            s.payment("TX-PAY", "addr_me"),
            Some(IncomingPayment { lovelace: 61_943_319, depth: 2 })
        );
        assert_eq!(s.payment("tx-pay", "addr_change"), None, "only watched addresses");
        assert_eq!(s.payment("tx-other", "addr_else"), None);

        s.apply_backward(Some(999), Some(99));
        assert_eq!(s.payment("tx-pay", "addr_me"), None, "rolled back off chain");
    }

    fn stored_point(slot: u64) -> StoredPoint {
        StoredPoint { slot, id: format!("block-at-{slot}") }
    }

    #[test]
    fn seeded_points_are_offered_newest_first_for_intersection() {
        let mut s = FollowerState::default();
        s.seed(vec![stored_point(10), stored_point(20), stored_point(30)], Vec::new());
        let slots: Vec<u64> = s.intersection_points().iter().map(|p| p.slot).collect();
        assert_eq!(slots, vec![30, 20, 10]);
    }

    #[test]
    fn seeded_payments_are_visible_and_rolled_back_like_live_ones() {
        let mut s = FollowerState::default();
        s.watched_addresses.insert("addr_me".into());
        s.seed(
            vec![stored_point(1000)],
            vec![StoredPayment {
                tx_id: "TX-OLD".into(),
                address: "addr_me".into(),
                lovelace: 7_000_000,
                height: 100,
                slot: 1000,
            }],
        );
        s.apply_forward(block(103, 1060, &[]), Some(103));
        assert_eq!(
            s.payment("tx-old", "addr_me"),
            Some(IncomingPayment { lovelace: 7_000_000, depth: 4 }),
            "a payment seen by an earlier process counts, with depth from the live tip"
        );
        s.apply_backward(Some(999), Some(99));
        assert_eq!(s.payment("tx-old", "addr_me"), None, "a rollback below it still removes it");
    }

    #[test]
    fn apply_forward_reports_newly_recorded_payments_only() {
        let mut s = FollowerState::default();
        s.watched_addresses.insert("addr_me".into());
        let mut b = block(100, 1000, &["tx-pay"]);
        b.outputs = vec![
            ("tx-pay".into(), "addr_me".into(), 1_000_000),
            ("tx-pay".into(), "addr_me".into(), 2_000_000),
            ("tx-pay".into(), "addr_change".into(), 5),
        ];
        let seen = s.apply_forward(b, Some(100));
        assert_eq!(
            seen,
            vec![StoredPayment {
                tx_id: "tx-pay".into(),
                address: "addr_me".into(),
                lovelace: 3_000_000,
                height: 100,
                slot: 1000,
            }],
            "one entry per tx, with the whole amount it paid to the address"
        );
        assert!(s.apply_forward(block(101, 1020, &[]), Some(101)).is_empty());
    }

    #[test]
    fn forgetting_points_makes_the_next_intersection_start_at_the_tip() {
        let mut s = FollowerState::default();
        s.seed(vec![stored_point(10)], Vec::new());
        s.forget_points();
        assert!(s.intersection_points().is_empty());
    }

    /// A store that keeps everything in memory, for the seeding test.
    #[derive(Default)]
    struct InMemoryFollowerStore {
        snapshot: std::sync::Mutex<FollowerSnapshot>,
    }

    #[async_trait::async_trait]
    impl FollowerStore for InMemoryFollowerStore {
        async fn load(&self) -> crate::Result<FollowerSnapshot> {
            Ok(self.snapshot.lock().unwrap().clone())
        }
        async fn save_points(&self, points: &[StoredPoint]) -> crate::Result<()> {
            self.snapshot.lock().unwrap().points = points.to_vec();
            Ok(())
        }
        async fn save_payment(&self, payment: &StoredPayment) -> crate::Result<()> {
            let mut snap = self.snapshot.lock().unwrap();
            snap.payments.retain(|p| p.tx_id != payment.tx_id);
            snap.payments.push(payment.clone());
            Ok(())
        }
        async fn drop_payments_after(&self, slot: Option<u64>) -> crate::Result<()> {
            self.snapshot.lock().unwrap().payments.retain(|p| slot.is_some_and(|cut| p.slot <= cut));
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_follower_started_with_a_store_knows_the_stored_payments_before_any_block() {
        let store = Arc::new(InMemoryFollowerStore::default());
        store
            .save_payment(&StoredPayment {
                tx_id: "tx-stored".into(),
                address: "addr_me".into(),
                lovelace: 42,
                height: 5,
                slot: 50,
            })
            .await
            .unwrap();
        // Port 1 never answers: the background task keeps retrying and the
        // state must already carry the stored payment.
        let follower = ChainFollower::spawn_with_store("ws://127.0.0.1:1".into(), store).await;
        follower.watch_address("addr_me");
        assert_eq!(
            follower.payment("tx-stored", "addr_me"),
            Some(IncomingPayment { lovelace: 42, depth: 1 })
        );
    }

    #[test]
    fn parse_step_collects_outputs() {
        let v = json!({"direction": "forward", "block": {"id": "b", "slot": 7, "height": 3,
            "transactions": [{"id": "t1", "outputs": [
                {"address": "addr_a", "value": {"ada": {"lovelace": 1500000}}},
                {"address": "addr_b", "value": {"ada": {"lovelace": 2}}}
            ]}]}});
        match parse_step(&v).unwrap() {
            Step::Forward(b) => assert_eq!(
                b.outputs,
                vec![("t1".to_string(), "addr_a".to_string(), 1_500_000), ("t1".into(), "addr_b".into(), 2)]
            ),
            other => panic!("unexpected {other:?}"),
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
