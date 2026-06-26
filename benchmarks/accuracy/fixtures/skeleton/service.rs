//! Ledger service fixture for the skeleton accuracy task. It is intentionally
//! larger than the control budget (2 KB) with long function bodies, so a naive
//! truncated read drops the signatures near the end while the tree-sitter
//! skeleton keeps every signature compactly.

use std::collections::BTreeMap;

/// A single posted entry in the ledger.
pub struct Entry {
    pub id: u64,
    pub account: String,
    pub amount_cents: i64,
    pub posted_at: u64,
}

/// A parsed, validated batch ready to post.
pub struct Batch {
    pub entries: Vec<Entry>,
    pub source: String,
}

/// Parse a raw `id,account,amount,ts` line into an `Entry`. The body is long on
/// purpose so the byte budget is spent before the interesting signatures below.
pub fn parse_entry(line: &str) -> Option<Entry> {
    let mut parts = line.split(',');
    let id = parts.next()?.trim().parse().ok()?;
    let account = parts.next()?.trim().to_string();
    let amount_cents = parts.next()?.trim().parse().ok()?;
    let posted_at = parts.next()?.trim().parse().ok()?;
    if account.is_empty() {
        return None;
    }
    Some(Entry {
        id,
        account,
        amount_cents,
        posted_at,
    })
}

/// Validate that an amount is within the per-entry posting limits. Returns the
/// normalized amount in cents or an explanatory error string.
pub fn validate_amount(amount_cents: i64) -> Result<i64, String> {
    const MAX_CENTS: i64 = 100_000_00;
    if amount_cents == 0 {
        return Err("zero amount".to_string());
    }
    if amount_cents.abs() > MAX_CENTS {
        return Err(format!("amount {amount_cents} exceeds limit {MAX_CENTS}"));
    }
    Ok(amount_cents)
}

/// Apply a flat processing fee to every debit in a batch, leaving credits
/// untouched. Mutates the batch in place and returns the total fee charged.
pub fn apply_fees(batch: &mut Batch, fee_cents: i64) -> i64 {
    let mut total = 0;
    for e in &mut batch.entries {
        if e.amount_cents < 0 {
            e.amount_cents -= fee_cents;
            total += fee_cents;
        }
    }
    total
}

/// Group entries by account into a deterministic map, summing amounts so a
/// caller can see each account's net movement across the batch.
pub fn net_by_account(batch: &Batch) -> BTreeMap<String, i64> {
    let mut out: BTreeMap<String, i64> = BTreeMap::new();
    for e in &batch.entries {
        *out.entry(e.account.clone()).or_insert(0) += e.amount_cents;
    }
    out
}

/// Summary of a reconciliation pass.
pub struct Summary {
    pub posted: usize,
    pub rejected: usize,
    pub net_cents: i64,
}

/// Error raised when reconciliation cannot complete.
pub enum LedgerError {
    Imbalanced(i64),
    Empty,
}

/// Reconcile a batch as of a cutoff timestamp, posting everything at or before
/// the cutoff and rejecting the rest. THIS SIGNATURE IS PAST THE CONTROL BUDGET.
pub fn reconcile_ledger(entries: &[Entry], cutoff: u64) -> Result<Summary, LedgerError> {
    if entries.is_empty() {
        return Err(LedgerError::Empty);
    }
    let mut posted = 0;
    let mut rejected = 0;
    let mut net_cents = 0;
    for e in entries {
        if e.posted_at <= cutoff {
            posted += 1;
            net_cents += e.amount_cents;
        } else {
            rejected += 1;
        }
    }
    if net_cents != 0 {
        return Err(LedgerError::Imbalanced(net_cents));
    }
    Ok(Summary {
        posted,
        rejected,
        net_cents,
    })
}

/// A running ledger that accumulates balances by period.
pub struct Ledger {
    balances: BTreeMap<u32, i64>,
}

impl Ledger {
    /// Finalize a period: freeze its balance and return it. ALSO PAST BUDGET.
    pub fn close_period(&mut self, period: u32) -> i64 {
        let bal = self.balances.get(&period).copied().unwrap_or(0);
        self.balances.insert(period, bal);
        bal
    }
}
