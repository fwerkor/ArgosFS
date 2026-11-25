//! Coalesce overlapping normal reads of the same immutable stripe.
//! Scrub and encrypted reads deliberately bypass this short-lived registry.

use crate::error::Result;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

type Decoded = (Vec<u8>, Vec<String>);
type Flight = Mutex<Option<Decoded>>;

#[derive(Default)]
pub(super) struct DecodeFlights {
    entries: Mutex<BTreeMap<String, Weak<Flight>>>,
}

struct Lease<'a> {
    registry: &'a DecodeFlights,
    key: String,
    flight: Option<Arc<Flight>>,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let mut entries = self.registry.entries.lock();
        let flight = self.flight.take().expect("live flight lease");
        let last = Arc::strong_count(&flight) == 1;
        drop(flight);
        if last {
            entries.remove(&self.key);
        }
    }
}

impl DecodeFlights {
    pub(super) fn run(
        &self,
        key: &str,
        decode: impl FnOnce() -> Result<Decoded>,
    ) -> Result<Decoded> {
        let flight = {
            let mut entries = self.entries.lock();
            match entries.get(key).and_then(Weak::upgrade) {
                Some(flight) => flight,
                None => {
                    let flight = Arc::new(Mutex::new(None));
                    entries.insert(key.to_owned(), Arc::downgrade(&flight));
                    flight
                }
            }
        };
        let lease = Lease {
            registry: self,
            key: key.to_owned(),
            flight: Some(flight),
        };
        let flight = lease.flight.as_ref().expect("live flight lease");
        let mut completed = flight.lock();
        if let Some(result) = completed.as_ref() {
            return Ok(result.clone());
        }
        // Failed decodes are not memoized. A waiter may recover from a
        // transient I/O failure and the caller's normal repair path runs.
        let result = decode()?;
        // Do not copy the whole payload on the common uncontended path.
        // A late caller rechecks the verified block cache before decoding.
        if Arc::strong_count(flight) > 1 {
            *completed = Some(result.clone());
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;

    #[test]
    fn simultaneous_readers_share_payload_and_damage_without_retaining_entries() {
        let registry = DecodeFlights::default();
        let start = Barrier::new(8);
        let decodes = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    start.wait();
                    let result = registry
                        .run("stripe", || {
                            decodes.fetch_add(1, Ordering::Relaxed);
                            // Wait for all participants to acquire the same flight.
                            let deadline =
                                std::time::Instant::now() + std::time::Duration::from_secs(3);
                            loop {
                                let readers = registry.entries.lock()["stripe"].strong_count();
                                if readers == 8 {
                                    break;
                                }
                                assert!(std::time::Instant::now() < deadline);
                                std::thread::yield_now();
                            }
                            Ok((vec![7; 32], vec!["damaged:0".into()]))
                        })
                        .unwrap();
                    assert_eq!(result, (vec![7; 32], vec!["damaged:0".into()]));
                });
            }
        });
        assert_eq!(decodes.load(Ordering::Relaxed), 1);
        assert!(registry.entries.lock().is_empty());
    }

    #[test]
    fn failed_decode_is_not_retained_and_can_be_retried() {
        let registry = DecodeFlights::default();
        assert!(registry
            .run("stripe", || Err(crate::error::ArgosError::Invalid(
                "transient".into()
            )))
            .is_err());
        assert!(registry.entries.lock().is_empty());
        assert_eq!(
            registry
                .run("stripe", || Ok((vec![42], Vec::new())))
                .unwrap()
                .0,
            vec![42]
        );
        assert!(registry.entries.lock().is_empty());
    }
}
