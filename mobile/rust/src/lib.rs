//! Consistent CEO mobile — Rust local agent core.
//!
//! Built as a static library for iOS (packaged by `scripts/build-rust.sh` into
//! `modules/cw-rust-bridge/ios/CWMobile.xcframework`, which the local Expo
//! module of the same name vendors) and as a cdylib for Android. The C ABI in
//! `include/cw_mobile.h` is the whole surface: `cw_version`,
//! `cw_parse_reminder`, `cw_parse_note` and `cw_free_string`.
//!
//! The core runs the on-device reminder agent without the gateway: it turns
//! natural language ("remind me to call Mom tomorrow 9am") into a structured
//! reminder or note, computes the fire time, and returns JSON that the TS side
//! (`src/lib/rustBridge.ts`) writes to SQLite.
//!
//! ## Wire contract
//!
//! Input is JSON, `{"text": "...", "utcOffsetMinutes": 780}`, with the offset
//! optional. Anything that is not such an object is taken as the raw text, so a
//! bare string also works. Output is JSON in the exact shape of `ParsedReminder`
//! and `ParsedNote` in `rustBridge.ts`, camelCase included: an earlier version
//! emitted `fire_at`, which the TS side read as `fireAt: undefined` and fired the
//! reminder immediately. When there is nothing to return (a NULL pointer, invalid
//! UTF-8, a panic) the parse functions return NULL, the Swift module rejects, and
//! `rustBridge.ts` falls back to its TS parser.
//!
//! ## Why the caller passes its UTC offset
//!
//! chrono's `Local` reads `/etc/localtime` or `/usr/share/zoneinfo`, which the iOS
//! app sandbox does not guarantee, and silently becomes UTC when it can't, which
//! would ring "tomorrow 9am" at 9am UTC. The Swift module passes
//! `TimeZone.current.secondsFromGMT() / 60` instead, and `Local` is only the
//! fallback for callers that don't. A fixed offset is wrong by an hour only for
//! a reminder whose fire time lies across a daylight-saving change.

use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

use chrono::{DateTime, Duration, FixedOffset, Local, NaiveDate, SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Serialize};

const VERSION: &str = concat!("consistent-world-mobile ", env!("CARGO_PKG_VERSION"));

// ── Wire types ────────────────────────────────────────────────────────

/// A reminder parsed from natural language. Mirrors `ParsedReminder` in
/// `src/lib/rustBridge.ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedReminder {
    /// What to remind about, in the user's own words and case.
    pub what: String,
    /// ISO 8601 in UTC (`2026-11-06T20:00:00.000Z`, the shape JS
    /// `toISOString()` produces). `None` when `in_minutes` is set.
    pub fire_at: Option<String>,
    /// Relative delay for "in 30 minutes" style input.
    pub in_minutes: Option<i64>,
    pub confidence: f32,
}

/// A note parsed from natural language. Mirrors `ParsedNote` in
/// `src/lib/rustBridge.ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedNote {
    pub content: String,
    /// The time phrase as the user wrote it ("tomorrow", "at 5pm").
    pub when: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParseRequest {
    text: String,
    #[serde(default, alias = "utc_offset_minutes")]
    utc_offset_minutes: Option<i32>,
}

impl ParseRequest {
    fn from_input(raw: &str) -> Self {
        if raw.trim_start().starts_with('{') {
            if let Ok(req) = serde_json::from_str::<ParseRequest>(raw) {
                return req;
            }
        }
        ParseRequest {
            text: raw.to_string(),
            utc_offset_minutes: None,
        }
    }

    /// The caller's wall clock: its own offset when it sent one, else `Local`.
    fn now(&self) -> DateTime<FixedOffset> {
        let offset = self
            .utc_offset_minutes
            .and_then(|m| m.checked_mul(60))
            .and_then(FixedOffset::east_opt);
        match offset {
            Some(offset) => Utc::now().with_timezone(&offset),
            None => Local::now().fixed_offset(),
        }
    }
}

