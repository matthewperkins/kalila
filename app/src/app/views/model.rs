//! The model of a trace window: pure functions of the stream layout and the view's settings
//! (rows, channel keys, filters, per-channel GPU info, pyramid keys, sweep hashes). No egui,
//! no GPU and no shared state, so each can be tested from a table.

use crate::config::{TraceViewSettings, TrigSource, ViewFilter};
use crate::review;
use engine::record::RecSource;
use engine::rhythm::{ChannelKind, StreamLayout};

/// Key of an electrode in `TraceViewSettings::channel_filter`: headstage name and index
/// within it ("B1:5"), stable when rows are regrouped or another port changes size.
pub(in crate::app) fn chan_key(l: &StreamLayout, ch: usize) -> Option<String> {
    let c = l.channels.get(ch)?;
    if c.kind != ChannelKind::Electrode {
        return None;
    }
    let hs = c.headstage?;
    let k = l.channels[..ch]
        .iter()
        .filter(|o| o.kind == ChannelKind::Electrode && o.headstage == Some(hs))
        .count();
    Some(format!("{}:{k}", l.headstages.get(hs)?.name))
}

/// `chan_key` of every channel of a layout, made in one pass (`chan_key` rescans the channels
/// before it on every call). Build one where a function needs the keys of many channels.
pub(super) struct ChanKeys(Vec<Option<String>>);

impl ChanKeys {
    pub(super) fn new(l: &StreamLayout) -> Self {
        let mut seen = vec![0usize; l.headstages.len()];
        Self(
            l.channels
                .iter()
                .map(|c| {
                    if c.kind != ChannelKind::Electrode {
                        return None;
                    }
                    let hs = c.headstage?;
                    // The index counts every electrode of the headstage, also when its name
                    // is unknown (then there is no key), exactly as `chan_key` does.
                    let k = match seen.get_mut(hs) {
                        Some(n) => {
                            *n += 1;
                            *n - 1
                        }
                        None => return None,
                    };
                    Some(format!("{}:{k}", l.headstages[hs].name))
                })
                .collect(),
        )
    }

    /// Same as `chan_key(l, ch)`.
    pub(super) fn get(&self, ch: usize) -> Option<&str> {
        self.0.get(ch)?.as_deref()
    }
}

/// Display filter of every channel (AUX/ADC are always drawn raw).
pub(super) fn chan_filters(l: &StreamLayout, d: &TraceViewSettings) -> Vec<ViewFilter> {
    let keys = ChanKeys::new(l);
    (0..l.num_channels())
        .map(|i| {
            if l.channels[i].kind == ChannelKind::Electrode {
                d.filter_of(keys.get(i))
            } else {
                ViewFilter::Raw
            }
        })
        .collect()
}

/// Filters in use among the electrodes drawn in a view (in `ViewFilter::ALL` order).
pub(super) fn used_filters(l: &StreamLayout, filters: &[ViewFilter], row_ch: &[u32]) -> Vec<ViewFilter> {
    ViewFilter::ALL
        .into_iter()
        .filter(|f| {
            row_ch
                .iter()
                .any(|&c| l.channels[c as usize].kind == ChannelKind::Electrode && filters[c as usize] == *f)
        })
        .collect()
}

/// Display CAR applies to the Band electrodes only (reference and subtraction alike).
pub(super) fn car_applies(d: &TraceViewSettings, used: &[ViewFilter]) -> bool {
    d.car.enabled && used.contains(&ViewFilter::Band)
}

/// Set channels' filters (a row chip); if that leaves one filter for every drawn electrode,
/// it becomes the view's filter again and the per-channel entries go.
pub(super) fn set_row_filter(
    l: &StreamLayout,
    d: &mut TraceViewSettings,
    chans: &[usize],
    f: ViewFilter,
    row_ch: &[u32],
) {
    for &c in chans {
        if let Some(k) = chan_key(l, c) {
            d.channel_filter.insert(k, f);
        }
    }
    let used = used_filters(l, &chan_filters(l, d), row_ch);
    if let [only] = used[..] {
        d.filter = only;
        d.channel_filter.clear();
    }
}

/// µV/row text for the status line: one value, or one per filter in use.
pub(super) fn uv_text(d: &TraceViewSettings, used: &[ViewFilter]) -> String {
    let base = match used {
        [] => format!("{:.0} µV/row", d.uv(d.filter)),
        [f] => format!("{:.0} µV/row", d.uv(*f)),
        _ => format!(
            "{} µV/row",
            used.iter()
                .map(|f| format!("{} {:.0}", f.letter(), d.uv(*f)))
                .collect::<Vec<_>>()
                .join(" ")
        ),
    };
    if d.channel_uv.is_empty() {
        base
    } else {
        format!("{base} · per-row")
    }
}

/// Per-channel GPU info for a view (see traces.wgsl): gain; flags (bits 3-4 filter preset,
/// bit 1 CAR reference, bit 2 CAR subtract; electrodes only); units per row; centre.
pub(super) fn view_chinfo(l: &StreamLayout, d: &TraceViewSettings, filters: &[ViewFilter]) -> Vec<[f32; 4]> {
    let in_set = |set: &[usize], ch: usize| set.contains(&ch);
    let keys = ChanKeys::new(l);
    l.channels
        .iter()
        .enumerate()
        .map(|(i, c)| match c.kind {
            ChannelKind::Electrode => {
                let f = filters[i];
                let car = f == ViewFilter::Band;
                let flags = 8 * f.index() as u32
                    + 2 * (car && in_set(&d.car.reference, i)) as u32
                    + 4 * (car && in_set(&d.car.subtract, i)) as u32;
                [c.bit_volts, flags as f32, d.uv_of(keys.get(i), f), 0.0]
            }
            ChannelKind::Aux => [c.bit_volts, 0.0, 2.5, 0.0],
            ChannelKind::Adc => [c.bit_volts, 0.0, d.adc_v_per_row, 0.0],
        })
        .collect()
}

