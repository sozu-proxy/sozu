//! `HealthCheckHeaderValue` decodes, replaces, clears and drops without ever
//! releasing a buffer that still holds a secret.
//!
//! prost's generated decode replaces a populated `bytes` field with `clear`,
//! `reserve`, `put`: when `reserve` reallocates, the allocator frees the old
//! buffer before any `Drop` can wipe it. The oracle below is a global
//! allocator bound to ONE explicitly tracked allocation. At the moment that
//! allocation is released (`dealloc`, or a `realloc` forced to move), it reads
//! only the bytes the test itself initialised and records whether any is
//! non-zero. Every secret byte used here is non-zero, so "any non-zero byte"
//! means "a secret was freed unwiped". All values are synthetic.
//!
//! The statics are process-wide, so every test holds [`serialize`] for its
//! whole body: two tests never track at the same time.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use prost::{
    Message,
    encoding::{WireType, encode_key, encode_varint},
};
use sozu_command_lib::proto::command::HealthCheckHeaderValue;

/// The same message, derived by prost: the differential oracle for the wire
/// format and for decode errors. Named like the real one so that the error
/// location stack compares equal.
mod reference {
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct HealthCheckHeaderValue {
        #[prost(string, required, tag = "1")]
        pub name: String,
        #[prost(bytes = "vec", required, tag = "2")]
        pub value: Vec<u8>,
    }
}

struct Oracle;

/// Address of the tracked allocation; `0` when nothing is tracked.
static TRACKED_PTR: AtomicUsize = AtomicUsize::new(0);
/// Leading bytes of the tracked allocation that the test initialised.
static TRACKED_LEN: AtomicUsize = AtomicUsize::new(0);
/// How many times the tracked allocation was released.
static RELEASES: AtomicUsize = AtomicUsize::new(0);
/// Whether a released allocation held a non-zero initialised byte.
static UNWIPED: AtomicBool = AtomicBool::new(false);
/// Whether a `realloc` of the tracked allocation really moved it.
static MOVED: AtomicBool = AtomicBool::new(false);

impl Oracle {
    /// Claim the release of `ptr` if it is the tracked allocation, and inspect
    /// it. Claiming first makes the event happen once and stops tracking
    /// before the address can be handed out again.
    fn release(ptr: *mut u8) -> bool {
        let tracked = TRACKED_PTR.load(Ordering::SeqCst);
        if tracked == 0
            || tracked != ptr as usize
            || TRACKED_PTR
                .compare_exchange(tracked, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return false;
        }
        // SAFETY: the test registered `ptr` with `track`, promising that its
        // first `TRACKED_LEN` bytes are initialised, and the allocation is
        // still live: it is being released by the caller right now.
        let bytes = unsafe { std::slice::from_raw_parts(ptr, TRACKED_LEN.load(Ordering::SeqCst)) };
        if bytes.iter().any(|&byte| byte != 0) {
            UNWIPED.store(true, Ordering::SeqCst);
        }
        RELEASES.fetch_add(1, Ordering::SeqCst);
        true
    }
}

// SAFETY: every method forwards to `System` with the arguments it received;
// the only addition is a read of the tracked allocation's initialised prefix
// before it is released, and a forced move on `realloc` of that allocation.
unsafe impl GlobalAlloc for Oracle {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        Oracle::release(ptr);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, old: Layout, size: usize) -> *mut u8 {
        let tracked = TRACKED_PTR.load(Ordering::SeqCst) == ptr as usize;
        if !tracked {
            return unsafe { System.realloc(ptr, old, size) };
        }
        // A legal `realloc` may grow in place; this one always moves, so the
        // old buffer is always freed and always inspected.
        let Ok(layout) = Layout::from_size_align(size, old.align()) else {
            return std::ptr::null_mut();
        };
        let new = unsafe { System.alloc(layout) };
        if new.is_null() {
            return new;
        }
        unsafe { std::ptr::copy_nonoverlapping(ptr, new, old.size().min(size)) };
        Oracle::release(ptr);
        MOVED.store(!std::ptr::eq(new, ptr), Ordering::SeqCst);
        unsafe { System.dealloc(ptr, old) };
        new
    }
}

