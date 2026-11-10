//! Kommunikation zwischen Hintergrund-Thread (Backup, Upload, Wiederherstellung) und Oberfläche.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// Ab so vielen Warnungen werden weitere nicht mehr einzeln ins Protokoll geschrieben.
const MAX_WARNINGS: usize = 200;

pub enum Msg {
    Status(String),
    Progress { done: u64, total: u64 },
    Log(String),
    Warn(String),
}

/// Fehler-Typ, der einen Abbruch durch den Benutzer kennzeichnet.
#[derive(Debug)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(t!("Vorgang abgebrochen", "Operation cancelled"))
    }
}

impl std::error::Error for Cancelled {}

/// Wird an die Arbeits-Threads gereicht: meldet Status/Fortschritt und kennt das Abbruch-Flag.
#[derive(Clone)]
pub struct Reporter {
    tx: mpsc::Sender<Msg>,
    cancel: Arc<AtomicBool>,
    warnings: Arc<AtomicUsize>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Reporter {
    /// `wake` wird nach jeder Nachricht aufgerufen (z. B. um die Oberfläche neu zu zeichnen).
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> (Self, mpsc::Receiver<Msg>, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let reporter = Self {
            tx,
            cancel: cancel.clone(),
            warnings: Arc::new(AtomicUsize::new(0)),
            wake: Arc::new(wake),
        };
        (reporter, rx, cancel)
    }

    fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
        (self.wake)();
    }

    pub fn status(&self, text: impl Into<String>) {
        self.send(Msg::Status(text.into()));
    }

    pub fn log(&self, text: impl Into<String>) {
        self.send(Msg::Log(text.into()));
    }

    pub fn warn(&self, text: impl Into<String>) {
        let n = self.warnings.fetch_add(1, Ordering::Relaxed);
        if n < MAX_WARNINGS {
            self.send(Msg::Warn(text.into()));
        } else if n == MAX_WARNINGS {
            self.send(Msg::Warn(
                t!(
                    "Weitere Warnungen werden nicht mehr einzeln angezeigt.",
                    "Further warnings are no longer shown individually."
                )
                .into(),
            ));
        }
    }

    pub fn progress(&self, done: u64, total: u64) {
        self.send(Msg::Progress { done, total });
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Gibt `Err(Cancelled)` zurück, sobald der Benutzer abgebrochen hat.
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.cancelled() { Err(Cancelled) } else { Ok(()) }
    }
}

/// Zählt verarbeitete Bytes und meldet den Fortschritt gedrosselt an die Oberfläche.
pub struct Counter {
    rep: Reporter,
    done: u64,
    total: u64,
    last: Instant,
}

impl Counter {
    pub fn new(rep: &Reporter, total: u64) -> Self {
        rep.progress(0, total);
        Self { rep: rep.clone(), done: 0, total, last: Instant::now() }
    }

    pub fn add(&mut self, n: u64) {
        self.done = self.done.saturating_add(n);
        if self.last.elapsed() >= Duration::from_millis(60) {
            self.flush();
        }
    }

    pub fn done(&self) -> u64 {
        self.done
    }

    /// Setzt den Zähler zurück (z. B. vor dem erneuten Senden eines Upload-Teils).
    pub fn set(&mut self, done: u64) {
        self.done = done;
        self.flush();
    }

    pub fn flush(&mut self) {
        self.last = Instant::now();
        self.rep.progress(self.done.min(self.total), self.total);
    }
}