/// The band corners as hashable bits.
pub(super) fn band_bits(d: &TraceViewSettings) -> (u32, u32) {
    (d.band_low_hz.to_bits(), d.band_high_hz.to_bits())
}

/// Identifies the per-channel filters (a change clears the sweeps).
pub(super) fn filters_hash(filters: &[ViewFilter], band: (u32, u32)) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    filters.hash(&mut h);
    band.hash(&mut h);
    h.finish()
}

/// The band pyramid a view wants: its band over the electrodes of its headstages, with the
/// display CAR exactly as the shader applies it (reference and subtraction on Band rows only).
pub(super) fn band_key(l: &StreamLayout, d: &TraceViewSettings) -> engine::pyramid::BandKey {
    let band = d.band();
    let chans = engine::pyramid::band_channels(l, &d.headstages);
    let car = d.car.enabled.then(|| {
        let filters = chan_filters(l, d);
        let on = |set: &[usize]| {
            set.iter()
                .copied()
                .filter(|&c| chans.contains(&c) && filters.get(c) == Some(&ViewFilter::Band))
                .collect::<Vec<_>>()
        };
        (on(&d.car.reference), on(&d.car.subtract))
    });
    engine::pyramid::BandKey {
        low_bits: band.low_hz.to_bits(),
        high_bits: band.high_hz.to_bits(),
        chans,
        car,
    }
}

/// A review image from the pyramids: the column buffer (`nch × cols`, physical units), one
/// TTL word per column, and whether every sample on screen (up to the live edge) was covered.
/// None when the pyramids are not of this recording or the view's band pyramid is missing.
pub(super) fn pyramid_columns(
    st: &engine::pyramid::Store,
    src: &RecSource,
    view: u32,
    l: &StreamLayout,
    filters: &[ViewFilter],
    key: &review::Key,
) -> Option<(Vec<[f32; 2]>, Vec<u16>, bool)> {
    let raw = st.raw.as_ref()?;
    if st.src.as_ref().map(|s| &s.dat) != Some(&src.dat) || raw.nch != key.nch {
        return None;
    }
    let nch = key.nch;
    let cols = key.cols as usize;
    let is_band = |i: usize| filters[i] == ViewFilter::Band && l.channels[i].kind == ChannelKind::Electrode;
    let band = if (0..nch).any(is_band) {
        Some((st.band(view)?, &st.bands.get(&view)?.0))
    } else {
        None
    };
    let t0 = key.t0 - src.first_sample;
    let span = key.span as u64;
    // None where the level this span needs was released under the RAM budget: the caller
    // reads the file instead.
    let coarser_ok = span as f64 / src.sample_rate > super::DIRECT_MAX_S as f64;
    let (rc, words) = raw.columns(t0, span, key.cols, coarser_ok)?;
    let bc = match band {
        Some((p, _)) => Some(p.columns(t0, span, key.cols, coarser_ok)?.0),
        None => None,
    };
    let mut out = vec![[1e30f32, -1e30]; nch * cols];
    for i in 0..nch {
        let gain = l.channels[i].bit_volts;
        let row = if is_band(i) {
            band.and_then(|(_, k)| k.chans.iter().position(|&c| c == i))
        } else {
            None
        };
        for c in 0..cols {
            let m = match (row, &bc) {
                (Some(r), Some(bc)) => bc[c * band.unwrap().0.nch + r],
                (Some(_), None) => continue,
                _ if is_band(i) => continue,
                _ => rc[c * nch + i],
            };
            if m[0] <= m[1] {
                out[i * cols + c] = [m[0] as f32 * gain, m[1] as f32 * gain];
            }
        }
    }
    let lo = t0.max(0) as u64;
    let hi = ((t0 + span as i64).max(0) as u64).min(st.live_end);
    let need = hi.saturating_sub(lo);
    let complete = raw.covered_in(lo, hi) >= need && band.is_none_or(|(p, _)| p.covered_in(lo, hi) >= need);
    Some((out, words, complete))
}

/// Global channel index of a view's slope-trigger channel, when the layout has it.
pub(super) fn trig_channel(l: &StreamLayout, d: &TraceViewSettings) -> Option<usize> {
    if d.trig_channel.is_empty() {
        return None;
    }
    let keys = ChanKeys::new(l);
    (0..l.num_channels()).find(|&i| keys.get(i) == Some(d.trig_channel.as_str()))
}

/// Everything the sweeps of a view depend on, apart from the crossing level (which may move
/// while dragging or as σ updates): a change clears them.
pub(super) fn sweep_hash(
    d: &TraceViewSettings,
    pre: u32,
    post: u32,
    nslots: u32,
    filters: &[ViewFilter],
    cols: u32,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (pre, post, nslots, cols, filters_hash(filters, band_bits(d))).hash(&mut h);
    (
        d.trig_source,
        d.trig_line,
        &d.trig_channel,
        d.trig_slope,
        d.trig_align,
        d.trig_window,
    )
        .hash(&mut h);
    (
        d.trig_level_uv == 0.0,
        d.trig_holdoff_ms.to_bits(),
        d.trig_level2_uv.to_bits(),
    )
        .hash(&mut h);
    h.finish()
}

/// Sweep window (pre, post) of a view in ms: the slope trigger has its own, finer window.
pub(super) fn trig_window_ms(d: &TraceViewSettings) -> (f32, f32) {
    match d.trig_source {
        TrigSource::Ttl => (d.trig_pre_ms, d.trig_post_ms),
        TrigSource::Spike => (d.spk_pre_ms, d.spk_post_ms),
    }
}

