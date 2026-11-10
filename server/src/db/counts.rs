//! What a pool has run, so tests can measure an operation's work rather than
//! its time. Counting costs an atomic increment or two per statement.
use std::sync::atomic::{AtomicU64, Ordering};

/// What a pool has run since it opened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub statements: u64,
    /// Statements run for their effect (`execute`). On SQLite each one
    /// outside a transaction takes the write lock.
    pub writes: u64,
    /// Transactions begun. On SQLite each takes the write lock.
    pub transactions: u64,
}

impl std::ops::Sub for Counts {
    type Output = Self;

    fn sub(self, earlier: Self) -> Self {
        Self {
            statements: self.statements - earlier.statements,
            writes: self.writes - earlier.writes,
            transactions: self.transactions - earlier.transactions,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct Counters {
    statements: AtomicU64,
    writes: AtomicU64,
    transactions: AtomicU64,
    rows: AtomicU64,
}

impl Counters {
    pub(super) fn read(&self) {
        self.statements.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn write(&self) {
        self.statements.fetch_add(1, Ordering::Relaxed);
        self.writes.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn returned(&self, rows: usize) {
        self.rows
            .fetch_add(u64::try_from(rows).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    pub(super) fn transaction(&self) {
        self.transactions.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn counts(&self) -> Counts {
        Counts {
            statements: self.statements.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            transactions: self.transactions.load(Ordering::Relaxed),
        }
    }

    pub(super) fn rows(&self) -> u64 {
        self.rows.load(Ordering::Relaxed)
    }
}
