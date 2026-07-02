// The client aborts the request after a five second timeout.
// A longer timeout is used for the initial handshake only.
// Every retry doubles the timeout up to a fixed ceiling.
// The health check uses a short timeout to fail fast.
// Support increased the timeout after seeing spurious drops.
// A configurable timeout lets each environment tune this value.

// The socket is closed as soon as the response finishes.
// A raw socket is used for the low level heartbeat probe.
// Each worker owns exactly one socket for its whole lifetime.
// The socket send size is tuned for small frequent writes.
// Reconnecting opens a fresh socket instead of reusing the old one.
// The test harness mocks the socket to avoid real network calls.

// The nightly job recomputes the balance for every open account.
// A stale balance triggers a reconciliation task automatically.
// The statement shows the balance at the start and end of the period.
// Support can manually adjust the balance with an approval step.
// The ledger view highlights any account with a negative balance.
// Rounding errors in the balance are logged for later review.

// Each entry in the log includes a timestamp and an account id.
// The index rebuilds one entry at a time during startup.
// A duplicate entry is skipped rather than causing an error.
// The last entry in the file marks the end of the current epoch.
// Every entry is validated against the schema before it is stored.
// An orphaned entry is removed during the next cleanup pass.