#[global_allocator]
static ALLOCATOR: Oracle = Oracle;

static SERIAL: Mutex<()> = Mutex::new(());

/// Hold this for the whole test: it owns the oracle statics, which it resets.
fn serialize() -> MutexGuard<'static, ()> {
    let guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    TRACKED_PTR.store(0, Ordering::SeqCst);
    TRACKED_LEN.store(0, Ordering::SeqCst);
    RELEASES.store(0, Ordering::SeqCst);
    UNWIPED.store(false, Ordering::SeqCst);
    MOVED.store(false, Ordering::SeqCst);
    guard
}

/// Track the allocation behind `value`, of which the first `initialised`
/// bytes were written by the test.
fn track(value: &[u8], initialised: usize) {
    TRACKED_LEN.store(initialised, Ordering::SeqCst);
    TRACKED_PTR.store(value.as_ptr() as usize, Ordering::SeqCst);
}

/// What happened to the tracked allocation since [`track`].
struct Released {
    times: usize,
    unwiped: bool,
    moved: bool,
}

/// Stop tracking and report. `times` is `0` when the allocation is still live.
fn untrack() -> Released {
    TRACKED_PTR.store(0, Ordering::SeqCst);
    Released {
        times: RELEASES.swap(0, Ordering::SeqCst),
        unwiped: UNWIPED.swap(false, Ordering::SeqCst),
        moved: MOVED.swap(false, Ordering::SeqCst),
    }
}

/// Read back a live allocation the test initialised, to prove it was zeroed in
/// place.
fn assert_zeroed_in_place(pointer: *const u8, initialised: usize, what: &str) {
    // SAFETY: the caller owns a live allocation (no release has happened: the
    // oracle and the pointer comparison in each test prove it) whose first
    // `initialised` bytes it wrote itself.
    let bytes = unsafe { std::slice::from_raw_parts(pointer, initialised) };
    assert!(bytes.iter().all(|&byte| byte == 0), "{what}: not wiped");
}

const SECRET: &[u8] = b"SYNTHETIC-PRIOR-HEADER-VALUE-0xA5!";

/// `len` non-zero bytes in an allocation of exactly `len` bytes.
fn nonzero(len: usize) -> Vec<u8> {
    let bytes: Vec<u8> = (0..len).map(|i| 0xa0 | (i % 16) as u8).collect();
    assert_eq!(bytes.capacity(), len, "the test owns every byte");
    bytes
}

fn make(name: &str, value: &[u8]) -> HealthCheckHeaderValue {
    let carrier = HealthCheckHeaderValue {
        name: name.to_owned(),
        value: value.to_vec(),
    };
    assert_eq!(carrier.value.capacity(), value.len(), "exact capacity");
    carrier
}

fn make_auth(value: &[u8]) -> HealthCheckHeaderValue {
    make("authorization", value)
}

fn field(tag: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_key(tag, WireType::LengthDelimited, &mut out);
    encode_varint(payload.len() as u64, &mut out);
    out.extend_from_slice(payload);
    out
}

fn concat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

// ---- wire format and prost semantics, against the derived reference --------

#[test]
fn round_trips_and_keeps_the_wire_layout() {
    let _serial = serialize();
    let original = make("authorization", SECRET);
    let wire = original.encode_to_vec();
    assert_eq!(
        wire,
        concat(&[&field(1, b"authorization"), &field(2, SECRET)]),
        "name is field 1, value is field 2, both length-delimited"
    );
    assert_eq!(original.encoded_len(), wire.len());
    assert_eq!(
        HealthCheckHeaderValue::decode(wire.as_slice()).unwrap(),
        original
    );

    let delimited = original.encode_length_delimited_to_vec();
    assert_eq!(
        HealthCheckHeaderValue::decode_length_delimited(delimited.as_slice()).unwrap(),
        original
    );
    let mut written = Vec::new();
    original.encode(&mut written).unwrap();
    assert_eq!(written, wire);

    let reference = reference::HealthCheckHeaderValue {
        name: "authorization".to_owned(),
        value: SECRET.to_vec(),
    };
    assert_eq!(
        reference.encode_to_vec(),
        wire,
        "same bytes as generated prost"
    );
}