/// Rows of one view: channel order (electrodes, then AUX/ADC) and per-row (first, count).
pub(in crate::app) fn view_rows(l: &StreamLayout, v: &TraceViewSettings) -> (Vec<u32>, Vec<[u32; 2]>) {
    let k = v.per_row.clamp(1, 8) as usize;
    let hs_on = |hs: Option<usize>| {
        v.headstages.is_empty()
            || hs
                .and_then(|i| l.headstages.get(i))
                .is_some_and(|h| v.headstages.contains(&h.name))
    };
    let el: Vec<u32> = (0..l.num_channels() as u32)
        .filter(|&i| {
            let c = &l.channels[i as usize];
            c.kind == ChannelKind::Electrode && hs_on(c.headstage)
        })
        .collect();
    let an: Vec<u32> = (0..l.num_channels() as u32)
        .filter(|&i| match l.channels[i as usize].kind {
            ChannelKind::Aux => v.aux,
            ChannelKind::Adc => v.adc,
            ChannelKind::Electrode => false,
        })
        .collect();
    let mut rowtab: Vec<[u32; 2]> = (0..el.len())
        .step_by(k)
        .map(|s| [s as u32, k.min(el.len() - s) as u32])
        .collect();
    rowtab.extend((el.len()..el.len() + an.len()).map(|i| [i as u32, 1]));
    let mut order = el;
    order.extend(an);
    (order, rowtab)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::type_complexity)] // the case tables are tuples of settings and expectations
    use super::*;
    use crate::config::{Slope, TrigAlign, ViewCar};
    use engine::rhythm::{ChannelInfo, HeadstageInfo};

    const UV: f32 = 0.195;
    const AUX_V: f32 = 0.0000374;
    const ADC_V: f32 = 0.00015258789;

    fn hs(name: &str, slot: usize, chip: &str, first: usize) -> HeadstageInfo {
        HeadstageInfo {
            name: name.into(),
            slot,
            port: slot / 2,
            chip: chip.into(),
            num_channels: 16,
            first_channel: first,
            has_bno: false,
        }
    }

    fn ch(name: String, kind: ChannelKind, bit_volts: f32, headstage: Option<usize>) -> ChannelInfo {
        let units = if kind == ChannelKind::Electrode { "uV" } else { "V" };
        ChannelInfo {
            name,
            kind,
            bit_volts,
            units: units.into(),
            headstage,
        }
    }

    /// The rig's layout (as `OniBoard::layout` makes it): A1 an RHD2132 in 16-channel mode
    /// (16 electrodes), B1 an RHD2216 (16), then 3 AUX per headstage and 8 ADC. Electrode names
    /// count on across headstages ("CH17" is B1's first). Channels 0-15 are A1, 16-31 B1,
    /// 32-37 AUX, 38-45 ADC.
    fn rig() -> StreamLayout {
        let mut channels = Vec::new();
        for h in 0..2 {
            for _ in 0..16 {
                let n = channels.len() + 1;
                channels.push(ch(format!("CH{n}"), ChannelKind::Electrode, UV, Some(h)));
            }
        }
        for i in 1..=6 {
            channels.push(ch(format!("AUX{i}"), ChannelKind::Aux, AUX_V, None));
        }
        for i in 1..=8 {
            channels.push(ch(format!("ADC{i}"), ChannelKind::Adc, ADC_V, None));
        }
        StreamLayout {
            name: "acquisition_board".into(),
            sample_rate: 30000.0,
            channels,
            headstages: vec![hs("A1", 0, "RHD2132", 0), hs("B1", 2, "RHD2216", 16)],
            ttl_lines: 8,
            side_streams: Vec::new(),
            hw_clock_hz: 0.0,
        }
    }

    fn view() -> TraceViewSettings {
        TraceViewSettings::default()
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Rows of one channel each, `order` positions `from..to`.
    fn single_rows(from: u32, to: u32) -> Vec<[u32; 2]> {
        (from..to).map(|i| [i, 1]).collect()
    }

    /// `n` electrodes in rows of `k` (the last one short), then 14 single AUX/ADC rows.
    fn grouped(k: u32, n: u32) -> Vec<[u32; 2]> {
        let mut t: Vec<[u32; 2]> = (0..n.div_ceil(k)).map(|r| [r * k, k.min(n - r * k)]).collect();
        t.extend((n..n + 14).map(|i| [i, 1]));
        t
    }

    #[test]
    fn rows_table() {
        let l = rig();
        let all_el: Vec<u32> = (0..32).collect();
        // (what, settings, expected channel order, expected row table). The row table indexes
        // `order`, not channels; AUX/ADC rows follow the electrodes, one channel each.
        let mut cases: Vec<(&str, TraceViewSettings, Vec<u32>, Vec<[u32; 2]>)> = vec![
            ("defaults", view(), (0..46).collect(), single_rows(0, 46)),
            (
                "B1 only",
                TraceViewSettings {
                    headstages: names(&["B1"]),
                    ..view()
                },
                (16..46).collect(),
                single_rows(0, 30),
            ),
            (
                "A1 only, no AUX/ADC",
                TraceViewSettings {
                    headstages: names(&["A1"]),
                    aux: false,
                    adc: false,
                    ..view()
                },
                (0..16).collect(),
                single_rows(0, 16),
            ),
            (
                "both named is all, in channel order",
                TraceViewSettings {
                    headstages: names(&["B1", "A1"]),
                    aux: false,
                    adc: false,
                    ..view()
                },
                all_el,
                single_rows(0, 32),
            ),
            (
                "AUX only",
                TraceViewSettings {
                    aux: true,
                    adc: false,
                    headstages: names(&["A1"]),
                    ..view()
                },
                (0..16).chain(32..38).collect(),
                single_rows(0, 22),
            ),
            (
                "ADC only",
                TraceViewSettings {
                    aux: false,
                    adc: true,
                    headstages: names(&["A1"]),
                    ..view()
                },
                (0..16).chain(38..46).collect(),
                single_rows(0, 24),
            ),
            (
                "unknown headstage: only AUX/ADC",
                TraceViewSettings {
                    headstages: names(&["C1"]),
                    ..view()
                },
                (32..46).collect(),
                single_rows(0, 14),
            ),
            (
                "per_row 2",
                TraceViewSettings { per_row: 2, ..view() },
                (0..46).collect(),
                grouped(2, 32),
            ),
            (
                "per_row 4",
                TraceViewSettings { per_row: 4, ..view() },
                (0..46).collect(),
                grouped(4, 32),
            ),
            (
                "per_row 8",
                TraceViewSettings { per_row: 8, ..view() },
                (0..46).collect(),
                grouped(8, 32),
            ),
            // 32 electrodes in rows of 3: ten full rows and a last row of 2. AUX/ADC stay single.
            (
                "per_row 3 leaves a short last row",
                TraceViewSettings { per_row: 3, ..view() },
                (0..46).collect(),
                grouped(3, 32),
            ),
            (
                "per_row 0 acts as 1",
                TraceViewSettings { per_row: 0, ..view() },
                (0..46).collect(),
                single_rows(0, 46),
            ),
            (
                "per_row 100 acts as 8",
                TraceViewSettings { per_row: 100, ..view() },
                (0..46).collect(),
                grouped(8, 32),
            ),
            // `rows` and `first_row` belong to the window's scrolling: they do not shape the table.
            (
                "rows/first_row do not matter",
                TraceViewSettings {
                    rows: 5,
                    first_row: 7,
                    ..view()
                },
                (0..46).collect(),
                single_rows(0, 46),
            ),
        ];
        // B1 only, 16 electrodes, rows of 8: two rows, then the 14 AUX/ADC rows after them.
        let mut b8 = vec![[0, 8], [8, 8]];
        b8.extend((16..30).map(|i| [i, 1]));
        cases.push((
            "B1 per_row 8",
            TraceViewSettings {
                headstages: names(&["B1"]),
                per_row: 8,
                ..view()
            },
            (16..46).collect(),
            b8,
        ));
        for (what, v, order, rows) in cases {
            assert_eq!(view_rows(&l, &v), (order, rows), "{what}");
        }
    }

    #[test]
    fn rows_of_an_empty_layout() {
        let mut l = rig();
        l.channels.clear();
        l.headstages.clear();
        assert_eq!(view_rows(&l, &view()), (vec![], vec![]));
    }

    #[test]
    fn keys_table() {
        let l = rig();
        // The index is within the headstage, not the stream: B1's first electrode is "B1:0"
        // although it is channel 16 (named "CH17").
        let table: [(usize, Option<&str>); 12] = [
            (0, Some("A1:0")),
            (7, Some("A1:7")),
            (15, Some("A1:15")),
            (16, Some("B1:0")),
            (20, Some("B1:4")),
            (31, Some("B1:15")),
            (32, None), // AUX1
            (37, None), // AUX6
            (38, None), // ADC1
            (45, None), // ADC8
            (46, None), // past the end
            (usize::MAX, None),
        ];
        for (c, want) in table {
            assert_eq!(chan_key(&l, c).as_deref(), want, "channel {c}");
        }
        // A headstage index the layout does not have, or none, gives no key.
        let mut bad = rig();
        bad.channels[3].headstage = Some(9);
        assert_eq!(chan_key(&bad, 3), None);
        bad.channels[3].headstage = None;
        assert_eq!(chan_key(&bad, 3), None);
        // An electrode without a headstage is not counted in the others' index: channel 4 is
        // then "A1:3".
        assert_eq!(chan_key(&bad, 4).as_deref(), Some("A1:3"));
    }

    /// The one-pass cache is the same as `chan_key` for every channel, on odd layouts too.
    #[test]
    fn chan_keys_cache_equals_chan_key() {
        let mut odd = rig();
        odd.channels[3].headstage = Some(9); // a headstage the layout lacks
        odd.channels[10].headstage = None; // an electrode without one
        odd.channels.swap(20, 40); // kinds interleaved
        let mut empty = rig();
        empty.channels.clear();
        for l in [rig(), odd, empty] {
            let keys = ChanKeys::new(&l);
            for c in 0..l.num_channels() + 3 {
                assert_eq!(keys.get(c), chan_key(&l, c).as_deref(), "channel {c}");
            }
        }
    }

    fn with_filters(f: ViewFilter, over: &[(&str, ViewFilter)]) -> TraceViewSettings {
        TraceViewSettings {
            filter: f,
            channel_filter: over.iter().map(|(k, f)| (k.to_string(), *f)).collect(),
            ..view()
        }
    }

    #[test]
    fn filters_table() {
        use ViewFilter::{Band, Raw};
        let l = rig();
        // (settings, electrodes that differ from the window default, the window default)
        let table: [(TraceViewSettings, Vec<(usize, ViewFilter)>, ViewFilter); 5] = [
            (with_filters(Band, &[]), vec![], Band),
            (with_filters(Raw, &[]), vec![], Raw),
            // Overrides by key: "B1:3" is channel 19, "A1:0" channel 0.
            (
                with_filters(Raw, &[("B1:3", Band), ("A1:0", Band)]),
                vec![(19, Band), (0, Band)],
                Raw,
            ),
            (with_filters(Band, &[("A1:15", Raw)]), vec![(15, Raw)], Band),
            // Keys of headstages or electrodes this layout does not have are ignored.
            (
                with_filters(Band, &[("C1:0", Raw), ("A1:16", Raw), ("B1:99", Raw)]),
                vec![],
                Band,
            ),
        ];
        for (d, diff, base) in table {
            let got = chan_filters(&l, &d);
            assert_eq!(got.len(), 46);
            for (c, f) in got.iter().enumerate() {
                let want = if c >= 32 {
                    Raw // AUX and ADC are never filtered, whatever the view says
                } else {
                    diff.iter().find(|(x, _)| *x == c).map_or(base, |(_, f)| *f)
                };
                assert_eq!(*f, want, "channel {c} of {d:?}");
            }
        }
    }

    #[test]
    fn used_filters_table() {
        use ViewFilter::{Band, Raw};
        let l = rig();
        let mut f = vec![Band; 46];
        f[19] = Raw;
        let table: [(&str, Vec<u32>, Vec<ViewFilter>); 7] = [
            (
                "both among all electrodes, in ALL order",
                (0..32).collect(),
                vec![Raw, Band],
            ),
            (
                "rows without the Raw channel",
                (0..19).chain(20..32).collect(),
                vec![Band],
            ),
            ("only the Raw channel", vec![19], vec![Raw]),
            ("A1 only", (0..16).collect(), vec![Band]),
            ("AUX/ADC rows count for nothing", (32..46).collect(), vec![]),
            ("no rows", vec![], vec![]),
            ("AUX does not add to electrodes", vec![0, 32, 45], vec![Band]),
        ];
        for (what, rows, want) in table {
            assert_eq!(used_filters(&l, &f, &rows), want, "{what}");
        }
        // Even when the filter table says Band for an AUX channel, it is not an electrode.
        assert_eq!(used_filters(&l, &[Band; 46], &[33]), vec![]);
    }

    #[test]
    fn set_row_filter_table() {
        use ViewFilter::{Band, Raw};
        let l = rig();
        let all: Vec<u32> = (0..32).collect();
        let entries = |v: &TraceViewSettings| -> Vec<(String, ViewFilter)> {
            v.channel_filter.iter().map(|(k, f)| (k.clone(), *f)).collect()
        };
        let e = |k: &str, f: ViewFilter| (k.to_string(), f);

        // One chip click: the row gets its own entry, the window default stays.
        let mut d = with_filters(Band, &[]);
        set_row_filter(&l, &mut d, &[3], Raw, &all);
        assert_eq!((d.filter, entries(&d)), (Band, vec![e("A1:3", Raw)]));

        // Setting it back leaves a single filter in use: the entries go, that filter is the
        // window's.
        set_row_filter(&l, &mut d, &[3], Band, &all);
        assert_eq!((d.filter, entries(&d)), (Band, vec![]));

        // Every drawn electrode set to Raw: the window default becomes Raw, entries cleared.
        let mut d = with_filters(Band, &[("A1:0", Raw)]);
        set_row_filter(
            &l,
            &mut d,
            &all.iter().map(|&c| c as usize).collect::<Vec<_>>(),
            Raw,
            &all,
        );
        assert_eq!((d.filter, entries(&d)), (Raw, vec![]));

        // Two channels of one row (per_row 2), a mixed result keeps both entries.
        let mut d = with_filters(Raw, &[]);
        set_row_filter(&l, &mut d, &[16, 17], Band, &all);
        assert_eq!((d.filter, entries(&d)), (Raw, vec![e("B1:0", Band), e("B1:1", Band)]));

        // An AUX channel has no key: nothing is inserted.
        let mut d = with_filters(Raw, &[("A1:2", Band)]);
        set_row_filter(&l, &mut d, &[33], Band, &all);
        assert_eq!((d.filter, entries(&d)), (Raw, vec![e("A1:2", Band)]));

        // No electrode rows drawn: nothing to collapse.
        let mut d = with_filters(Raw, &[("A1:2", Band)]);
        set_row_filter(&l, &mut d, &[0], Band, &[]);
        assert_eq!((d.filter, entries(&d)), (Raw, vec![e("A1:0", Band), e("A1:2", Band)]));

        // DOUBTFUL (pinned, not endorsed): the collapse looks only at the rows drawn. In a
        // B1-only window, setting all of B1 to Raw makes Raw the window default and wipes every
        // entry, so A1 channels that are not drawn (Band by the old default or by an entry) turn
        // Raw too.
        let b1: Vec<u32> = (16..32).collect();
        let mut d = with_filters(Band, &[("A1:0", Band)]);
        set_row_filter(&l, &mut d, &(16..32).collect::<Vec<_>>(), Raw, &b1);
        assert_eq!((d.filter, entries(&d)), (Raw, vec![]));
        assert_eq!(chan_filters(&l, &d)[0], Raw);
    }

    fn info(d: &TraceViewSettings) -> Vec<[f32; 4]> {
        let l = rig();
        view_chinfo(&l, d, &chan_filters(&l, d))
    }

    // Flag bits (traces.wgsl): value 2 = CAR reference, 4 = CAR subtract, 8 = Band filter.
    const REF: f32 = 2.0;
    const SUB: f32 = 4.0;
    const BAND: f32 = 8.0;

    #[test]
    fn chinfo_defaults() {
        let v = info(&view());
        assert_eq!(v.len(), 46);
        // Default: Band, 250 µV/row, ADC 5 V/row, AUX 2.5 V/row.
        for (i, e) in v.iter().enumerate() {
            let want = match i {
                0..=31 => [UV, BAND, 250.0, 0.0],
                32..=37 => [AUX_V, 0.0, 2.5, 0.0],
                _ => [ADC_V, 0.0, 5.0, 0.0],
            };
            assert_eq!(*e, want, "channel {i}");
        }
    }

    #[test]
    fn chinfo_flags_table() {
        use ViewFilter::{Band, Raw};
        let car = |reference: &[usize], subtract: &[usize]| ViewCar {
            enabled: true,
            reference: reference.to_vec(),
            subtract: subtract.to_vec(),
        };
        // (what, settings, [(channel, expected flags)])
        let table: Vec<(&str, TraceViewSettings, Vec<(usize, f32)>)> = vec![
            ("raw window", with_filters(Raw, &[]), vec![(0, 0.0), (31, 0.0)]),
            (
                "one Band row",
                with_filters(Raw, &[("B1:1", Band)]),
                vec![(16, 0.0), (17, BAND), (18, 0.0)],
            ),
            (
                "CAR sets on Band rows",
                TraceViewSettings {
                    car: car(&[0, 1], &[1, 2]),
                    ..view()
                },
                vec![(0, BAND + REF), (1, BAND + REF + SUB), (2, BAND + SUB), (3, BAND)],
            ),
            (
                "CAR membership is ignored on Raw rows",
                TraceViewSettings {
                    car: car(&[0, 1], &[1, 2]),
                    ..with_filters(Raw, &[("A1:1", Band)])
                },
                vec![(0, 0.0), (1, BAND + REF + SUB), (2, 0.0)],
            ),
            // The flags carry set membership whether or not the CAR is enabled; the shader is
            // told separately (`Ingest.car`, from `car_applies`) whether to apply it.
            (
                "sets with the CAR disabled still flag",
                TraceViewSettings {
                    car: ViewCar {
                        enabled: false,
                        reference: vec![5],
                        subtract: vec![5],
                    },
                    ..view()
                },
                vec![(5, BAND + REF + SUB), (6, BAND)],
            ),
            (
                "AUX/ADC are never flagged, even listed in a set",
                TraceViewSettings {
                    car: car(&[32, 40], &[32, 40]),
                    ..view()
                },
                vec![(32, 0.0), (40, 0.0)],
            ),
        ];
        for (what, d, want) in table {
            let v = info(&d);
            for (c, flags) in want {
                assert_eq!(v[c][1], flags, "{what}: channel {c}");
            }
        }
    }

    #[test]
    fn chinfo_scale_table() {
        use ViewFilter::Raw;
        // Window default 250 µV/row (Raw), Band 150; channel overrides keyed like the filters.
        let mut d = TraceViewSettings {
            uv_per_row: 250.0,
            uv_by_filter: [0.0, 150.0],
            ..view()
        };
        d.channel_filter.insert("A1:2".into(), Raw);
        d.channel_filter.insert("A1:3".into(), Raw);
        d.channel_uv.insert("A1:0".into(), 100.0);
        d.channel_uv.insert("A1:3".into(), 80.0);
        d.channel_uv.insert("Z9:0".into(), 1.0); // no such headstage: no effect
        let v = info(&d);
        assert_eq!(v[0][2], 100.0, "override on a Band row beats the Band scale");
        assert_eq!(v[1][2], 150.0, "Band row: the filter's scale");
        assert_eq!(v[2][2], 250.0, "Raw row, no Raw scale set: uv_per_row");
        assert_eq!(v[3][2], 80.0, "override on a Raw row");
        assert_eq!(v[16][2], 150.0);
        assert_eq!(v[2][0], UV);

        // A scale of its own for Raw rows is used for them.
        d.uv_by_filter = [40.0, 150.0];
        let v = info(&d);
        assert_eq!(v[2][2], 40.0);
        assert_eq!(v[3][2], 80.0);
        assert_eq!(v[1][2], 150.0);
    }

    #[test]
    fn chinfo_aux_and_adc() {
        let d = TraceViewSettings {
            adc_v_per_row: 2.0,
            uv_per_row: 777.0,
            uv_by_filter: [10.0, 20.0],
            ..view()
        };
        let v = info(&d);
        // AUX: 2.5 V/row whatever the µV scale; ADC: the window's V/row; neither is filtered
        // or given a centre. The gain is the layout's bit_volts.
        for (c, e) in v.iter().enumerate().take(38).skip(32) {
            assert_eq!(*e, [AUX_V, 0.0, 2.5, 0.0], "AUX {c}");
        }
        for (c, e) in v.iter().enumerate().skip(38) {
            assert_eq!(*e, [ADC_V, 0.0, 2.0, 0.0], "ADC {c}");
        }
        // The ADC range does not touch the electrode scale.
        assert_eq!(v[0][2], 20.0);
    }

    fn key_of(d: &TraceViewSettings) -> engine::pyramid::BandKey {
        band_key(&rig(), d)
    }

    #[test]
    fn band_key_content() {
        let k = key_of(&view());
        assert_eq!(k.low_bits, 300.0f64.to_bits());
        assert_eq!(k.high_bits, 6000.0f64.to_bits());
        assert_eq!(k.chans, (0..32).collect::<Vec<_>>());
        assert_eq!(k.car, None);
        let a = key_of(&TraceViewSettings {
            headstages: names(&["A1"]),
            ..view()
        });
        assert_eq!(a.chans, (0..16).collect::<Vec<_>>());
        let b = key_of(&TraceViewSettings {
            headstages: names(&["B1"]),
            ..view()
        });
        assert_eq!(b.chans, (16..32).collect::<Vec<_>>());
        // Corners go through `BandSpec` (clamped to 0.1..5000 and 10..15000; 0 = off).
        let off = key_of(&TraceViewSettings {
            band_low_hz: 0.0,
            band_high_hz: 0.0,
            ..view()
        });
        assert_eq!((off.low_bits, off.high_bits), (0, 0));
        let lo = key_of(&TraceViewSettings {
            band_low_hz: 0.05,
            ..view()
        });
        assert_eq!(lo.low_bits, (0.1f32 as f64).to_bits());
        assert_eq!(
            lo.low_bits,
            key_of(&TraceViewSettings {
                band_low_hz: 0.1,
                ..view()
            })
            .low_bits
        );
    }

    #[test]
    fn band_key_changes_only_with_its_inputs() {
        let base = TraceViewSettings {
            car: ViewCar {
                enabled: true,
                reference: vec![0, 1, 20],
                subtract: vec![1, 40],
            },
            ..view()
        };
        let k0 = key_of(&base);
        let b = || base.clone();
        let changes: Vec<(&str, TraceViewSettings)> = vec![
            (
                "low corner",
                TraceViewSettings {
                    band_low_hz: 10.0,
                    ..b()
                },
            ),
            (
                "high corner",
                TraceViewSettings {
                    band_high_hz: 3000.0,
                    ..b()
                },
            ),
            (
                "headstages",
                TraceViewSettings {
                    headstages: names(&["A1"]),
                    ..b()
                },
            ),
            (
                "CAR off",
                TraceViewSettings {
                    car: ViewCar {
                        enabled: false,
                        ..base.car.clone()
                    },
                    ..b()
                },
            ),
            (
                "CAR reference",
                TraceViewSettings {
                    car: ViewCar {
                        reference: vec![0, 1],
                        ..base.car.clone()
                    },
                    ..b()
                },
            ),
            (
                "CAR subtract",
                TraceViewSettings {
                    car: ViewCar {
                        subtract: vec![2],
                        ..base.car.clone()
                    },
                    ..b()
                },
            ),
            // A CAR member that stops being a Band row drops out of the key.
            (
                "a CAR channel made Raw",
                TraceViewSettings {
                    channel_filter: [("A1:1".to_string(), ViewFilter::Raw)].into(),
                    ..b()
                },
            ),
        ];
        for (what, d) in changes {
            assert_ne!(key_of(&d), k0, "{what} must change the key");
        }
        let same: Vec<(&str, TraceViewSettings)> = vec![
            (
                "name",
                TraceViewSettings {
                    name: "x".into(),
                    ..b()
                },
            ),
            ("window_s", TraceViewSettings { window_s: 7.0, ..b() }),
            (
                "uv_per_row",
                TraceViewSettings {
                    uv_per_row: 900.0,
                    ..b()
                },
            ),
            (
                "uv_by_filter",
                TraceViewSettings {
                    uv_by_filter: [1.0, 2.0],
                    ..b()
                },
            ),
            (
                "channel_uv",
                TraceViewSettings {
                    channel_uv: [("A1:0".to_string(), 5.0)].into(),
                    ..b()
                },
            ),
            (
                "rows/first_row",
                TraceViewSettings {
                    rows: 4,
                    first_row: 3,
                    ..b()
                },
            ),
            ("per_row", TraceViewSettings { per_row: 4, ..b() }),
            (
                "aux/adc",
                TraceViewSettings {
                    aux: false,
                    adc: false,
                    ..b()
                },
            ),
            (
                "adc range",
                TraceViewSettings {
                    adc_v_per_row: 1.0,
                    ..b()
                },
            ),
            ("thickness", TraceViewSettings { thickness: 3.0, ..b() }),
            (
                "triggers",
                TraceViewSettings {
                    triggered: true,
                    trig_line: 3,
                    trig_sweeps: 5,
                    trig_level_uv: -90.0,
                    ..b()
                },
            ),
            (
                "overlay",
                TraceViewSettings {
                    ttl_overlay: false,
                    ttl_hidden: 3,
                    ..b()
                },
            ),
            // A Band entry on a channel that is Band already changes nothing.
            (
                "redundant Band entry",
                TraceViewSettings {
                    channel_filter: [("A1:5".to_string(), ViewFilter::Band)].into(),
                    ..b()
                },
            ),
        ];
        for (what, d) in same {
            assert_eq!(key_of(&d), k0, "{what} must not change the key");
        }
        // The CAR sets only count while the CAR is on.
        let off = |r: Vec<usize>| TraceViewSettings {
            car: ViewCar {
                enabled: false,
                reference: r,
                subtract: vec![],
            },
            ..view()
        };
        assert_eq!(key_of(&off(vec![1])), key_of(&off(vec![2, 3])));
        assert_eq!(key_of(&off(vec![1])), key_of(&view()));
    }

    #[test]
    fn band_key_car_sets() {
        let d = |car: ViewCar, hs: &[&str], over: &[(&str, ViewFilter)]| TraceViewSettings {
            car,
            headstages: names(hs),
            channel_filter: over.iter().map(|(k, f)| (k.to_string(), *f)).collect(),
            ..view()
        };
        let car = ViewCar {
            enabled: true,
            reference: vec![0, 1, 20],
            subtract: vec![1, 40],
        };
        // 40 is an AUX channel: not in the pyramid, dropped. The rest stay.
        assert_eq!(key_of(&d(car.clone(), &[], &[])).car, Some((vec![0, 1, 20], vec![1])));
        // Channel 1 drawn Raw: neither reference nor subtracted in the pyramid.
        assert_eq!(
            key_of(&d(car.clone(), &[], &[("A1:1", ViewFilter::Raw)])).car,
            Some((vec![0, 20], vec![]))
        );
        // B1's channel 20 is outside an A1-only window.
        assert_eq!(key_of(&d(car.clone(), &["A1"], &[])).car, Some((vec![0, 1], vec![1])));
        // DOUBTFUL (pinned): with every row Raw and the CAR on, the key is Some(([], [])), not
        // None, so toggling the CAR on a Raw window changes the key (a rebuild) though the
        // pyramid would be the same.
        let raw = TraceViewSettings {
            filter: ViewFilter::Raw,
            ..d(car, &[], &[])
        };
        assert_eq!(key_of(&raw).car, Some((vec![], vec![])));
        // DOUBTFUL (pinned): set order matters ([1, 0] differs from [0, 1]); the CAR palette
        // keeps the sets sorted, so it does not show.
        let swapped = ViewCar {
            enabled: true,
            reference: vec![1, 0],
            subtract: vec![],
        };
        let sorted = ViewCar {
            enabled: true,
            reference: vec![0, 1],
            subtract: vec![],
        };
        assert_ne!(key_of(&d(swapped, &[], &[])), key_of(&d(sorted, &[], &[])));
    }

    #[test]
    fn sweep_hash_table() {
        let filters = vec![ViewFilter::Band; 46];
        let h = |d: &TraceViewSettings| sweep_hash(d, 300, 1200, 20, &filters, 800);
        let base = view();
        let b = || base.clone();
        let h0 = h(&base);
        let mut one_raw = filters.clone();
        one_raw[3] = ViewFilter::Raw;
        let differs: Vec<(&str, u64)> = vec![
            ("pre", sweep_hash(&base, 301, 1200, 20, &filters, 800)),
            ("post", sweep_hash(&base, 300, 1201, 20, &filters, 800)),
            ("slots", sweep_hash(&base, 300, 1200, 21, &filters, 800)),
            ("cols", sweep_hash(&base, 300, 1200, 20, &filters, 801)),
            ("filters", sweep_hash(&base, 300, 1200, 20, &one_raw, 800)),
            (
                "band low",
                h(&TraceViewSettings {
                    band_low_hz: 100.0,
                    ..b()
                }),
            ),
            (
                "band high",
                h(&TraceViewSettings {
                    band_high_hz: 5000.0,
                    ..b()
                }),
            ),
            (
                "source",
                h(&TraceViewSettings {
                    trig_source: TrigSource::Spike,
                    ..b()
                }),
            ),
            ("line", h(&TraceViewSettings { trig_line: 2, ..b() })),
            (
                "channel",
                h(&TraceViewSettings {
                    trig_channel: "A1:3".into(),
                    ..b()
                }),
            ),
            (
                "slope",
                h(&TraceViewSettings {
                    trig_slope: Slope::Pos,
                    ..b()
                }),
            ),
            (
                "align",
                h(&TraceViewSettings {
                    trig_align: TrigAlign::Crossing,
                    ..b()
                }),
            ),
            (
                "window",
                h(&TraceViewSettings {
                    trig_window: true,
                    ..b()
                }),
            ),
            (
                "holdoff",
                h(&TraceViewSettings {
                    trig_holdoff_ms: 2.0,
                    ..b()
                }),
            ),
            (
                "level2",
                h(&TraceViewSettings {
                    trig_level2_uv: 100.0,
                    ..b()
                }),
            ),
            // A fixed level instead of the detector's is a change; moving it is not (below).
            (
                "fixed level on",
                h(&TraceViewSettings {
                    trig_level_uv: -80.0,
                    ..b()
                }),
            ),
        ];
        for (what, x) in differs {
            assert_ne!(x, h0, "{what} must clear the sweeps");
        }
        let fixed = TraceViewSettings {
            trig_level_uv: -80.0,
            ..b()
        };
        let moved = TraceViewSettings {
            trig_level_uv: -95.0,
            ..b()
        };
        assert_eq!(h(&moved), h(&fixed), "moving the level keeps the sweeps");
        for (what, d) in [
            (
                "uv",
                TraceViewSettings {
                    uv_per_row: 900.0,
                    ..b()
                },
            ),
            ("window_s", TraceViewSettings { window_s: 9.0, ..b() }),
            ("sweep count", TraceViewSettings { trig_sweeps: 3, ..b() }),
            (
                "TTL window ms",
                TraceViewSettings {
                    trig_pre_ms: 5.0,
                    trig_post_ms: 9.0,
                    ..b()
                },
            ),
            ("per_row", TraceViewSettings { per_row: 4, ..b() }),
            (
                "rows",
                TraceViewSettings {
                    rows: 3,
                    first_row: 2,
                    ..b()
                },
            ),
            (
                "car",
                TraceViewSettings {
                    car: ViewCar {
                        enabled: true,
                        reference: vec![1],
                        subtract: vec![1],
                    },
                    ..b()
                },
            ),
        ] {
            assert_eq!(h(&d), h0, "{what} must not clear the sweeps");
        }
    }

    #[test]
    fn small_helpers() {
        let l = rig();
        // trig_channel: the key's channel; None for "", an unknown headstage or index, a name.
        for (key, want) in [
            ("", None),
            ("A1:0", Some(0)),
            ("B1:3", Some(19)),
            ("B1:15", Some(31)),
            ("C1:0", None),
            ("A1:16", None),
            ("AUX1", None),
        ] {
            let d = TraceViewSettings {
                trig_channel: key.into(),
                ..view()
            };
            assert_eq!(trig_channel(&l, &d), want, "{key:?}");
        }
        // Sweep window: TTL has its own, the slope trigger a finer one.
        let d = TraceViewSettings {
            trig_pre_ms: 10.0,
            trig_post_ms: 40.0,
            spk_pre_ms: 1.0,
            spk_post_ms: 2.0,
            ..view()
        };
        assert_eq!(trig_window_ms(&d), (10.0, 40.0));
        let s = TraceViewSettings {
            trig_source: TrigSource::Spike,
            ..d
        };
        assert_eq!(trig_window_ms(&s), (1.0, 2.0));
        // The CAR applies only when enabled and a Band row is in use.
        let (raw, band, both) = (
            [ViewFilter::Raw],
            [ViewFilter::Band],
            [ViewFilter::Raw, ViewFilter::Band],
        );
        let cases: [(bool, &[ViewFilter], bool); 5] = [
            (true, &raw, false),
            (true, &band, true),
            (true, &both, true),
            (true, &[], false),
            (false, &band, false),
        ];
        for (enabled, used, want) in cases {
            let d = TraceViewSettings {
                car: ViewCar {
                    enabled,
                    ..Default::default()
                },
                ..view()
            };
            assert_eq!(car_applies(&d, used), want, "{enabled} {used:?}");
        }
        // µV/row text.
        let d = TraceViewSettings {
            uv_per_row: 250.0,
            uv_by_filter: [100.0, 150.0],
            ..view()
        };
        assert_eq!(uv_text(&d, &[ViewFilter::Band]), "150 µV/row");
        assert_eq!(uv_text(&d, &[ViewFilter::Raw]), "100 µV/row");
        assert_eq!(uv_text(&d, &[ViewFilter::Raw, ViewFilter::Band]), "R 100 B 150 µV/row");
        assert_eq!(
            uv_text(&d, &[]),
            "150 µV/row",
            "no electrode rows: the window's filter (Band)"
        );
        let o = TraceViewSettings {
            channel_uv: [("A1:0".to_string(), 5.0)].into(),
            ..d
        };
        assert_eq!(uv_text(&o, &[ViewFilter::Band]), "150 µV/row · per-row");
        // Band corners as bits; the filter hash follows the filters and the corners.
        assert_eq!(band_bits(&view()), (300.0f32.to_bits(), 6000.0f32.to_bits()));
        let f = [ViewFilter::Raw, ViewFilter::Band];
        assert_eq!(filters_hash(&f, (1, 2)), filters_hash(&f, (1, 2)));
        assert_ne!(filters_hash(&f, (1, 2)), filters_hash(&f, (1, 3)));
        assert_ne!(
            filters_hash(&f, (1, 2)),
            filters_hash(&[ViewFilter::Band, ViewFilter::Raw], (1, 2))
        );
    }
}