// ── C ABI ─────────────────────────────────────────────────────────────

/// Version string. Free with `cw_free_string`.
#[no_mangle]
pub extern "C" fn cw_version() -> *mut c_char {
    into_c_string(VERSION.to_string())
}

/// Free a string returned by one of the `cw_*` functions. NULL is a no-op.
///
/// # Safety
///
/// `s` is NULL or a pointer returned by this library, freed exactly once.
#[no_mangle]
pub unsafe extern "C" fn cw_free_string(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: the caller upholds the contract above, and every pointer this
        // library hands out comes from `CString::into_raw`.
        unsafe { drop(CString::from_raw(s)) };
    }
}

/// Parse a free-form reminder into `ParsedReminder` JSON.
///
/// Input:  `{"text":"remind me to call Mom tomorrow 9am","utcOffsetMinutes":780}`
/// Output: `{"what":"call Mom","fireAt":"2026-11-05T20:00:00.000Z","inMinutes":null,"confidence":0.9}`
///
/// # Safety
///
/// `input` is NULL or a NUL-terminated string valid for the whole call.
#[no_mangle]
pub unsafe extern "C" fn cw_parse_reminder(input: *const c_char) -> *mut c_char {
    // SAFETY: forwarded from this function's contract.
    unsafe {
        ffi_parse(input, |req| {
            serde_json::to_string(&parse_reminder_at(&req.text, req.now()))
        })
    }
}

/// Parse a free-form note into `ParsedNote` JSON.
///
/// Input:  `{"text":"buy milk tomorrow"}`
/// Output: `{"content":"buy milk","when":"tomorrow"}`
///
/// # Safety
///
/// `input` is NULL or a NUL-terminated string valid for the whole call.
#[no_mangle]
pub unsafe extern "C" fn cw_parse_note(input: *const c_char) -> *mut c_char {
    // SAFETY: forwarded from this function's contract.
    unsafe {
        ffi_parse(input, |req| {
            serde_json::to_string(&parse_note_text(&req.text))
        })
    }
}

/// # Safety
///
/// `input` is NULL or a NUL-terminated string valid for the whole call.
unsafe fn ffi_parse(
    input: *const c_char,
    run: impl Fn(&ParseRequest) -> serde_json::Result<String>,
) -> *mut c_char {
    if input.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: non-NULL, and the caller guarantees a live NUL-terminated string.
    let Ok(raw) = unsafe { CStr::from_ptr(input) }.to_str() else {
        return ptr::null_mut();
    };
    // A panic must not cross the C boundary (it would abort the app); NULL
    // sends the caller to its fallback parser instead.
    match catch_unwind(AssertUnwindSafe(|| run(&ParseRequest::from_input(raw)))) {
        Ok(Ok(json)) => into_c_string(json),
        _ => ptr::null_mut(),
    }
}

fn into_c_string(s: String) -> *mut c_char {
    CString::new(s)
        .map(CString::into_raw)
        .unwrap_or(ptr::null_mut())
}

// ── Parsing ───────────────────────────────────────────────────────────
//
// `mobile/src/lib/localParse.ts` is a line-for-line TS port of this section,
// the parser wherever this library isn't linked (Android, web). Change both,
// and mirror new tests in `localParse.test.mjs`.

const REMINDER_PREFIXES: &[&str] = &[
    "remind me to ",
    "remind me ",
    "set a reminder to ",
    "set a reminder for ",
    "set reminder ",
    "reminder to ",
    "reminder ",
];

const NOTE_PREFIXES: &[&str] = &["save note ", "note ", "remember to ", "remember ", "save "];

/// `parse_reminder_at` against the device's local clock.
pub fn parse_reminder_text(input: &str) -> ParsedReminder {
    parse_reminder_at(input, Local::now().fixed_offset())
}