#[test]
fn empty_name_empty_value_and_defaults() {
    let _serial = serialize();
    let default = HealthCheckHeaderValue::default();
    assert!(default.name.is_empty() && default.value.is_empty());
    // Required fields are always encoded, even when empty.
    assert_eq!(default.encode_to_vec(), [0x0a, 0x00, 0x12, 0x00]);
    assert_eq!(default.encoded_len(), 4);
    assert_eq!(
        HealthCheckHeaderValue::decode([0x0a, 0x00, 0x12, 0x00].as_slice()).unwrap(),
        default
    );
    // prost does not enforce `required` on decode: absent fields are defaults.
    assert_eq!(HealthCheckHeaderValue::decode(&b""[..]).unwrap(), default);
    assert_eq!(
        HealthCheckHeaderValue::decode(field(1, b"x-only-name").as_slice()).unwrap(),
        make("x-only-name", b"")
    );
    assert_eq!(
        HealthCheckHeaderValue::decode(field(2, SECRET).as_slice()).unwrap(),
        make("", SECRET)
    );
    let ordered = make("a", b"b");
    assert!(make("a", b"a") < ordered && ordered < make("b", b""));
}

fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    let name = field(1, b"authorization");
    let value = field(2, SECRET);
    let long_value = field(2, &nonzero(4096));
    vec![
        ("empty", vec![]),
        ("both", concat(&[&name, &value])),
        ("reversed", concat(&[&value, &name])),
        (
            "duplicate name",
            concat(&[&field(1, b"first"), &field(1, b"second")]),
        ),
        ("duplicate value", concat(&[&value, &long_value])),
        (
            "duplicate value, shorter last",
            concat(&[&long_value, &field(2, b"s")]),
        ),
        (
            "duplicate value, empty last",
            concat(&[&value, &field(2, b"")]),
        ),
        ("unknown varint", concat(&[&name, &[0x18, 0x07], &value])),
        ("unknown fixed32", concat(&[&[0x25, 1, 2, 3, 4], &value])),
        (
            "unknown fixed64",
            concat(&[&[0x29, 1, 2, 3, 4, 5, 6, 7, 8], &value]),
        ),
        (
            "unknown length-delimited",
            concat(&[&field(9, b"ignored"), &value]),
        ),
        (
            "unknown group",
            concat(&[&[0x2b], &field(3, b"x"), &[0x2c], &value]),
        ),
        ("value wire type varint", vec![0x10, 0x05]),
        ("name wire type varint", vec![0x08, 0x05]),
        (
            "name invalid utf-8",
            concat(&[&value, &field(1, &[0xff, 0xfe])]),
        ),
        ("value truncated", vec![0x12, 0x05, 0x41, 0x42]),
        ("name truncated", vec![0x0a, 0x03, 0x61]),
        (
            "value after accepted value, key only",
            concat(&[&value, &[0x0a]]),
        ),
        (
            "value then truncated varint key",
            concat(&[&value, &[0x80]]),
        ),
        ("value then tag zero", concat(&[&value, &[0x00]])),
        ("value then stray end group", concat(&[&value, &[0x0c]])),
        ("huge value length", concat(&[&[0x12], &[0xff; 9], &[0x01]])),
        (
            "overlong varint length",
            concat(&[&[0x12], &[0xff; 10], &[0x01]]),
        ),
        (
            "unterminated group",
            concat(&[&value, &[0x2b], &field(3, b"x")]),
        ),
    ]
}

