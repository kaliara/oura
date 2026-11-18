//! Ring time → wall-clock time.
//!
//! A history event's timestamp is a per-boot decisecond counter (`ring_timestamp`).
//! `time_sync` (`0x42`) and `rtc_beacon` (`0x85`) events are the authoritative bridge
//! from that counter to UTC; `captured_unix` (when the host downloaded the event) is
//! only an epoch-selection hint and a last-resort fallback.
//!
//! Ported from open_health `crates/oura-summary/src/ring_time.rs` (MIT, Thomas
//! Marchand), which handles reboots, a counter that stalled while the ring was off, a
//! counter that ran faster than wall time, and replayed old boots. Two changes:
//!
//! - Anchors are read from the raw event body, so dating does not depend on which
//!   decoder version wrote `decoded_json`.
//! - Each anchor also carries the UTC offset the host sent with the time sync (signed
//!   half hours, see `OuraClient::sync_time`). An event takes the offset in effect at
//!   its wall-clock time (the latest anchor at or before it), so local time follows
//!   daylight-saving changes and travel, one sync late at worst.

/// `ring_start`: a reboot record. A brownout reboot keeps counting, so this marks
/// where a stalled counter lost its time.
pub const RING_START_TAG: u8 = 0x41;
/// `time_sync`: the ring's echo of a host time sync (or a synthetic phone anchor).
pub const TIME_SYNC_TAG: u8 = 0x42;
/// `rtc_beacon`: the ring's own 1-second wall-clock anchor.
pub const RTC_BEACON_TAG: u8 = 0x85;

/// Epochs are split when the counter jumps back by more than this (a reboot).
const RESET_SLACK_DS: i64 = 6 * 3600 * 10;
// Two anchors of one boot normally agree on the counter rate (10 ds per second, plus
// drift). When the counter *stalled* between them (the ring lost hours while off), the
// wall clock advanced more than the counter and the later anchor's offset applies from
// the stall on; the download time tells which side of the stall an event sits on. When
// the counter ran *faster* than wall time (a fresh ring's first days jumped weeks of ds
// in an hour), nothing between the two anchors can be placed on the calendar.
// Half an hour absorbs RTC drift and the second-granular anchors; 2 % covers long gaps.
const ANCHOR_AGREEMENT_S: f64 = 30.0 * 60.0;
const ANCHOR_AGREEMENT_FRACTION: f64 = 0.02;
// A history event cannot legitimately occur well after the host captured it. A few
// hours tolerate clock corrections/timezone setup without allowing a replayed
// pre-reboot high ds value to fabricate weeks of future data.
const FUTURE_SLACK_S: f64 = 6.0 * 3600.0;

/// Decode the time-sync UTC offset byte (signed half hours) into minutes. Offsets
/// outside UTC-12..UTC+14 are rejected as garbage.
pub fn utc_offset_minutes(half_hours: u8) -> Option<i32> {
    let half_hours = half_hours as i8 as i32;
    (-24..=28).contains(&half_hours).then_some(half_hours * 30)
}

/// Where an anchor came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AnchorSource {
    /// The ring's echo of a host time sync.
    TimeSync,
    /// The ring's own RTC beacon.
    RtcBeacon,
    /// A synthetic anchor recorded by a phone at the end of a sync.
    Phone,
}

impl AnchorSource {
    pub fn label(self) -> &'static str {
        match self {
            AnchorSource::TimeSync => "time_sync",
            AnchorSource::RtcBeacon => "rtc_beacon",
            AnchorSource::Phone => "phone",
        }
    }
}

/// A wall-clock anchor parsed from a `time_sync` or `rtc_beacon` body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockAnchor {
    pub unix: i64,
    pub utc_offset_min: Option<i32>,
    pub source: AnchorSource,
}

/// Parse an anchor from a raw event body. `time_sync` bodies are the host's request
/// echoed back: u64 LE unix seconds then the UTC-offset byte (or u32 unix + `phone`
/// for a synthetic phone anchor). `rtc_beacon` bodies are u64 LE unix seconds, the
/// UTC-offset byte, then a flags byte.
pub fn parse_anchor(tag: u8, body: &[u8]) -> Option<ClockAnchor> {
    let unix = || u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as i64;
    match tag {
        TIME_SYNC_TAG if body.len() >= 4 => {
            if body.get(4..) == Some(b"phone") {
                return Some(ClockAnchor {
                    unix: unix(),
                    utc_offset_min: None,
                    source: AnchorSource::Phone,
                });
            }
            Some(ClockAnchor {
                unix: unix(),
                utc_offset_min: body.get(8).copied().and_then(utc_offset_minutes),
                source: AnchorSource::TimeSync,
            })
        }
        RTC_BEACON_TAG if body.len() >= 10 => Some(ClockAnchor {
            unix: unix(),
            utc_offset_min: utc_offset_minutes(body[8]),
            source: AnchorSource::RtcBeacon,
        }),
        _ => None,
    }
}

/// One stored event as the clock needs it. `body` only matters for anchor tags.
#[derive(Clone, Copy, Debug)]
pub struct ClockEvent<'a> {
    pub ring_ds: i64,
    pub tag: u8,
    pub body: &'a [u8],
    pub captured_unix: i64,
}