/// Parse a reminder relative to `now`, whose offset is the user's time zone.
pub fn parse_reminder_at(input: &str, now: DateTime<FixedOffset>) -> ParsedReminder {
    let body = strip_prefix_ci(input.trim(), REMINDER_PREFIXES);
    let words = tokenize(body);
    let phrase = scan_time_phrase(&words);
    let what = phrase.rest(&words);
    let what = if what.is_empty() {
        "reminder".to_string()
    } else {
        what
    };

    if let Some(minutes) = phrase.in_minutes {
        return ParsedReminder {
            what,
            fire_at: None,
            in_minutes: Some(minutes),
            confidence: 0.95,
        };
    }
    let (fire, confidence) = match (phrase.day, phrase.clock) {
        (None, None) => (at_day(now, Day::Tomorrow, None), 0.5),
        (Some(Day::Today), None) => (at_day(now, Day::Today, None), 0.7),
        (day, clock) => (at_day(now, day.unwrap_or(Day::Today), clock), 0.9),
    };
    ParsedReminder {
        what,
        fire_at: Some(to_utc_iso(fire)),
        in_minutes: None,
        confidence,
    }
}

/// Parse a note, splitting off its time phrase. Without one, `when` is `None`
/// and the whole text is the content.
pub fn parse_note_text(input: &str) -> ParsedNote {
    let body = strip_prefix_ci(input.trim(), NOTE_PREFIXES);
    let words = tokenize(body);
    let phrase = scan_time_phrase(&words);
    let content = phrase.rest(&words);
    let when = phrase.taken(&words);
    if content.is_empty() || when.is_empty() {
        return ParsedNote {
            content: tidy(body),
            when: None,
        };
    }
    ParsedNote {
        content,
        when: Some(when),
    }
}

/// Case-insensitive prefix strip that keeps the remainder's original case.
/// Prefixes are ASCII, so comparing bytes never splits a character.
fn strip_prefix_ci<'a>(text: &'a str, prefixes: &[&str]) -> &'a str {
    for prefix in prefixes {
        if let Some(head) = text.get(..prefix.len()) {
            if head.eq_ignore_ascii_case(prefix) {
                return text[prefix.len()..].trim_start();
            }
        }
    }
    text
}

struct Word<'a> {
    /// As written, so the reminder keeps the user's case.
    raw: &'a str,
    /// Lowercased with surrounding punctuation removed, for matching.
    key: String,
}