#[test]
fn decode_matches_the_generated_reference_for_every_fragment() {
    let _serial = serialize();
    for (what, bytes) in corpus() {
        let ours = HealthCheckHeaderValue::decode(bytes.as_slice());
        let theirs = reference::HealthCheckHeaderValue::decode(bytes.as_slice());
        match (ours, theirs) {
            (Ok(ours), Ok(theirs)) => {
                assert_eq!(ours.name, theirs.name, "{what}");
                assert_eq!(ours.value, theirs.value, "{what}");
                assert_eq!(ours.encode_to_vec(), theirs.encode_to_vec(), "{what}");
            }
            (Err(ours), Err(theirs)) => assert_eq!(ours, theirs, "{what}"),
            (ours, theirs) => panic!(
                "{what}: ours is_ok={} reference is_ok={}",
                ours.is_ok(),
                theirs.is_ok()
            ),
        }

        let mut delimited = Vec::new();
        encode_varint(bytes.len() as u64, &mut delimited);
        delimited.extend_from_slice(&bytes);
        for (framing, input) in [
            ("delimited", delimited.as_slice()),
            ("raw", bytes.as_slice()),
        ] {
            let ours = HealthCheckHeaderValue::decode_length_delimited(input);
            let theirs = reference::HealthCheckHeaderValue::decode_length_delimited(input);
            match (ours, theirs) {
                (Ok(ours), Ok(theirs)) => {
                    assert_eq!((&ours.name, &ours.value), (&theirs.name, &theirs.value));
                }
                (Err(ours), Err(theirs)) => assert_eq!(ours, theirs, "{what} {framing}"),
                (ours, theirs) => panic!(
                    "{what} {framing}: ours is_ok={} reference is_ok={}",
                    ours.is_ok(),
                    theirs.is_ok()
                ),
            }
        }
    }
}

#[test]
fn merging_into_a_populated_carrier_keeps_the_last_value_like_prost() {
    let _serial = serialize();
    for (what, bytes) in corpus() {
        let mut ours = make("prior-name", SECRET);
        let mut theirs = reference::HealthCheckHeaderValue {
            name: "prior-name".to_owned(),
            value: SECRET.to_vec(),
        };
        match (ours.merge(bytes.as_slice()), theirs.merge(bytes.as_slice())) {
            (Ok(()), Ok(())) => {
                assert_eq!(
                    (&ours.name, &ours.value),
                    (&theirs.name, &theirs.value),
                    "{what}"
                );
            }
            (Err(ours), Err(theirs)) => assert_eq!(ours, theirs, "{what}"),
            (ours, theirs) => panic!("{what}: {ours:?} vs {theirs:?}"),
        }
    }
}

// ---- zeroization -----------------------------------------------------------

#[test]
fn replacing_the_value_that_must_grow_wipes_the_old_allocation_before_release() {
    let _serial = serialize();
    let mut carrier = make("authorization", SECRET);
    let replacement = nonzero(4096);
    track(&carrier.value, SECRET.len());
    carrier
        .merge(field(2, &replacement).as_slice())
        .expect("a well-formed replacement");
    let released = untrack();
    assert_eq!(
        released.times, 1,
        "the old allocation was released exactly once"
    );
    assert!(released.moved, "the oracle forced a real move");
    assert!(
        !released.unwiped,
        "the old allocation was freed holding the secret"
    );
    assert_eq!(carrier.value, replacement, "last one wins");
    assert_eq!(
        carrier.name, "authorization",
        "an absent name is left alone"
    );
}

#[test]
fn a_duplicate_value_field_in_one_fragment_keeps_the_last_value() {
    let _serial = serialize();
    let mut carrier = make("authorization", SECRET);
    let (first, second) = (nonzero(60), nonzero(4096));
    track(&carrier.value, SECRET.len());
    carrier
        .merge(concat(&[&field(2, &first), &field(2, &second)]).as_slice())
        .expect("two well-formed value fields");
    let released = untrack();
    assert!(released.times == 1 && released.moved);
    assert!(
        !released.unwiped,
        "an earlier value reached the allocator unwiped"
    );
    assert_eq!(carrier.value, second);

    // Each earlier allocation, in turn, is wiped before the next replacement.
    let mut carrier = make_auth(&first);
    track(&carrier.value, first.len());
    carrier.merge(field(2, &second).as_slice()).unwrap();
    let released = untrack();
    assert_eq!((released.times, released.unwiped), (1, false));
    track(&carrier.value, second.len());
    carrier
        .merge(field(2, &nonzero(20_000)).as_slice())
        .unwrap();
    let released = untrack();
    assert_eq!((released.times, released.unwiped), (1, false));
    assert_eq!(carrier.value, nonzero(20_000));

    // A shorter last value fits in place: nothing is released, nothing stays.
    let mut carrier = make_auth(&second);
    let pointer = carrier.value.as_ptr();
    carrier.merge(field(2, &first).as_slice()).unwrap();
    assert_eq!(carrier.value, first);
    assert_eq!(carrier.value.as_ptr(), pointer, "replaced in place");
    assert_eq!(
        carrier.value.capacity(),
        second.len(),
        "the spare capacity is the old buffer's tail"
    );
    // SAFETY: `carrier` still owns this allocation; the test initialised all
    // `second.len()` bytes and the tail past `first.len()` was zeroed by the wipe.
    let tail =
        unsafe { std::slice::from_raw_parts(pointer.add(first.len()), second.len() - first.len()) };
    assert!(tail.iter().all(|&byte| byte == 0), "the old tail survived");
}