/// Why an event could not be placed on the calendar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UndatedReason {
    /// The boot epoch has no `time_sync` or `rtc_beacon` anchor (and was not
    /// incrementally drained across syncs).
    MissingAnchor,
    /// Two anchors in the same boot epoch disagree because the decisecond counter
    /// advanced faster than wall-clock time between them, or the counter jumped
    /// ahead of capture time relative to its anchor.
    AcceleratedCounter,
    /// Multiple `ring_start` reboots occurred inside a stalled-anchor interval where
    /// wall time exceeded counter time, so lost time cannot be partitioned.
    AmbiguousRebootStall,
}

impl UndatedReason {
    pub fn code(self) -> &'static str {
        match self {
            UndatedReason::MissingAnchor => "missing_anchor",
            UndatedReason::AcceleratedCounter => "accelerated_counter",
            UndatedReason::AmbiguousRebootStall => "ambiguous_reboot_stall",
        }
    }
}

/// How an event's wall-clock time was obtained. Only `Anchor` and `Projected` are
/// trustworthy to the minute; `Fallback` is download-time arithmetic (off by up to
/// one sync gap) and `Undated` means nothing ties this boot to real time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockSource {
    Anchor,
    Projected,
    Fallback,
    Undated,
}

impl ClockSource {
    pub fn is_dated(self) -> bool {
        matches!(self, ClockSource::Anchor | ClockSource::Projected)
    }

    pub fn label(self) -> &'static str {
        match self {
            ClockSource::Anchor => "anchor",
            ClockSource::Projected => "projected",
            ClockSource::Fallback => "download_time",
            ClockSource::Undated => "undated",
        }
    }
}

/// An event's resolved wall-clock time.
#[derive(Clone, Copy, Debug)]
pub struct Resolved {
    /// UTC unix seconds (decisecond precision).
    pub unix: f64,
    pub source: ClockSource,
    pub undated_reason: Option<UndatedReason>,
    /// UTC offset in effect at `unix`; `None` when undated or no anchor carried one.
    pub utc_offset_min: Option<i32>,
    /// Index of the boot epoch the event belongs to.
    pub epoch: usize,
}

/// Per-boot diagnostics.
#[derive(Clone, Debug, PartialEq)]
pub struct EpochSummary {
    pub min_ds: i64,
    pub max_ds: i64,
    pub capture_min: i64,
    pub capture_max: i64,
    pub anchors: usize,
    pub anchor_sources: Vec<&'static str>,
    pub utc_offsets_min: Vec<i32>,
    pub boots: usize,
    pub latest_anchor_unix: Option<i64>,
}

#[derive(Clone, Debug)]
struct Epoch {
    min_ds: i64,
    max_ds: i64,
    capture_min: i64,
    capture_max: i64,
    fallback_anchor_unix: i64,
    anchors: Vec<(i64, i64)>, // (ring ds, UTC unix seconds), sorted by ds
    anchor_sources: Vec<AnchorSource>,
    anchor_offsets_min: Vec<i32>,
    // `ring_start` ds values: a brownout reboot keeps counting, so this is where a
    // stalled counter lost its time.
    boots: Vec<i64>,
}

enum Bracket {
    Consistent,
    /// The counter lost time between the two anchors (ring powered off).
    Stalled {
        before: (i64, i64),
        after: (i64, i64),
    },
    /// The counter advanced faster than wall time: untrustworthy.
    Erratic,
}

/// Maps the ring's rebooting relative clock onto UTC (and the host's local offset).
pub struct RingClock {
    epochs: Vec<Epoch>,
    // `(unix * 10 - ring_ds, epoch index)`: the UTC projection offset in deciseconds
    // of every anchor, sorted by offset so replay recovery can find the newest
    // plausible projection in O(log n) instead of scanning every anchor per event.
    anchor_offsets_ds: Vec<(i64, usize)>,
    // `(anchor unix, UTC offset minutes)` for every anchor that carried an offset,
    // sorted by unix: the zone in effect from that moment on.
    zones: Vec<(i64, i32)>,
}