fn tokenize(text: &str) -> Vec<Word<'_>> {
    text.split_whitespace()
        .map(|raw| Word {
            raw,
            key: raw
                .trim_matches(|c: char| ",.;!?()\"".contains(c))
                .to_lowercase(),
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Day {
    Today,
    Tonight,
    Tomorrow,
}

impl Day {
    fn from_key(key: &str) -> Option<Self> {
        match key {
            "today" => Some(Day::Today),
            "tonight" => Some(Day::Tonight),
            "tomorrow" | "tmrw" => Some(Day::Tomorrow),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Clock {
    hour: u32,
    minute: u32,
    /// A bare 1–12 hour ("at 9", "9:30") that could be morning or evening.
    ambiguous: bool,
}

impl Clock {
    const fn at(hour: u32, minute: u32) -> Self {
        Clock {
            hour,
            minute,
            ambiguous: false,
        }
    }
}

/// The time words found in a sentence, and which words they used up.
struct TimePhrase {
    day: Option<Day>,
    clock: Option<Clock>,
    in_minutes: Option<i64>,
    used: Vec<bool>,
}

impl TimePhrase {
    fn take(&mut self, start: usize, len: usize) {
        for used in &mut self.used[start..start + len] {
            *used = true;
        }
    }

    /// The words that were not part of the time phrase.
    fn rest(&self, words: &[Word]) -> String {
        let kept: Vec<&str> = words
            .iter()
            .zip(&self.used)
            .filter(|(_, u)| !**u)
            .map(|(w, _)| w.raw)
            .collect();
        let mut text = tidy(&kept.join(" "));
        // "remind me tomorrow to call Mom" leaves "to call Mom".
        text = strip_prefix_ci(&text, &["to "]).to_string();
        // "call Mom by tomorrow" leaves a dangling "by".
        if self.used.iter().any(|u| *u) {
            for tail in [" at", " on", " by"] {
                if text.len() > tail.len() && text.to_ascii_lowercase().ends_with(tail) {
                    text.truncate(text.len() - tail.len());
                }
            }
        }
        tidy(&text)
    }

    /// The time phrase itself, as written.
    fn taken(&self, words: &[Word]) -> String {
        let taken: Vec<&str> = words
            .iter()
            .zip(&self.used)
            .filter(|(_, u)| **u)
            .map(|(w, _)| w.raw)
            .collect();
        tidy(&taken.join(" "))
    }
}

fn tidy(text: &str) -> String {
    text.trim_matches(|c: char| c.is_whitespace() || ",;:-".contains(c))
        .to_string()
}

fn scan_time_phrase(words: &[Word]) -> TimePhrase {
    let mut phrase = TimePhrase {
        day: None,
        clock: None,
        in_minutes: None,
        used: vec![false; words.len()],
    };
    let mut i = 0;
    while i < words.len() {
        if phrase.in_minutes.is_none() && words[i].key == "in" {
            if let Some((minutes, len)) = parse_relative(&words[i + 1..]) {
                phrase.in_minutes = Some(minutes);
                phrase.take(i, 1 + len);
                i += 1 + len;
                continue;
            }
        }
        if phrase.day.is_none() {
            if let Some(day) = Day::from_key(&words[i].key) {
                phrase.day = Some(day);
                phrase.take(i, 1);
                i += 1;
                continue;
            }
        }
        if phrase.clock.is_none() {
            if let Some((clock, len)) = parse_clock_phrase(&words[i..]) {
                phrase.clock = Some(clock);
                phrase.take(i, len);
                i += len;
                continue;
            }
        }
        i += 1;
    }
    phrase
}

/// "30 minutes", "an hour", "2h", "2 days" after an "in". A number that ends
/// the sentence ("remind me in 30") counts as minutes.
fn parse_relative(words: &[Word]) -> Option<(i64, usize)> {
    let first = words.first()?.key.as_str();
    let digits_end = first
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(first.len());
    let (count, unit, len) = if matches!(first, "a" | "an") {
        (1, words.get(1)?.key.as_str(), 2)
    } else if digits_end == 0 || digits_end > 6 {
        return None;
    } else if digits_end < first.len() {
        (first[..digits_end].parse().ok()?, &first[digits_end..], 1)
    } else if let Some(next) = words.get(1) {
        (first.parse().ok()?, next.key.as_str(), 2)
    } else {
        return Some((first.parse().ok()?, 1));
    };
    let per_unit = match unit {
        "m" | "min" | "mins" | "minute" | "minutes" => 1,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60,
        "d" | "day" | "days" => 24 * 60,
        _ => return None,
    };
    Some((i64::checked_mul(count, per_unit)?, len))
}

/// A clock time with its lead-in: "9am", "at 9", "at 5:30 pm", "21:00",
/// "noon", "this evening", "in the morning".
fn parse_clock_phrase(words: &[Word]) -> Option<(Clock, usize)> {
    let key = |i: usize| words.get(i).map(|w| w.key.as_str());
    match key(0)? {
        "at" => parse_clock(&words[1..], true).map(|(c, len)| (c, len + 1)),
        "this" => part_of_day(key(1)?).map(|c| (c, 2)),
        "in" if key(1) == Some("the") => part_of_day(key(2)?).map(|c| (c, 3)),
        _ => parse_clock(words, false),
    }
}

/// A clock time at the start of `words`. A bare hour ("9") only counts after
/// "at", so "buy 3 apples" keeps its 3.
fn parse_clock(words: &[Word], after_at: bool) -> Option<(Clock, usize)> {
    let first = words.first()?.key.as_str();
    if let Some(clock) = part_of_day(first) {
        return Some((clock, 1));
    }
    let digits_end = first
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(first.len());
    if digits_end == 0 || digits_end > 2 {
        return None;
    }
    let hour: u32 = first[..digits_end].parse().ok()?;
    let mut suffix = &first[digits_end..];
    let mut minute = None;
    if let Some(after) = suffix.strip_prefix(':') {
        let mm = after
            .get(..2)
            .filter(|mm| mm.bytes().all(|b| b.is_ascii_digit()))?;
        minute = Some(mm.parse::<u32>().ok()?);
        suffix = &after[2..];
    }
    let minute_value = minute.unwrap_or(0);
    if minute_value > 59 {
        return None;
    }

    let (pm, len) = if !suffix.is_empty() {
        (Some(meridiem(suffix)?), 1)
    } else if let Some(pm) = words.get(1).and_then(|w| meridiem(&w.key)) {
        (Some(pm), 2)
    } else {
        (None, 1)
    };

    match pm {
        Some(pm) if (1..=12).contains(&hour) => {
            let hour = match (hour, pm) {
                (12, false) => 0,
                (12, true) => 12,
                (h, false) => h,
                (h, true) => h + 12,
            };
            Some((Clock::at(hour, minute_value), len))
        }
        Some(_) => None,
        None if hour > 23 => None,
        // "21:00" and "09:30" are clearly clock times; so is "9" after "at".
        None if minute.is_some() || after_at => {
            let zero_padded = digits_end == 2 && first.starts_with('0');
            let ambiguous = (1..=12).contains(&hour) && !zero_padded;
            Some((
                Clock {
                    hour,
                    minute: minute_value,
                    ambiguous,
                },
                1,
            ))
        }
        None => None,
    }
}

fn meridiem(key: &str) -> Option<bool> {
    match key {
        "am" | "a.m" => Some(false),
        "pm" | "p.m" => Some(true),
        _ => None,
    }
}

fn part_of_day(key: &str) -> Option<Clock> {
    match key {
        "morning" => Some(Clock::at(9, 0)),
        "noon" | "midday" => Some(Clock::at(12, 0)),
        "afternoon" => Some(Clock::at(15, 0)),
        "evening" => Some(Clock::at(18, 0)),
        "midnight" => Some(Clock::at(0, 0)),
        _ => None,
    }
}

/// The fire time for a day word and an optional clock, never in the past.
fn at_day(now: DateTime<FixedOffset>, day: Day, clock: Option<Clock>) -> DateTime<FixedOffset> {
    let today = now.date_naive();
    let fallback = now + Duration::hours(1);
    match day {
        Day::Tomorrow => {
            let c = clock.unwrap_or(Clock::at(9, 0));
            today
                .succ_opt()
                .and_then(|d| at_clock(now, d, c.hour, c.minute))
                .unwrap_or(fallback)
        }
        Day::Tonight => {
            let c = clock.unwrap_or(Clock::at(20, 0));
            // "tonight at 9" means 21:00.
            let hour = if c.ambiguous && c.hour < 12 {
                c.hour + 12
            } else {
                c.hour
            };
            next_at(now, &[(hour, c.minute)]).unwrap_or(fallback)
        }
        Day::Today => match clock {
            // "today" with no time: this evening, or soon if evening has passed.
            None => at_clock(now, today, 18, 0)
                .filter(|t| *t > now)
                .unwrap_or(fallback),
            // A bare "at 7" is whichever of 7:00 and 19:00 comes first.
            Some(c) if c.ambiguous => next_at(
                now,
                &[(c.hour % 12, c.minute), (c.hour % 12 + 12, c.minute)],
            )
            .unwrap_or(fallback),
            Some(c) => next_at(now, &[(c.hour, c.minute)]).unwrap_or(fallback),
        },
    }
}

/// The first of `times` still ahead of `now` today, else the first one tomorrow.
fn next_at(now: DateTime<FixedOffset>, times: &[(u32, u32)]) -> Option<DateTime<FixedOffset>> {
    let today = now.date_naive();
    let ahead = times
        .iter()
        .filter_map(|&(h, m)| at_clock(now, today, h, m))
        .find(|t| *t > now);
    ahead.or_else(|| {
        let &(h, m) = times.first()?;
        at_clock(now, today.succ_opt()?, h, m)
    })
}

fn at_clock(
    now: DateTime<FixedOffset>,
    date: NaiveDate,
    hour: u32,
    minute: u32,
) -> Option<DateTime<FixedOffset>> {
    let local = date.and_hms_opt(hour, minute, 0)?;
    now.offset().from_local_datetime(&local).single()
}

fn to_utc_iso(t: DateTime<FixedOffset>) -> String {
    t.with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 15:00 on 5 Nov 2026 in Auckland (UTC+13).
    fn now() -> DateTime<FixedOffset> {
        FixedOffset::east_opt(13 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 11, 5, 15, 0, 0)
            .unwrap()
    }

    fn fire(text: &str) -> String {
        parse_reminder_at(text, now()).fire_at.expect("fire_at")
    }

    #[test]
    fn parses_tomorrow_9am() {
        let p = parse_reminder_text("remind me to call Mom tomorrow 9am");
        assert_eq!(p.what, "call Mom");
        assert!(p.fire_at.is_some());
        assert!(p.in_minutes.is_none());
    }

    #[test]
    fn parses_in_minutes() {
        let p = parse_reminder_text("remind me in 30 minutes");
        assert_eq!(p.in_minutes, Some(30));
        assert!(p.fire_at.is_none());
    }

    #[test]
    fn parses_note_with_when() {
        let p = parse_note_text("buy milk tomorrow");
        assert_eq!(p.content, "buy milk");
        assert!(p.when.is_some());
    }

    #[test]
    fn tomorrow_resolves_in_the_callers_zone() {
        // 09:00 on the 6th at UTC+13 is 20:00 on the 5th in UTC.
        assert_eq!(
            fire("remind me to call Mom tomorrow 9am"),
            "2026-11-05T20:00:00.000Z"
        );
        assert_eq!(fire("call Mom tomorrow"), "2026-11-05T20:00:00.000Z");
        assert_eq!(
            fire("Remind me to email Sarah tomorrow at 3:30 pm"),
            "2026-11-06T02:30:00.000Z"
        );
    }

    #[test]
    fn keeps_case_and_drops_the_time_words() {
        let p = parse_reminder_at("Remind me to email Sarah tomorrow, at 3pm.", now());
        assert_eq!(p.what, "email Sarah");
        let p = parse_reminder_at("remind me tomorrow 9am to call Mom", now());
        assert_eq!(p.what, "call Mom");
        let p = parse_reminder_at("remind me to Pay Rent by tomorrow", now());
        assert_eq!(p.what, "Pay Rent");
    }

    #[test]
    fn tonight_defaults_to_eight_and_reads_bare_hours_as_evening() {
        assert_eq!(
            fire("take the bins out tonight"),
            "2026-11-05T07:00:00.000Z"
        ); // 20:00 local
        assert_eq!(fire("call Dad tonight at 9"), "2026-11-05T08:00:00.000Z"); // 21:00 local
    }

    #[test]
    fn a_time_without_a_day_is_its_next_occurrence() {
        // 17:30 local is still ahead today.
        assert_eq!(fire("call Bob at 5:30pm"), "2026-11-05T04:30:00.000Z");
        // 9am has passed, so it is tomorrow's.
        assert_eq!(fire("call Bob at 9am"), "2026-11-05T20:00:00.000Z");
        // A bare "at 7" at 15:00 is 19:00, not 07:00 tomorrow.
        assert_eq!(fire("call Bob at 7"), "2026-11-05T06:00:00.000Z");
        assert_eq!(fire("stand-up at 09:30"), "2026-11-05T20:30:00.000Z");
        assert_eq!(fire("lunch at noon"), "2026-11-05T23:00:00.000Z");
    }

    #[test]
    fn relative_delays() {
        let p = parse_reminder_at("remind me in 30 minutes to stretch", now());
        assert_eq!(
            (p.what.as_str(), p.in_minutes, p.fire_at),
            ("stretch", Some(30), None)
        );
        assert_eq!(
            parse_reminder_at("check the oven in 2h", now()).in_minutes,
            Some(120)
        );
        assert_eq!(
            parse_reminder_at("call back in an hour", now()).in_minutes,
            Some(60)
        );
        assert_eq!(
            parse_reminder_at("remind me in 30 minutes", now()).what,
            "reminder"
        );
    }

    #[test]
    fn plain_numbers_and_places_are_not_times() {
        let p = parse_reminder_at("buy 3 apples", now());
        assert_eq!((p.what.as_str(), p.confidence), ("buy 3 apples", 0.5));
        let n = parse_note_text("put the keys in the drawer");
        assert_eq!(
            (n.content.as_str(), n.when),
            ("put the keys in the drawer", None)
        );
        let n = parse_note_text("meet at the cafe");
        assert_eq!(n.when, None);
    }

    #[test]
    fn invalid_times_do_not_panic() {
        for text in [
            "call at 13pm",
            "call at 25:00",
            "call at 9:75",
            "call at 99",
            "in 9999999 minutes",
            "İstanbul at 9:",
        ] {
            let p = parse_reminder_at(text, now());
            assert!(p.fire_at.is_some() || p.in_minutes.is_some(), "{text}");
        }
    }

    #[test]
    fn note_keeps_the_time_phrase_as_written() {
        let n = parse_note_text("Remember to buy milk tomorrow");
        assert_eq!(
            (n.content.as_str(), n.when.as_deref()),
            ("buy milk", Some("tomorrow"))
        );
        let n = parse_note_text("note send the Deck to Ana at 5pm");
        assert_eq!(
            (n.content.as_str(), n.when.as_deref()),
            ("send the Deck to Ana", Some("at 5pm"))
        );
        let n = parse_note_text("ideas for the launch");
        assert_eq!((n.content.as_str(), n.when), ("ideas for the launch", None));
    }

    #[test]
    fn json_uses_the_camel_case_names_rust_bridge_ts_reads() {
        let json = serde_json::to_value(parse_reminder_at("call Mom tomorrow", now())).unwrap();
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["confidence", "fireAt", "inMinutes", "what"]);
    }

    fn call(
        f: unsafe extern "C" fn(*const c_char) -> *mut c_char,
        input: &str,
    ) -> Option<serde_json::Value> {
        let input = CString::new(input).unwrap();
        let out = unsafe { f(input.as_ptr()) };
        if out.is_null() {
            return None;
        }
        let json = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_string();
        unsafe { cw_free_string(out) };
        Some(serde_json::from_str(&json).unwrap())
    }

    #[test]
    fn c_abi_round_trip() {
        let r = call(
            cw_parse_reminder,
            r#"{"text":"remind me to call Mom tomorrow 9am","utcOffsetMinutes":780}"#,
        )
        .unwrap();
        assert_eq!(r["what"], "call Mom");
        assert!(r["fireAt"].as_str().unwrap().ends_with("T20:00:00.000Z"));
        assert!(r["inMinutes"].is_null());

        // Raw text is accepted too.
        let n = call(cw_parse_note, "buy milk tomorrow").unwrap();
        assert_eq!(
            n,
            serde_json::json!({"content": "buy milk", "when": "tomorrow"})
        );

        assert!(unsafe { cw_parse_reminder(ptr::null()) }.is_null());
        unsafe { cw_free_string(ptr::null_mut()) };

        let v = cw_version();
        assert_eq!(unsafe { CStr::from_ptr(v) }.to_str().unwrap(), VERSION);
        unsafe { cw_free_string(v) };
    }
}