#[test]
fn a_refused_fragment_leaves_no_accepted_secret_in_the_carrier() {
    let _serial = serialize();
    let accepted = field(2, &nonzero(SECRET.len()));
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "length beyond the input",
            vec![0x12, 0x64, 0x41, 0x42, 0x43],
        ),
        ("wrong wire type for the value", vec![0x10, 0x05]),
        (
            "accepted value, then a bare key",
            concat(&[&accepted, &[0x0a]]),
        ),
        (
            "accepted value, then a cut varint",
            concat(&[&accepted, &[0x80]]),
        ),
        (
            "accepted value, then tag zero",
            concat(&[&accepted, &[0x00]]),
        ),
        (
            "accepted value, then bad utf-8 name",
            concat(&[&accepted, &field(1, &[0xff])]),
        ),
        (
            "accepted value, then a name cut short",
            concat(&[&accepted, &[0x0a, 0x09, 0x61]]),
        ),
        (
            "accepted value, then a stray end group",
            concat(&[&accepted, &[0x0c]]),
        ),
    ];
    for (what, bytes) in cases {
        let mut carrier = make("authorization", SECRET);
        let pointer = carrier.value.as_ptr();
        assert!(carrier.merge(bytes.as_slice()).is_err(), "{what}");
        assert!(carrier.value.is_empty(), "{what}: the value was kept");
        assert_eq!(carrier.value.as_ptr(), pointer, "{what}: no reallocation");
        assert_zeroed_in_place(pointer, SECRET.len(), what);
    }

    // Failures between two fields of a length-delimited message.
    let exceeds = {
        let mut out = Vec::new();
        encode_varint(accepted.len() as u64 - 1, &mut out);
        out.extend_from_slice(&accepted);
        ("delimiter overrun after an accepted value", out)
    };
    let underflow = {
        let mut out = Vec::new();
        encode_varint(accepted.len() as u64 + 10, &mut out);
        out.extend_from_slice(&accepted);
        ("delimiter beyond the input", out)
    };
    for (what, bytes) in [exceeds, underflow] {
        let mut carrier = make("authorization", SECRET);
        let pointer = carrier.value.as_ptr();
        assert!(
            carrier.merge_length_delimited(bytes.as_slice()).is_err(),
            "{what}"
        );
        assert!(carrier.value.is_empty(), "{what}: the value was kept");
        assert_eq!(carrier.value.as_ptr(), pointer);
        assert_zeroed_in_place(pointer, SECRET.len(), what);
    }

    // A fresh decode that fails drops its carrier: the allocation it filled
    // is wiped by the failure itself, before the drop wipes it again.
    let mut fresh = HealthCheckHeaderValue::default();
    assert!(
        fresh
            .merge(concat(&[&accepted, &[0x0a]]).as_slice())
            .is_err()
    );
    assert!(fresh.value.is_empty());
    assert_zeroed_in_place(
        fresh.value.as_ptr(),
        fresh.value.capacity().min(SECRET.len()),
        "fresh",
    );
}