impl RingClock {
    /// Build the clock from events in capture order (`ORDER BY captured_unix, id`).
    /// The id tie-breaker matters: a full-history drain inserts thousands of events
    /// in the same second, including the backward jump at a reboot.
    pub fn from_events<'a, I: IntoIterator<Item = ClockEvent<'a>>>(events: I) -> Self {
        let mut epochs: Vec<Epoch> = Vec::new();
        let mut zones: Vec<(i64, i32)> = Vec::new();
        for ev in events {
            let (ds, captured) = (ev.ring_ds, ev.captured_unix);
            match epochs.last_mut() {
                Some(e) if ds >= e.max_ds - RESET_SLACK_DS => {
                    if ds >= e.max_ds {
                        e.max_ds = ds;
                        e.fallback_anchor_unix = captured;
                    }
                    e.min_ds = e.min_ds.min(ds);
                    e.capture_min = e.capture_min.min(captured);
                    e.capture_max = e.capture_max.max(captured);
                }
                _ => epochs.push(Epoch {
                    min_ds: ds,
                    max_ds: ds,
                    capture_min: captured,
                    capture_max: captured,
                    fallback_anchor_unix: captured,
                    anchors: Vec::new(),
                    anchor_sources: Vec::new(),
                    anchor_offsets_min: Vec::new(),
                    boots: Vec::new(),
                }),
            }
            let e = epochs.last_mut().expect("just pushed or matched");
            if ev.tag == RING_START_TAG {
                e.boots.push(ds);
            }
            if let Some(anchor) = parse_anchor(ev.tag, ev.body) {
                e.anchors.push((ds, anchor.unix));
                e.anchor_sources.push(anchor.source);
                if let Some(offset) = anchor.utc_offset_min {
                    e.anchor_offsets_min.push(offset);
                    zones.push((anchor.unix, offset));
                }
            }
        }
        for epoch in &mut epochs {
            epoch.anchors.sort_unstable();
        }
        let mut anchor_offsets_ds = epochs
            .iter()
            .enumerate()
            .flat_map(|(idx, epoch)| {
                epoch
                    .anchors
                    .iter()
                    .map(move |(ds, unix)| (unix.saturating_mul(10).saturating_sub(*ds), idx))
            })
            .collect::<Vec<_>>();
        anchor_offsets_ds.sort_unstable();
        anchor_offsets_ds.dedup_by_key(|(offset, _)| *offset);
        zones.sort_unstable();
        Self {
            epochs,
            anchor_offsets_ds,
            zones,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.epochs.is_empty()
    }

    pub fn unix_s(&self, ds: i64, captured_unix: i64) -> f64 {
        self.resolve(ds, captured_unix).unix
    }

    /// Resolve one event's wall-clock time and the UTC offset in effect then.
    pub fn resolve(&self, ds: i64, captured_unix: i64) -> Resolved {
        let mut r = self.resolve_utc(ds, captured_unix);
        if r.source != ClockSource::Undated {
            r.utc_offset_min = self.zone_at(r.unix);
        }
        r
    }

    fn resolve_utc(&self, ds: i64, captured_unix: i64) -> Resolved {
        let Some((idx, epoch)) = self.epoch_for(ds, captured_unix) else {
            return Resolved {
                unix: captured_unix as f64,
                source: ClockSource::Undated,
                undated_reason: Some(UndatedReason::MissingAnchor),
                utc_offset_min: None,
                epoch: 0,
            };
        };
        let resolved = |unix, source, undated_reason| Resolved {
            unix,
            source,
            undated_reason,
            utc_offset_min: None,
            epoch: idx,
        };
        let mut had_in_epoch_anchor = false;
        if let Some((anchor_ds, anchor_unix)) = epoch
            .anchors
            .iter()
            .min_by_key(|(a, _)| (*a as i128 - ds as i128).unsigned_abs())
        {
            had_in_epoch_anchor = true;
            let predicted = *anchor_unix as f64 + (ds - *anchor_ds) as f64 / 10.0;
            match Self::bracket(&epoch.anchors, ds) {
                Bracket::Erratic => {
                    // Any prediction would scatter the data across fabricated days.
                    return resolved(
                        predicted,
                        ClockSource::Undated,
                        Some(UndatedReason::AcceleratedCounter),
                    );
                }
                Bracket::Stalled { before, after } => {
                    let late = after.1 as f64 - (after.0 - ds) as f64 / 10.0;
                    let early = before.1 as f64 + (ds - before.0) as f64 / 10.0;
                    // A ring_start between the anchors marks the stall exactly; without
                    // one, the download time tells which side an event sits on (wrong
                    // when pre-stall events were only downloaded after the stall).
                    let boots: Vec<_> = epoch
                        .boots
                        .iter()
                        .copied()
                        .filter(|b| *b > before.0 && *b <= after.0)
                        .collect();
                    let first = boots.iter().min().copied();
                    let boot = boots.iter().max().copied();
                    // Between multiple reboots we cannot locate the lost time.
                    if first
                        .zip(boot)
                        .is_some_and(|(first, last)| ds >= first && ds < last)
                    {
                        return resolved(
                            predicted,
                            ClockSource::Undated,
                            Some(UndatedReason::AmbiguousRebootStall),
                        );
                    }
                    let unix = match boot {
                        Some(boot) if ds >= boot => late,
                        Some(_) => early,
                        None if late <= captured_unix as f64 + FUTURE_SLACK_S => late,
                        None => early,
                    };
                    return resolved(unix, ClockSource::Anchor, None);
                }
                Bracket::Consistent => {}
            }
            if predicted <= captured_unix as f64 + FUTURE_SLACK_S {
                return resolved(predicted, ClockSource::Anchor, None);
            }
        }

        if had_in_epoch_anchor {
            // A cursor rebase can replay an old boot after the newer boot was already
            // stored. Duplicate anchors are ignored by SQLite, while previously unseen
            // high-ds events are appended at today's capture time and can look like a
            // continuation of the new boot. If that epoch predicts the future, select
            // the most recent globally plausible time-sync projection instead.
            if let Some(predicted) = self.latest_plausible_projection(ds, captured_unix) {
                return resolved(predicted, ClockSource::Projected, None);
            }
            let unix = (epoch.fallback_anchor_unix as f64 - (epoch.max_ds - ds) as f64 / 10.0)
                .min(captured_unix as f64 + FUTURE_SLACK_S);
            return resolved(
                unix,
                ClockSource::Undated,
                Some(UndatedReason::AcceleratedCounter),
            );
        }

        // Download-time arithmetic is only meaningful when the host kept up with the
        // ring: a boot drained sync after sync has a capture span comparable to its ds
        // span, so the error is bounded by one sync gap. A boot downloaded in one go
        // (a fresh ring, a from-zero replay, an old boot) would simply be dated to the
        // moment of the download, so it stays undated instead.
        let ds_span_s = (epoch.max_ds - epoch.min_ds) as f64 / 10.0;
        let capture_span_s = (epoch.capture_max - epoch.capture_min) as f64;
        let incremental = ds_span_s <= 0.0 || capture_span_s * 2.0 >= ds_span_s;
        let unix = (epoch.fallback_anchor_unix as f64 - (epoch.max_ds - ds) as f64 / 10.0)
            .min(captured_unix as f64 + FUTURE_SLACK_S);
        if incremental {
            resolved(unix, ClockSource::Fallback, None)
        } else {
            resolved(
                unix,
                ClockSource::Undated,
                Some(UndatedReason::MissingAnchor),
            )
        }
    }

    /// The newest anchor time, or the newest download time when nothing is anchored.
    pub fn latest_unix(&self) -> Option<i64> {
        self.epochs
            .iter()
            .flat_map(|e| e.anchors.iter().map(|(_, unix)| *unix))
            .max()
            .or_else(|| self.epochs.iter().map(|e| e.fallback_anchor_unix).max())
    }

    /// Per-boot diagnostics.
    pub fn epochs(&self) -> Vec<EpochSummary> {
        self.epochs
            .iter()
            .map(|e| {
                let mut sources: Vec<_> = e.anchor_sources.iter().map(|s| s.label()).collect();
                sources.sort_unstable();
                sources.dedup();
                let mut offsets = e.anchor_offsets_min.clone();
                offsets.sort_unstable();
                offsets.dedup();
                EpochSummary {
                    min_ds: e.min_ds,
                    max_ds: e.max_ds,
                    capture_min: e.capture_min,
                    capture_max: e.capture_max,
                    anchors: e.anchors.len(),
                    anchor_sources: sources,
                    utc_offsets_min: offsets,
                    boots: e.boots.len(),
                    latest_anchor_unix: e.anchors.iter().map(|(_, u)| *u).max(),
                }
            })
            .collect()
    }

    /// The UTC offset in effect at `unix`: the latest anchor at or before it, or the
    /// earliest anchor when the event predates every offset-bearing anchor.
    fn zone_at(&self, unix: f64) -> Option<i32> {
        let idx = self
            .zones
            .partition_point(|(anchor_unix, _)| (*anchor_unix as f64) <= unix);
        idx.checked_sub(1)
            .and_then(|i| self.zones.get(i))
            .or_else(|| self.zones.first())
            .map(|(_, offset)| *offset)
    }

    /// How the anchors on either side of `ds` (sorted by ds) relate. Outside the
    /// anchored range the single nearest anchor extrapolates as usual: that is how an
    /// ordinary boot's first hours are dated.
    fn bracket(anchors: &[(i64, i64)], ds: i64) -> Bracket {
        let idx = anchors.partition_point(|(a, _)| *a < ds);
        let (Some(&next), Some(prev)) = (anchors.get(idx), idx.checked_sub(1).map(|i| anchors[i]))
        else {
            return Bracket::Consistent;
        };
        if next.0 == ds {
            return Bracket::Consistent;
        }
        let wall_s = (next.1 - prev.1) as f64;
        let counter_s = (next.0 - prev.0) as f64 / 10.0;
        let tolerance = ANCHOR_AGREEMENT_S.max(counter_s * ANCHOR_AGREEMENT_FRACTION);
        if wall_s < counter_s - tolerance {
            Bracket::Erratic
        } else if wall_s > counter_s + tolerance {
            Bracket::Stalled {
                before: prev,
                after: next,
            }
        } else {
            Bracket::Consistent
        }
    }

    fn epoch_for(&self, ds: i64, captured_unix: i64) -> Option<(usize, &Epoch)> {
        self.epochs
            .iter()
            .enumerate()
            .filter(|(_, e)| ds >= e.min_ds - RESET_SLACK_DS && ds <= e.max_ds + RESET_SLACK_DS)
            .min_by_key(|(_, e)| {
                if captured_unix < e.capture_min {
                    (e.capture_min - captured_unix) as u64
                } else if captured_unix > e.capture_max {
                    (captured_unix - e.capture_max) as u64
                } else {
                    0
                }
            })
            .or_else(|| self.epochs.iter().enumerate().next_back())
    }

    fn latest_plausible_projection(&self, ds: i64, captured_unix: i64) -> Option<f64> {
        let max_offset = captured_unix
            .saturating_add(FUTURE_SLACK_S as i64)
            .saturating_mul(10)
            .saturating_sub(ds);
        let end = self
            .anchor_offsets_ds
            .partition_point(|(offset, _)| *offset <= max_offset);
        // Borrowing another boot's clock is only legitimate when this ds continues
        // that boot's counter. A new boot restarts near zero, so a low ds must never
        // be projected through an older boot that only ever ran at higher counts, and
        // a counter jump far beyond an older boot's max_ds must not borrow that older
        // boot either.
        self.anchor_offsets_ds[..end]
            .iter()
            .rev()
            .find(|(_, idx)| {
                let ep = &self.epochs[*idx];
                ds >= ep.min_ds - RESET_SLACK_DS && ds <= ep.max_ds + RESET_SLACK_DS
            })
            .map(|(offset, _)| ds.saturating_add(*offset) as f64 / 10.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EST: u8 = 0xF6; // -10 half hours = UTC-05:00
    const EDT: u8 = 0xF8; // -8 half hours = UTC-04:00

    /// A host time-sync echo: u64 LE unix seconds then the UTC-offset byte.
    fn ts(unix: i64, tz: u8) -> Vec<u8> {
        let mut body = (unix as u64).to_le_bytes().to_vec();
        body.push(tz);
        body
    }

    fn rtc(unix: i64, tz: u8) -> Vec<u8> {
        let mut body = (unix as u64).to_le_bytes().to_vec();
        body.extend_from_slice(&[tz, 0x01]);
        body
    }

    fn phone(unix: i64) -> Vec<u8> {
        let mut body = (unix as u32).to_le_bytes().to_vec();
        body.extend_from_slice(b"phone");
        body
    }

    type Row = (i64, u8, Vec<u8>, i64);

    fn ev(ds: i64, tag: u8, captured: i64) -> Row {
        (ds, tag, Vec::new(), captured)
    }

    fn anchor(ds: i64, unix: i64, captured: i64) -> Row {
        (ds, TIME_SYNC_TAG, ts(unix, 0x00), captured)
    }

    fn clock(rows: &[Row]) -> RingClock {
        RingClock::from_events(rows.iter().map(|(ds, tag, body, captured)| ClockEvent {
            ring_ds: *ds,
            tag: *tag,
            body,
            captured_unix: *captured,
        }))
    }

    #[test]
    fn parses_real_ring5_anchor_bodies() {
        // Captured from a Ring 5 (COR_08, firmware 2.2.2) synced from UTC-05:00.
        let sync = parse_anchor(0x42, &hex::decode("DA72FC6A00000000F6").unwrap()).unwrap();
        assert_eq!(
            sync,
            ClockAnchor {
                unix: 1_794_929_370,
                utc_offset_min: Some(-300),
                source: AnchorSource::TimeSync,
            }
        );
        let beacon = parse_anchor(0x85, &hex::decode("4C75FC6A00000000F601").unwrap()).unwrap();
        assert_eq!(beacon.unix, 1_794_929_996);
        assert_eq!(beacon.utc_offset_min, Some(-300));
        assert_eq!(beacon.source, AnchorSource::RtcBeacon);
        let p = parse_anchor(0x42, &phone(1_787_733_221)).unwrap();
        assert_eq!(p.source, AnchorSource::Phone);
        assert_eq!(p.utc_offset_min, None);
        assert!(parse_anchor(0x85, &[0; 9]).is_none());
        assert!(parse_anchor(0x43, &ts(1, EST)).is_none());
    }

    #[test]
    fn utc_offset_byte_is_signed_half_hours() {
        assert_eq!(utc_offset_minutes(0xF6), Some(-300));
        assert_eq!(utc_offset_minutes(0x00), Some(0));
        assert_eq!(utc_offset_minutes(0x0B), Some(330)); // India, UTC+05:30
        assert_eq!(utc_offset_minutes(0x1C), Some(840)); // Kiribati, UTC+14:00
        assert_eq!(utc_offset_minutes(0xE8), Some(-720)); // UTC-12:00
        assert_eq!(utc_offset_minutes(0x7F), None);
        assert_eq!(utc_offset_minutes(0x80), None);
    }

    #[test]
    fn dates_the_real_ring5_history_in_local_time() {
        // The first sync after a factory reset: three host time syncs and one RTC
        // beacon, all from UTC-05:00, plus the reboot record.
        let rows = vec![
            ev(1_367, 0x43, 1_794_929_612),
            (1_379, 0x42, hex::decode("DA72FC6A00000000F6").unwrap(), 1_794_929_612),
            ev(1_380, RING_START_TAG, 1_794_929_612),
            (3_779, 0x42, hex::decode("C973FC6A00000000F6").unwrap(), 1_794_929_612),
            (7_648, 0x85, hex::decode("4C75FC6A00000000F601").unwrap(), 1_794_929_998),
            (10_389, 0x42, hex::decode("5E76FC6A00000000F6").unwrap(), 1_794_930_272),
            ev(10_412, 0x43, 1_794_930_273),
        ];
        let clock = clock(&rows);
        assert_eq!(clock.epochs().len(), 1);
        let first = clock.resolve(1_367, 1_794_929_612);
        assert_eq!(first.source, ClockSource::Anchor);
        assert!((first.unix - 1_794_929_368.8).abs() < 1e-6);
        assert_eq!(first.utc_offset_min, Some(-300));
        let last = clock.resolve(10_412, 1_794_930_273);
        assert_eq!(last.source, ClockSource::Anchor);
        assert!((last.unix - 1_794_930_272.3).abs() < 1e-6);
        assert_eq!(clock.latest_unix(), Some(1_794_930_270));
        let summary = &clock.epochs()[0];
        assert_eq!(summary.anchors, 4);
        assert_eq!(summary.anchor_sources, vec!["rtc_beacon", "time_sync"]);
        assert_eq!(summary.utc_offsets_min, vec![-300]);
        assert_eq!(summary.boots, 1);
    }

    #[test]
    fn local_offset_follows_daylight_saving_at_the_next_anchor() {
        // Synced in EST, then the clocks spring forward and the next sync is in EDT.
        let t0 = 1_804_000_000;
        let t1 = t0 + 2 * 86_400;
        let rows = vec![
            (10_000, 0x42, ts(t0, EST), t0),
            ev(10_000 + 864_000, 0x43, t1),
            (10_000 + 1_728_000, 0x42, ts(t1, EDT), t1),
            ev(10_000 + 1_800_000, 0x43, t1 + 7_200),
        ];
        let clock = clock(&rows);
        assert_eq!(clock.resolve(10_000 + 864_000, t1).utc_offset_min, Some(-300));
        assert_eq!(
            clock.resolve(10_000 + 1_800_000, t1 + 7_200).utc_offset_min,
            Some(-240)
        );
        // Before the first anchor the earliest known zone applies.
        assert_eq!(clock.resolve(5_000, t0).utc_offset_min, Some(-300));
    }

    #[test]
    fn undated_events_carry_no_offset_and_empty_clock_is_undated() {
        let clock = clock(&[ev(100_000, 1, 1_789_056_420), ev(700_000, 1, 1_789_056_420)]);
        let r = clock.resolve(400_000, 1_789_056_420);
        assert_eq!(r.source, ClockSource::Undated);
        assert_eq!(r.utc_offset_min, None);
        let empty = RingClock::from_events(std::iter::empty());
        assert!(empty.is_empty());
        let r = empty.resolve(5, 1_000);
        assert_eq!(r.undated_reason, Some(UndatedReason::MissingAnchor));
        assert_eq!(empty.latest_unix(), None);
    }

    #[test]
    fn phone_anchor_dates_but_borrows_the_zone_from_ring_anchors() {
        let t0 = 1_789_000_000;
        let rows = vec![
            (5_000, 0x42, ts(t0, EST), t0),
            (905_000, 0x42, phone(t0 + 90_000), t0 + 90_000),
            ev(910_000, 0x43, t0 + 90_000),
        ];
        let clock = clock(&rows);
        let r = clock.resolve(910_000, t0 + 90_000);
        assert_eq!(r.source, ClockSource::Anchor);
        assert_eq!(r.utc_offset_min, Some(-300));
    }

    // ---- Ported from open_health ring_time.rs ----

    #[test]
    fn multiple_reboots_leave_middle_undated() {
        let clock = clock(&[
            anchor(1000, 1700000000, 1700000000),
            ev(2000, 0x41, 1700100000),
            ev(8000, 0x41, 1700100000),
            anchor(10000, 1700100000, 1700100000),
        ]);
        assert_eq!(clock.resolve(5000, 1700100000).source, ClockSource::Undated);
        assert_eq!(clock.resolve(1500, 1700100000).unix, 1700000050.0);
        assert_eq!(clock.resolve(9000, 1700100000).unix, 1700099900.0);
    }

    #[test]
    fn erratic_counter_between_disagreeing_anchors_is_undated() {
        // A fresh ring: the counter advanced 28 hours of ds in one wall-clock hour
        // between two time syncs, then ran normally between the next two.
        let clock = clock(&[
            ev(20_000, 1, 1_783_000_000),
            anchor(20_928, 1782939604, 1_783_000_000),
            ev(500_000, 1, 1_783_000_000),
            (1_032_193, 0x85, rtc(1782943316, 0x00), 1_783_000_000),
            ev(1_100_000, 1, 1_783_000_000),
            (1_133_000, 0x85, rtc(1782953397, 0x00), 1_783_000_000),
            ev(1_200_000, 1, 1_783_000_000),
        ]);
        assert_eq!(
            clock.resolve(500_000, 1_783_000_000).source,
            ClockSource::Undated
        );
        // Inside the healthy pocket the nearest anchor dates the event as usual.
        let inside = clock.resolve(1_100_000, 1_783_000_000);
        assert_eq!(inside.source, ClockSource::Anchor);
        assert!((inside.unix - (1_782_953_397.0 - 3_300.0)).abs() < 0.01);
        // Past the last anchor, extrapolation from that anchor still applies.
        assert_eq!(
            clock.resolve(1_200_000, 1_783_000_000).source,
            ClockSource::Anchor
        );
        assert_eq!(
            clock.resolve(20_000, 1_783_000_000).source,
            ClockSource::Anchor
        );
    }

    #[test]
    fn stalled_counter_uses_download_time_to_pick_the_anchor_side() {
        // The ring lost 40 h while off between two syncs: anchors 15 days apart in ds,
        // 17 days apart in wall time. An event downloaded before the later anchor's
        // projection would allow keeps the earlier offset; one downloaded later takes
        // the later offset.
        let before = (47_893_458_i64, 1_787_733_180_i64);
        let after = (61_076_535_i64, 1_789_195_380_i64);
        let clock = clock(&[
            anchor(before.0, before.1, before.1 + 60),
            ev(52_000_000, 1, 1_788_100_000),
            anchor(after.0, after.1, after.1 + 60),
        ]);
        let early = clock.resolve(52_000_000, 1_788_100_000);
        assert_eq!(early.source, ClockSource::Anchor);
        assert!(
            (early.unix - (before.1 as f64 + (52_000_000 - before.0) as f64 / 10.0)).abs() < 0.01
        );
        let late = clock.resolve(52_000_000, after.1 + 60);
        assert_eq!(late.source, ClockSource::Anchor);
        assert!((late.unix - (after.1 as f64 - (after.0 - 52_000_000) as f64 / 10.0)).abs() < 0.01);
    }

    #[test]
    fn rtc_beacon_dates_overnight_sleep_independently_of_download_time() {
        // 22:01 Sep 9 -> 07:07 Sep 10 in UTC+1, downloaded ten hours later.
        let clock = clock(&[
            ev(672_400, 1, 1_789_056_420),
            (1_000_000, 0x85, rtc(1789020420, 0x02), 1_789_056_420),
        ]);
        assert_eq!(clock.unix_s(672_400, 1_789_056_420), 1_788_987_660.0);
        assert_eq!(clock.unix_s(1_000_000, 1_789_056_420), 1_789_020_420.0);
        assert_eq!(clock.latest_unix(), Some(1_789_020_420));
        assert_eq!(clock.resolve(672_400, 1_789_056_420).utc_offset_min, Some(60));
    }

    #[test]
    fn rtc_beacon_anchors_new_boot_instead_of_reusing_old_time_sync() {
        let clock = clock(&[
            anchor(5_000_000, 1788800000, 1_788_800_000),
            ev(672_400, 1, 1_789_056_420),
            (1_000_000, 0x85, rtc(1789020420, 0x00), 1_789_056_420),
        ]);
        assert_eq!(clock.unix_s(672_400, 1_789_056_420), 1_788_987_660.0);
        assert_eq!(clock.unix_s(5_000_000, 1_788_800_000), 1_788_800_000.0);
        assert_eq!(clock.latest_unix(), Some(1_789_020_420));
    }

    #[test]
    fn ring_start_marks_the_stall_when_pre_stall_events_download_late() {
        // Ring 4, Sep 2026: synced 09-04, died on an empty battery 09-23, booted on the
        // charger 09-25 without resetting ds, next sync 09-27. Every pre-stall event was
        // downloaded after the stall, so the download time cannot pick the side.
        let before = (8_040_603_i64, 1_788_523_149_i64);
        let boot = 24_041_617_i64;
        let after = (25_724_127_i64, 1_790_523_679_i64);
        let captured = after.1;
        let clock = clock(&[
            (before.0, 0x42, phone(before.1), before.1),
            ev(20_000_000, 1, captured),
            ev(boot, 0x41, captured),
            ev(25_000_000, 1, captured),
            (after.0, 0x42, phone(after.1), captured),
        ]);
        let early = clock.resolve(20_000_000, captured);
        assert_eq!(early.source, ClockSource::Anchor);
        assert!(
            (early.unix - (before.1 as f64 + (20_000_000 - before.0) as f64 / 10.0)).abs() < 0.01
        );
        let late = clock.resolve(25_000_000, captured);
        assert!((late.unix - (after.1 as f64 - (after.0 - 25_000_000) as f64 / 10.0)).abs() < 0.01);
    }

    #[test]
    fn time_sync_wins_over_download_time() {
        let clock = clock(&[
            ev(5_000_000, 1, 1_783_543_000),
            anchor(5_527_617, 1783490291, 1_783_543_500),
            ev(5_600_000, 1, 1_783_544_000),
        ]);
        let got = clock.unix_s(5_266_813, 1_783_543_500);
        assert!((got - 1_783_464_210.6).abs() < 0.01);
    }

    #[test]
    fn capture_time_selects_overlapping_boot_epoch() {
        let clock = clock(&[
            ev(0, 1, 1_700_000_100),
            anchor(5_000_000, 1_700_000_000, 1_700_000_150),
            ev(6_000_000, 1, 1_700_000_199),
            ev(0, 1, 1_800_000_200),
            anchor(500_000, 1_800_000_000, 1_800_000_250),
            ev(1_000_000, 1, 1_800_000_299),
        ]);
        assert_eq!(clock.epochs().len(), 2);
        assert_eq!(clock.unix_s(400_000, 1_800_000_250), 1_799_990_000.0);
    }

    #[test]
    fn insertion_order_retains_reset_when_capture_seconds_match() {
        let clock = clock(&[
            anchor(5_000_000, 1700000000, 300),
            ev(5_100_000, 1, 300),
            anchor(10, 1800000000, 300),
            ev(20, 1, 300),
        ]);
        assert_eq!(clock.epochs().len(), 2);
        assert_eq!(clock.latest_unix(), Some(1_800_000_000));
    }

    #[test]
    fn replayed_old_boot_cannot_create_future_days() {
        let clock = clock(&[
            anchor(7_500_000, 1000000, 2_000_000),
            ev(13_000_000, 1, 2_000_000),
            ev(10, 1, 2_100_000),
            anchor(5_500_000, 2200000, 2_300_000),
            // Newly seen old-boot event appended by a from-zero replay.
            ev(13_000_000, 1, 2_400_000),
        ]);
        assert_eq!(clock.unix_s(13_000_000, 2_400_000), 1_550_000.0);
    }

    #[test]
    fn unanchored_full_drain_epoch_is_undated_not_download_time() {
        // A whole boot downloaded in one second with no time_sync/rtc_beacon: dating
        // it to the download would put the night's end at the sync time.
        let clock = clock(&[
            ev(100_000, 1, 1_789_056_420),
            ev(400_000, 0x76, 1_789_056_420),
            ev(700_000, 1, 1_789_056_420),
        ]);
        let r = clock.resolve(400_000, 1_789_056_420);
        assert_eq!(r.source, ClockSource::Undated);
        assert!(!r.source.is_dated());
    }

    #[test]
    fn incrementally_drained_epoch_falls_back_to_download_time() {
        // Synced every day for three days: download time tracks ring time to within a
        // sync gap, so the fallback is usable (but flagged).
        let clock = clock(&[
            ev(100_000, 1, 1_000_000),
            ev(964_000, 1, 1_086_400),
            ev(1_828_000, 1, 1_172_800),
        ]);
        let r = clock.resolve(1_828_000, 1_172_800);
        assert_eq!(r.source, ClockSource::Fallback);
        assert_eq!(r.unix, 1_172_800.0);
    }

    #[test]
    fn rebase_does_not_project_old_boot_offset_onto_new_boot() {
        // Old boot ran at high counts (anchored). A reboot restarts near zero and the
        // new boot has no anchor yet: its night must not be dated through the old
        // boot's clock (which would land it days earlier), nor to the download time.
        let clock = clock(&[
            anchor(5_000_000, 1788800000, 1_788_800_000),
            ev(5_100_000, 1, 1_788_800_000),
            ev(10, 1, 1_789_056_420),
            ev(300_000, 0x76, 1_789_056_420),
        ]);
        let r = clock.resolve(300_000, 1_789_056_420);
        assert_eq!(r.source, ClockSource::Undated);
        assert_ne!(r.unix, 1_788_800_000.0 + (300_000 - 5_000_000) as f64 / 10.0);
    }

    #[test]
    fn phone_anchor_dates_new_boot() {
        // Same reboot, but the phone recorded an anchor at the end of the sync (ring
        // ds of the newest drained event and phone time). 23:00 to 08:00 UTC+2.
        let sync_unix = 1_789_056_420;
        let clock = clock(&[
            anchor(5_000_000, 1788800000, 1_788_800_000),
            ev(10, 1, sync_unix),
            (705_000, 0x42, phone(1789056420), sync_unix),
        ]);
        let start = clock.resolve(705_000 - (sync_unix - 1_789_002_000) * 10, sync_unix);
        let end = clock.resolve(705_000 - (sync_unix - 1_789_020_000) * 10, sync_unix);
        assert_eq!(start.source, ClockSource::Anchor);
        assert_eq!(start.unix, 1_789_002_000.0);
        assert_eq!(end.unix, 1_789_020_000.0);
        assert_eq!(clock.epochs()[1].anchor_sources, vec!["phone"]);
    }

    #[test]
    fn audited_stalled_interval_with_hidden_and_decoded_reboots_marks_middle_ambiguous() {
        // Anchor 1: ring_ds=47_893_458, unix_time=1_787_733_221
        // Reboot A: 49_912_254; Reboot B: 57_660_709
        // Anchor 2: ring_ds=61_076_535, unix_time=1_789_195_418
        // Wall time (406.166 h) exceeds counter time (366.197 h) by ~39.969 h.
        let cap = 1_789_195_500;
        let clock = clock(&[
            anchor(47_893_458, 1787733221, 1_787_733_300),
            ev(49_912_254, 0x41, cap),
            ev(53_000_000, 0x76, cap),
            ev(57_660_709, 0x41, cap),
            anchor(61_076_535, 1789195418, cap),
        ]);
        let before_first = clock.resolve(48_500_000, cap);
        assert_eq!(before_first.source, ClockSource::Anchor);
        assert!(before_first.undated_reason.is_none());
        let middle = clock.resolve(53_000_000, cap);
        assert_eq!(middle.source, ClockSource::Undated);
        assert_eq!(middle.undated_reason, Some(UndatedReason::AmbiguousRebootStall));
        assert_eq!(middle.undated_reason.unwrap().code(), "ambiguous_reboot_stall");
        let after_second = clock.resolve(59_000_000, cap);
        assert_eq!(after_second.source, ClockSource::Anchor);
        assert!(after_second.undated_reason.is_none());
    }
}
