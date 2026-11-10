//! Tempo eines laufenden Vorgangs: aus den Fortschrittsmeldungen der letzten Sekunden.

use std::collections::VecDeque;

/// So weit zurück reicht die Messung. Kürzer zappelt die Anzeige, länger hinkt sie hinterher.
const WINDOW_SECONDS: f64 = 6.0;
/// Erst ab dieser Messdauer wird ein Tempo genannt.
const MIN_SPAN_SECONDS: f64 = 1.0;

/// Merkt sich (Zeitpunkt, erledigte Bytes) und rechnet daraus Bytes pro Sekunde.
#[derive(Default)]
pub struct RateMeter {
    samples: VecDeque<(f64, u64)>,
}

impl RateMeter {
    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Nimmt einen Stand auf. `now` in Sekunden (beliebiger Nullpunkt, aber fortlaufend).
    pub fn add(&mut self, now: f64, done: u64) {
        // Läuft der Zähler rückwärts, hat ein neuer Abschnitt begonnen (z. B. Prüfen nach dem Packen).
        if self.samples.back().is_some_and(|(_, last)| done < *last) {
            self.samples.clear();
        }
        self.samples.push_back((now, done));
        while self.samples.len() > 2 && self.samples.front().is_some_and(|(time, _)| now - *time > WINDOW_SECONDS) {
            self.samples.pop_front();
        }
    }

    /// Bytes pro Sekunde über die letzten Sekunden; `None`, solange zu kurz gemessen wurde.
    pub fn per_second(&self) -> Option<f64> {
        let (first_time, first_done) = *self.samples.front()?;
        let (last_time, last_done) = *self.samples.back()?;
        let span = last_time - first_time;
        (span >= MIN_SPAN_SECONDS).then(|| last_done.saturating_sub(first_done) as f64 / span)
    }

    /// Voraussichtliche Restzeit in Sekunden bis `total`; `None`, wenn sie sich nicht schätzen lässt.
    pub fn seconds_left(&self, total: u64) -> Option<f64> {
        let speed = self.per_second().filter(|speed| *speed >= 1.0)?;
        let (_, done) = *self.samples.back()?;
        (total > done).then(|| (total - done) as f64 / speed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_speed_over_the_last_seconds() {
        let mut meter = RateMeter::default();
        assert_eq!(meter.per_second(), None);
        meter.add(0.0, 0);
        meter.add(0.5, 5_000_000);
        // Noch zu kurz gemessen.
        assert_eq!(meter.per_second(), None);
        meter.add(2.0, 20_000_000);
        assert_eq!(meter.per_second(), Some(10_000_000.0));
        assert_eq!(meter.seconds_left(120_000_000), Some(10.0));
        assert_eq!(meter.seconds_left(20_000_000), None);

        // Alte Werte fallen heraus: Jetzt zählt nur noch das langsame Tempo der letzten Sekunden.
        for second in 3..=12 {
            meter.add(second as f64, 20_000_000 + (second - 2) * 1_000_000);
        }
        assert_eq!(meter.per_second(), Some(1_000_000.0));
    }

    #[test]
    fn starts_over_when_a_new_phase_begins() {
        let mut meter = RateMeter::default();
        meter.add(0.0, 0);
        meter.add(2.0, 50_000_000);
        // Der Zähler springt zurück (neuer Abschnitt): nicht mit negativem Tempo rechnen.
        meter.add(2.5, 1_000);
        assert_eq!(meter.per_second(), None);
        meter.add(4.5, 2_001_000);
        assert_eq!(meter.per_second(), Some(1_000_000.0));
        // Steht der Vorgang, gibt es keine Restzeit.
        let mut idle = RateMeter::default();
        idle.add(0.0, 10);
        idle.add(3.0, 10);
        assert_eq!((idle.per_second(), idle.seconds_left(100)), (Some(0.0), None));
    }
}