#[test]
fn a_refused_length_accepts_no_partial_bytes() {
    let _serial = serialize();
    // The declared length overruns the input: nothing is copied or allocated
    // for it, so there is no partial secret to leave behind.
    let mut carrier = make("authorization", SECRET);
    let capacity = carrier.value.capacity();
    track(&carrier.value, SECRET.len());
    assert!(carrier.merge(&[0x12, 0x64, 0x41, 0x42, 0x43][..]).is_err());
    let released = untrack();
    assert_eq!(released.times, 0, "the refused fragment released nothing");
    assert_eq!(carrier.value.capacity(), capacity, "and allocated nothing");
}

#[test]
fn clear_wipes_the_value_and_yields_the_defaults() {
    let _serial = serialize();
    let mut carrier = make("authorization", SECRET);
    let pointer = carrier.value.as_ptr();
    carrier.clear();
    assert_eq!(carrier, HealthCheckHeaderValue::default());
    assert!(carrier.name.is_empty() && carrier.value.is_empty());
    assert_eq!(carrier.value.as_ptr(), pointer, "cleared in place");
    assert_zeroed_in_place(pointer, SECRET.len(), "clear");
}

#[test]
fn every_clone_is_an_independent_carrier_wiped_on_its_own_drop() {
    let _serial = serialize();
    let original = make("authorization", SECRET);
    let cloned = original.clone();
    let owned = original.to_owned();
    assert_eq!(cloned, original);
    assert_eq!(owned, original);
    let pointers = [
        original.value.as_ptr(),
        cloned.value.as_ptr(),
        owned.value.as_ptr(),
    ];
    assert!(
        pointers[0] != pointers[1] && pointers[1] != pointers[2] && pointers[0] != pointers[2],
        "each carrier owns its own allocation"
    );
    track(&cloned.value, SECRET.len());
    drop(cloned);
    let released = untrack();
    assert_eq!((released.times, released.unwiped), (1, false), "clone");

    track(&owned.value, SECRET.len());
    drop(owned);
    let released = untrack();
    assert_eq!((released.times, released.unwiped), (1, false), "to_owned");

    // The first carrier is untouched by its clones' end of life.
    assert_eq!(original.value, SECRET);
    track(&original.value, SECRET.len());
    drop(original);
    let released = untrack();
    assert_eq!((released.times, released.unwiped), (1, false), "original");
}

#[test]
fn drop_wipes_the_initialised_bytes_and_the_owned_spare_capacity() {
    let _serial = serialize();
    // A value shorter than its allocation, whose tail the test initialised.
    let mut value = nonzero(64);
    value.truncate(SECRET.len());
    assert!(value.capacity() >= 64);
    let initialised = 64;
    let carrier = HealthCheckHeaderValue {
        name: "authorization".to_owned(),
        value,
    };
    track(&carrier.value, initialised);
    drop(carrier);
    let released = untrack();
    assert_eq!(released.times, 1);
    assert!(
        !released.unwiped,
        "the value or its spare capacity was freed unwiped"
    );
}

#[test]
fn an_oracle_that_is_not_fooled_sees_an_unwiped_release() {
    let _serial = serialize();
    // Control: a plain Vec dropped without the carrier is reported unwiped, so
    // a pass above means the carrier wiped, not that the oracle looks away.
    let plain = nonzero(SECRET.len());
    track(&plain, plain.len());
    drop(plain);
    let released = untrack();
    assert_eq!((released.times, released.unwiped), (1, true));
}

#[test]
fn the_public_contract_is_unchanged() {
    let _serial = serialize();
    let built = HealthCheckHeaderValue {
        name: "x-token".to_owned(),
        value: SECRET.to_vec(),
    };
    let rendered = format!("{built:?} {built:#?}");
    assert!(
        rendered.contains("x-token") && rendered.contains("<protected>"),
        "{rendered}"
    );
    assert!(!rendered.contains("SYNTHETIC"), "{rendered}");
    assert!(
        serde_json::to_string(&built).is_err(),
        "serde refuses to write the value"
    );
    assert!(
        serde_json::from_str::<HealthCheckHeaderValue>(r#"{"name":"x","value":[1]}"#).is_err(),
        "serde refuses to read the value"
    );
}
