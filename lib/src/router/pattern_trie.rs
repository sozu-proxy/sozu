use std::{collections::HashMap, fmt::Debug, iter, str};

use regex::bytes::{Regex, RegexBuilder};

pub type Key = Vec<u8>;
pub type KeyValue<K, V> = (K, V);

#[derive(Debug, PartialEq, Eq)]
pub enum InsertResult {
    Ok,
    Existing,
    Failed,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RemoveResult {
    Ok,
    NotFound,
}

fn find_last_dot(input: &[u8]) -> Option<usize> {
    //println!("find_last_dot: input = {}", from_utf8(input).unwrap());
    (0..input.len()).rev().find(|&i| input[i] == b'.')
}

fn find_last_slash(input: &[u8]) -> Option<usize> {
    //println!("find_last_dot: input = {}", from_utf8(input).unwrap());
    (0..input.len()).rev().find(|&i| input[i] == b'/')
}

/// The stored pattern for one regex hostname segment: the configured
/// `segment` anchored at both ends so `Regex::is_match` — a substring
/// search — only succeeds on a whole-label match. `cdn[0-9]+` must match
/// `cdn1` and not `cdn1xxx`.
///
/// The group is NOT decoration. `|` binds looser than concatenation, so the
/// ungrouped `\Aa|b\z` parses as `(\Aa)|(b\z)` and each branch keeps only
/// one anchor — measured, it still matches `axx`, and with three branches the
/// middle one keeps NEITHER anchor and matches `zzbzz` as a bare substring.
/// `(?:` … `)` is non-capturing, so `captures_len` and therefore every
/// `$HOST[n]` rewrite index (`lookup_with_path` hands the segment regex to
/// `router/mod.rs`'s `RouteResult::new_with_trie`, which reads
/// `caps.iter().skip(1)`) are unchanged.
///
/// The segment is compiled ON ITS OWN first and only a segment that compiles
/// is wrapped. The wrapper supplies one `(` and one `)`, so an unbalanced
/// segment can be balanced BY the wrapping: `a)(b` is rejected by
/// `Regex::new`, yet `\A(?:a)(b)\z` compiles and matches `ab`. Without the
/// first compile, anchoring would silently promote a segment the trie rejects
/// today into a live route matching something the operator never wrote.
///
/// The same string is the identity of a `regexps` entry — the dedup scan in
/// `insert_recursive`, `remove_recursive` and `lookup_mut` all compare
/// `Regex::as_str()` against it — so every site must build it here. The one
/// site that also needs a compiled `Regex` goes through
/// [`compiled_segment`], which wraps this function and does not respell it.
///
/// `None` means "this segment is not a usable regex": insert fails, and
/// remove/lookup find nothing, which is correct because insert never stored
/// it. `PathRule::anchored_regex` in `router/mod.rs` is the same shape for
/// the path side; it is not shared because it returns a compiled `Regex` and
/// three of the four call sites here need only the string, on a control-plane
/// path that would otherwise recompile a regex per call.
fn anchored_segment(segment: &str) -> Option<String> {
    Regex::new(segment).ok()?;
    let anchored = format!("\\A(?:{segment})\\z");
    debug_assert!(
        anchored.starts_with("\\A(?:") && anchored.ends_with(")\\z"),
        "segment regex must be fully anchored AND grouped so every alternation \
         branch matches the whole segment only",
    );
    Some(anchored)
}

/// Compile the stored pattern for one regex hostname segment.
///
/// The pattern string comes from [`anchored_segment`] and from nowhere
/// else: it is the identity a `regexps` entry is addressed by --
/// `insert_recursive`, `remove_recursive` and `lookup_mut` all compare
/// `Regex::as_str()` against it -- and `RegexBuilder` leaves `as_str()`
/// as the pattern it was handed, so a stored segment stays addressable
/// and removable.
///
/// Compiled CASE-INSENSITIVELY, for the same reason `DomainRule::from_str`
/// is (`router/mod.rs`): a hostname is case-insensitive (RFC 9110 §4.2.3)
/// and the keys reaching `domain_lookup` are ASCII-lowercased, while the
/// segment source is no longer folded on the way in. `Router::add_tree_rule`
/// used to hand the whole hostname to `idna::domain_to_ascii`, which folded
/// `/API[0-9]/` to `/api[0-9]/` and made an uppercase literal match by
/// accident -- and folded `\D` to `\d` in the same pass, inverting the
/// class the operator wrote (sozu#1377). It now stops at the literal
/// labels, and the fold happens here instead, where it reaches literals
/// only: case insensitivity does not touch `\d`/`\D`, `\w`/`\W` or
/// `\s`/`\S`.
///
/// The folding is the crate default, i.e. Unicode simple case folding and
/// NOT ASCII-only -- `.unicode(false)` is deliberately left unset, exactly
/// as in `DomainRule::from_str`, whose comment carries the measurement:
/// it would reject `\p{L}` patterns that compile and route today.
///
/// The group `anchored_segment` supplies is non-capturing and the builder
/// adds none, so `captures_len` -- and therefore every `$HOST[n]` rewrite
/// index, read from `caps.iter().skip(1)` in `router/mod.rs`'s
/// `RouteResult::new_with_trie` -- is unchanged.
fn compiled_segment(segment: &str) -> Option<Regex> {
    let anchored = anchored_segment(segment)?;
    let compiled = RegexBuilder::new(&anchored)
        .case_insensitive(true)
        .build()
        .ok()?;
    debug_assert_eq!(
        compiled.as_str(),
        anchored,
        "a `regexps` entry is addressed by `as_str()`, so the builder must \
         not respell the pattern `anchored_segment` produced",
    );
    Some(compiled)
}

/// Implementation of a trie tree structure.
/// In Sozu this is used to store and lookup domains recursively.
/// Each node represents a "level domain".
/// A leaf node (leftmost label) can be a wildcard, a regex pattern or a plain string.
/// Leaves also store a value associated with the complete domain.
/// For Sozu it is a list of (PathRule, MethodRule, ClusterId). See the Router strucure.
#[derive(Debug, Default)]
pub struct TrieNode<V> {
    key_value: Option<KeyValue<Key, V>>,
    wildcard: Option<KeyValue<Key, V>>,
    children: HashMap<Key, TrieNode<V>>,
    regexps: Vec<(Regex, TrieNode<V>)>,
}

/// One step of a trie traversal where a non-literal segment matched.
///
/// `Wildcard` carries the actual segment bytes consumed by a `*` wildcard
/// (so a router that wants to capture them can splice them into a rewrite
/// template). `Regexp` carries both the matched bytes and the regex itself
/// so the caller can re-run `Regex::captures` to pull explicit groups.
#[derive(Debug)]
pub enum TrieSubMatch<'a, 'b> {
    Wildcard(&'a [u8]),
    Regexp(&'a [u8], &'b Regex),
}

/// Ordered list of non-literal trie segments visited during a successful
/// `lookup_with_path` traversal. Routers feed the entries into rewrite
/// templates (`$HOST[n]`) so frontend rewrites can reach into the matched
/// segments. Empty when only literal segments matched.
pub type TrieMatches<'a, 'b> = Vec<TrieSubMatch<'a, 'b>>;

/// Sink for the non-literal trie segments a lookup consumed.
///
/// [`TrieNode::lookup`] and [`TrieNode::lookup_with_path`] run the SAME
/// walk, so the precedence order is stated once. They differ only in
/// whether they record the segments it matched, and a caller with no
/// rewrite template to fill must not pay for a record it will drop:
/// [`NoTrace`] monomorphises every sink call away, so SNI resolution
/// (`lib/src/tls.rs`, `lib/src/protocol/tcp_preread/`) still allocates
/// nothing per lookup.
///
/// That is a claim about the SINK alone. The `accept` predicate beside it
/// is a `&mut dyn FnMut`, not a generic parameter, so it is one indirect
/// call per candidate leaf the walk reaches — including on the SNI path,
/// where it is the constant `true`. Making it generic too would
/// monomorphise `lookup_recursive` once per predicate type; it has not
/// been worth the code size.
///
/// The walk BACKTRACKS, so a sink must be rewindable: a candidate that
/// ends up not answering must leave none of its segments behind.
trait SubMatchSink<'a, 'b> {
    /// Position to rewind to when the branch about to be tried fails.
    fn mark(&self) -> usize;
    fn rewind(&mut self, mark: usize);
    fn push(&mut self, sub_match: TrieSubMatch<'a, 'b>);
}

/// Sink that records nothing.
struct NoTrace;

impl<'a, 'b> SubMatchSink<'a, 'b> for NoTrace {
    fn mark(&self) -> usize {
        0
    }

    fn rewind(&mut self, _mark: usize) {}

    fn push(&mut self, _sub_match: TrieSubMatch<'a, 'b>) {}
}

impl<'a, 'b> SubMatchSink<'a, 'b> for TrieMatches<'a, 'b> {
    fn mark(&self) -> usize {
        self.len()
    }

    fn rewind(&mut self, mark: usize) {
        Vec::truncate(self, mark);
    }

    fn push(&mut self, sub_match: TrieSubMatch<'a, 'b>) {
        Vec::push(self, sub_match);
    }
}

/// Segments of the trie walk kept on the stack by [`InlineTrieMatches`]
/// before it spills to the heap. A hostname carries one non-literal segment
/// per label at most, and routing tables rarely stack more than a couple.
const INLINE_TRIE_MATCHES: usize = 16;

/// Record of the non-literal segments a walk matched, for
/// [`crate::router::Router::lookup`]: the first [`INLINE_TRIE_MATCHES`]
/// entries live in a fixed array on the caller's stack, later ones in a
/// `Vec` that allocates only when an entry actually lands there.
///
/// [`TrieMatches`] is the same record as a plain `Vec`, which a lookup had to
/// allocate up front even for a literal host that records nothing. The fixed
/// array plays the part of HAProxy's `regmatch_t pmatch[MAX_MATCH]`
/// (`src/sample.c:3487` at haproxy `0ceb8c65`), except that HAProxy clamps a
/// match at `MAX_MATCH` (`src/regex.c:155-156`) where this spills, so no
/// segment is ever dropped from a `$HOST[n]` template.
pub(crate) struct InlineTrieMatches<'a, 'b> {
    inline: [Option<TrieSubMatch<'a, 'b>>; INLINE_TRIE_MATCHES],
    spilled: Vec<TrieSubMatch<'a, 'b>>,
    len: usize,
}

impl<'a, 'b> InlineTrieMatches<'a, 'b> {
    pub(crate) fn new() -> Self {
        Self {
            inline: [const { None }; INLINE_TRIE_MATCHES],
            spilled: Vec::new(),
            len: 0,
        }
    }

    /// The recorded segments, in walk order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &TrieSubMatch<'a, 'b>> {
        self.inline[..self.len.min(INLINE_TRIE_MATCHES)]
            .iter()
            .flatten()
            .chain(self.spilled.iter())
    }
}

impl<'a, 'b> SubMatchSink<'a, 'b> for InlineTrieMatches<'a, 'b> {
    fn mark(&self) -> usize {
        self.len
    }

    fn rewind(&mut self, mark: usize) {
        debug_assert!(mark <= self.len, "a rewind never moves forward");
        // Inline slots past `len` are dead: `iter` never reads them and the
        // next `push` overwrites them, so only the spill needs truncating.
        self.spilled
            .truncate(mark.saturating_sub(INLINE_TRIE_MATCHES));
        self.len = mark;
    }

    fn push(&mut self, sub_match: TrieSubMatch<'a, 'b>) {
        match self.inline.get_mut(self.len) {
            Some(slot) => *slot = Some(sub_match),
            None => self.spilled.push(sub_match),
        }
        self.len += 1;
    }
}

impl<V: PartialEq> std::cmp::PartialEq for TrieNode<V> {
    fn eq(&self, other: &Self) -> bool {
        self.key_value == other.key_value
            && self.wildcard == other.wildcard
            && self.children == other.children
            && self.regexps.len() == other.regexps.len()
            && self
                .regexps
                .iter()
                .zip(other.regexps.iter())
                .fold(true, |b, (left, right)| {
                    b && left.0.as_str() == right.0.as_str() && left.1 == right.1
                })
    }
}

impl<V: Debug + Clone> TrieNode<V> {
    pub fn new(key: Key, value: V) -> TrieNode<V> {
        TrieNode {
            key_value: Some((key, value)),
            wildcard: None,
            children: HashMap::new(),
            regexps: Vec::new(),
        }
    }

    pub fn wildcard(key: Key, value: V) -> TrieNode<V> {
        TrieNode {
            key_value: None,
            wildcard: Some((key, value)),
            children: HashMap::new(),
            regexps: Vec::new(),
        }
    }

    pub fn root() -> TrieNode<V> {
        TrieNode {
            key_value: None,
            wildcard: None,
            children: HashMap::new(),
            regexps: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.key_value.is_none()
            && self.wildcard.is_none()
            && self.regexps.is_empty()
            && self.children.is_empty()
    }

    /// Store `value` in THIS node's own `key_value` slot: the value of a
    /// domain whose every segment the recursion has already consumed.
    ///
    /// Exact dual of the empty-`partial_key` arm of
    /// [`TrieNode::remove_recursive`], which clears the same slot. A free
    /// slot takes the value (`Ok`); an occupied one is `Existing`.
    ///
    /// Always ASK the slot, never infer it from how the node was built.
    /// A regex subtree opened by the `pos > 0` create-path of
    /// `insert_recursive` is a valueless [`TrieNode::root`] carrying only a
    /// deeper domain, while one opened by its `pos == 0` path is a
    /// value-bearing [`TrieNode::new`]. Both are reachable at the same
    /// `regexps` entry, so the two cannot be told apart from the outside.
    fn insert_own_value(&mut self, key: &Key, value: V) -> InsertResult {
        if self.key_value.is_none() {
            self.key_value = Some((key.to_vec(), value));
            InsertResult::Ok
        } else {
            InsertResult::Existing
        }
    }

    pub fn insert(&mut self, key: Key, value: V) -> InsertResult {
        //println!("insert: key == {}", std::str::from_utf8(&key).unwrap());
        if key.is_empty() {
            return InsertResult::Failed;
        }
        if key[..] == b"."[..] {
            return InsertResult::Failed;
        }

        #[cfg(debug_assertions)]
        let before = self.count_values();

        let insert_result = self.insert_recursive(&key, &key, value);

        // Post: the value count grows by exactly one on a fresh insert,
        // is unchanged when the key already existed, and is ALSO unchanged
        // on `Failed` -- every mutating step in `insert_recursive`
        // (`children.insert`, `regexps.push`) is guarded by an `Ok` from
        // the level below, so a rejected key can never leave a half-built
        // branch behind.
        //
        // `Failed` is NOT an internal-invariant break: the two cheap guards
        // above only rule out the empty key and a bare `.`, while
        // `insert_recursive` rejects a much wider class of malformed
        // domains (a key ending in `/` with no openable regex segment, a
        // regex segment that is not `.`-anchored, a segment that is not a
        // valid regex, an empty label from a leading or doubled `.`).
        // Those keys arrive from the control plane -- an `AddHttpFrontend`
        // over the command socket, or a `LoadState` replay -- so this must
        // stay a graceful rejection the caller reports. It used to be an
        // `assert_ne!`, which turned a malformed hostname into a worker
        // panic (and, on state replay, a restart loop).
        #[cfg(debug_assertions)]
        {
            let after = self.count_values();
            match insert_result {
                InsertResult::Ok => debug_assert_eq!(
                    after,
                    before + 1,
                    "a fresh insert must add exactly one value to the trie",
                ),
                InsertResult::Existing => debug_assert_eq!(
                    after, before,
                    "an Existing insert must not change the trie value count",
                ),
                InsertResult::Failed => debug_assert_eq!(
                    after, before,
                    "a failed insert must leave the trie value count untouched",
                ),
            }
            self.check_invariants();
        }

        insert_result
    }

    pub fn insert_recursive(&mut self, partial_key: &[u8], key: &Key, value: V) -> InsertResult {
        //println!("insert_rec: key == {}", std::str::from_utf8(partial_key).unwrap());
        // An empty `partial_key` means the caller handed us a key with an
        // empty label -- a leading `.` (`.example.com`) or a doubled one --
        // which the dot-split recursion below cannot consume any further.
        // Reject it like any other malformed domain instead of panicking:
        // this input comes from the control plane, not from Sozu itself.
        //
        // Only TWO paths can deliver an empty `partial_key`, and both mean
        // "empty label": the dot-split recursing on `partial_key[..pos]`
        // with `pos == 0` (`.example.com`, `..`), and the regex arm
        // recursing on `partial_key[..pos - 1]` with `pos == 1`, i.e. a
        // leading `.` before the segment (`./test[0-9]/.example.com`).
        //
        // So this arm is INPUT VALIDATION, not "the value belongs to this
        // node". Do not reroute a legitimate insert through it -- the
        // leftmost-regex case looks like it wants an empty key (that is how
        // `remove_recursive` reaches its own `key_value`), but taking it
        // here would make every leading-dot hostname insertable. It calls
        // `insert_own_value` directly instead.
        if partial_key.is_empty() {
            return InsertResult::Failed;
        }
        // `partial_key` is always a suffix of the full `key` being
        // inserted — the recursion only ever shrinks the head, never
        // rewrites the tail.
        debug_assert!(
            partial_key.len() <= key.len(),
            "insert recursion must consume the key, never grow past it",
        );

        if partial_key[partial_key.len() - 1] == b'/' {
            let pos = find_last_slash(&partial_key[..partial_key.len() - 1]);

            if let Some(pos) = pos {
                if pos > 0 && partial_key[pos - 1] != b'.' {
                    return InsertResult::Failed;
                }

                if let Ok(s) = str::from_utf8(&partial_key[pos + 1..partial_key.len() - 1]) {
                    // `None` is a segment that is not a regex on its own;
                    // see `anchored_segment` for why that is refused here
                    // rather than rescued by the wrapping.
                    let Some(anchored_s) = anchored_segment(s) else {
                        return InsertResult::Failed;
                    };
                    for t in self.regexps.iter_mut() {
                        if t.0.as_str() == anchored_s {
                            // `pos > 0`: there is a `.`-separated prefix
                            // before this regex segment; recurse on it
                            // (dropping the leading `.` via `pos - 1`).
                            // `pos == 0`: the regex is the leftmost/only
                            // segment, so the value belongs to THIS
                            // subtree's own `key_value` slot -- ask the
                            // slot via `insert_own_value`, the dual of the
                            // empty-key arm `remove_recursive` reaches for
                            // the very same host.
                            //
                            // Do NOT assume the subtree is value-bearing
                            // here. A `regexps` entry is ALSO opened by the
                            // `pos > 0` create-path below, which builds a
                            // valueless `TrieNode::root()` holding only the
                            // deeper domain. So inserting
                            // `foo./test[0-9]/.example.com` and then
                            // `/test[0-9]/.example.com` lands on this arm
                            // with `key_value == None`: answering `Existing`
                            // unconditionally (as this arm used to) stored
                            // nothing while reporting the host as already
                            // present, leaving it permanently un-insertable.
                            // In debug the router's post-insert reachability
                            // check (`router/mod.rs`) then killed the worker
                            // on an `AddHttpFrontend`; in release
                            // `add_tree_rule` returned `true` and
                            // `sozu query frontends` listed a route the trie
                            // never served -- control plane diverging from
                            // data plane, silently. The reverse insertion
                            // order always worked, which is why the gap
                            // stayed invisible.
                            //
                            // Older still, this arm did
                            // `partial_key[..pos - 1]` unconditionally,
                            // underflowing to `usize::MAX` and panicking on
                            // `pos == 0` (same latent bug as `lookup_mut`).
                            if pos > 0 {
                                return t.1.insert_recursive(&partial_key[..pos - 1], key, value);
                            }
                            return t.1.insert_own_value(key, value);
                        }
                    }

                    // `anchored_s` above is the very string the dedup scan
                    // just failed to find, so the entry this opens is
                    // addressable by `remove_recursive` and `lookup_mut`,
                    // which rebuild it the same way.
                    if let Some(r) = compiled_segment(s) {
                        if pos > 0 {
                            let mut node = TrieNode::root();
                            let pos = pos - 1;

                            let res = node.insert_recursive(&partial_key[..pos], key, value);

                            if res == InsertResult::Ok {
                                self.regexps.push((r, node));
                            }

                            return res;
                        } else {
                            let node = TrieNode::new(key.to_vec(), value);
                            self.regexps.push((r, node));
                            return InsertResult::Ok;
                        }
                    }
                }
            }

            return InsertResult::Failed;
        }

        let pos = find_last_dot(partial_key);
        match pos {
            None => {
                // Answering `Existing` off a bare `contains_key` is the
                // same shape as the regex arm above, and is sound only
                // because the two `children.insert` sites use DISJOINT key
                // spaces: this arm keys a `TrieNode::new` (value-bearing)
                // by a dotless `partial_key`, while the dot-split arm keys
                // a `TrieNode::root()` by `partial_key[pos..]`, which
                // always starts with `.`. A dotless probe therefore can
                // only ever find a value-bearing leaf, and such a leaf
                // never gains children (nothing recurses into it) nor
                // outlives its value (`remove_recursive` prunes it once
                // emptied). Break that disjointness and this arm acquires
                // the regex arm's bug.
                if self.children.contains_key(partial_key) {
                    InsertResult::Existing
                } else if partial_key == &b"*"[..] {
                    if self.wildcard.is_some() {
                        InsertResult::Existing
                    } else {
                        self.wildcard = Some((key.to_vec(), value));
                        InsertResult::Ok
                    }
                } else {
                    let node = TrieNode::new(key.to_vec(), value);
                    self.children.insert(partial_key.to_vec(), node);
                    InsertResult::Ok
                }
            }
            Some(pos) => {
                // The dot at `pos` is kept on the child key (suffix) and
                // stripped from the recursive prefix; the two slices
                // partition `partial_key` exactly.
                debug_assert_eq!(
                    partial_key[..pos].len() + partial_key[pos..].len(),
                    partial_key.len(),
                    "dot-split must partition partial_key without losing bytes",
                );
                debug_assert_eq!(
                    partial_key[pos], b'.',
                    "find_last_dot must point at a '.' byte",
                );
                if let Some(child) = self.children.get_mut(&partial_key[pos..]) {
                    return child.insert_recursive(&partial_key[..pos], key, value);
                }

                let mut node = TrieNode::root();
                let res = node.insert_recursive(&partial_key[..pos], key, value);

                if res == InsertResult::Ok {
                    self.children.insert(partial_key[pos..].to_vec(), node);
                }

                res
            }
        }
    }

    pub fn remove(&mut self, key: &Key) -> RemoveResult {
        #[cfg(debug_assertions)]
        let before = self.count_values();

        let remove_result = self.remove_recursive(key);

        // Post: a successful remove drops exactly one value; a NotFound
        // is a no-op on the value count. The structural invariants then
        // guarantee no emptied subtree was stranded by the prune.
        #[cfg(debug_assertions)]
        {
            let after = self.count_values();
            match remove_result {
                RemoveResult::Ok => debug_assert_eq!(
                    after + 1,
                    before,
                    "a successful remove must drop exactly one value from the trie",
                ),
                RemoveResult::NotFound => debug_assert_eq!(
                    after, before,
                    "a NotFound remove must not change the trie value count",
                ),
            }
            self.check_invariants();
        }

        remove_result
    }

    pub fn remove_recursive(&mut self, partial_key: &[u8]) -> RemoveResult {
        //println!("remove: key == {}", std::str::from_utf8(partial_key).unwrap());

        if partial_key.is_empty() {
            if self.key_value.is_some() {
                self.key_value = None;
                return RemoveResult::Ok;
            } else {
                return RemoveResult::NotFound;
            }
        }

        if partial_key == &b"*"[..] {
            if self.wildcard.is_some() {
                self.wildcard = None;
                return RemoveResult::Ok;
            } else {
                return RemoveResult::NotFound;
            }
        }

        if partial_key[partial_key.len() - 1] == b'/' {
            let pos = find_last_slash(&partial_key[..partial_key.len() - 1]);

            if let Some(pos) = pos {
                if pos > 0 && partial_key[pos - 1] != b'.' {
                    return RemoveResult::NotFound;
                }

                if let Ok(s) = str::from_utf8(&partial_key[pos + 1..partial_key.len() - 1]) {
                    // A segment `insert_recursive` refused stored nothing,
                    // so there is nothing here to remove either.
                    let Some(anchored_s) = anchored_segment(s) else {
                        return RemoveResult::NotFound;
                    };
                    // Mirror of `insert_recursive`. `pos > 0`: a
                    // `.`-separated prefix precedes the regex segment, so
                    // the value sits deeper in that segment's subtree —
                    // recurse on the prefix, dropping the separating `.`
                    // via `pos - 1`. `pos == 0`: the regex is the
                    // leftmost/only segment and the value IS that
                    // subtree's own `key_value`, reached with an empty key.
                    // That slot is asked, never assumed: a subtree opened
                    // by the `pos > 0` create-path of `insert_recursive` is
                    // a valueless `TrieNode::root()`, so the empty-key arm
                    // correctly answers `NotFound` for a leftmost host that
                    // only ever existed as a deeper domain's segment.
                    //
                    // Either way exactly one value goes. Dropping the
                    // whole `regexps` entry instead — which the `pos == 0`
                    // arm used to do with `retain` — also deletes every
                    // deeper domain sharing the segment, and reports `Ok`
                    // for a host that was never stored.
                    let rest: &[u8] = if pos > 0 {
                        &partial_key[..pos - 1]
                    } else {
                        &partial_key[..0]
                    };
                    // `check_invariants` keeps the anchored patterns
                    // unique per node, so at most one entry can match.
                    // `Vec::remove` (never `swap_remove`): `lookup` takes
                    // the FIRST matching regex, so the order of `regexps`
                    // is routing behaviour.
                    if let Some(index) = self
                        .regexps
                        .iter()
                        .position(|(r, _)| r.as_str() == anchored_s)
                        && self.regexps[index].1.remove_recursive(rest) == RemoveResult::Ok
                    {
                        // An emptied regex subtree must be pruned here,
                        // exactly like an emptied `children` subtree
                        // below: left behind it strands a valueless node
                        // and keeps every ancestor non-empty, so nothing
                        // above it could be pruned either.
                        if self.regexps[index].1.is_empty() {
                            self.regexps.remove(index);
                        }
                        return RemoveResult::Ok;
                    }
                }
            }

            return RemoveResult::NotFound;
        }

        let pos = find_last_dot(partial_key);
        let (prefix, suffix) = match pos {
            None => (&b""[..], partial_key),
            Some(pos) => (&partial_key[..pos], &partial_key[pos..]),
        };
        //println!("remove: prefix|suffix: {} | {}", std::str::from_utf8(prefix).unwrap(), std::str::from_utf8(suffix).unwrap());
        debug_assert_eq!(
            prefix.len() + suffix.len(),
            partial_key.len(),
            "dot-split must partition the key without losing or duplicating bytes",
        );

        match self.children.get_mut(suffix) {
            Some(child) => match child.remove_recursive(prefix) {
                RemoveResult::NotFound => RemoveResult::NotFound,
                RemoveResult::Ok => {
                    // An emptied child subtree MUST be pruned here so the
                    // parent never strands a node with no value. After the
                    // prune the suffix key is gone from `children`.
                    if child.is_empty() {
                        self.children.remove(suffix);
                        debug_assert!(
                            !self.children.contains_key(suffix),
                            "an emptied child subtree must be removed from the parent",
                        );
                    } else {
                        // `count_values` is debug-only; gate the whole
                        // assert so the call does not have to compile in
                        // release (HARD RULE 2 — E0425 guard).
                        #[cfg(debug_assertions)]
                        debug_assert!(
                            child.count_values() > 0,
                            "a retained child subtree must still hold at least one value",
                        );
                    }
                    RemoveResult::Ok
                }
            },
            None => RemoveResult::NotFound,
        }
    }

    /// Resolve `partial_key` against this subtree, MOST SPECIFIC FIRST.
    ///
    /// At every level the candidates are tried in one stated order:
    ///
    /// 1. the exact literal child,
    /// 2. the regex segments, in declaration order,
    /// 3. the `*` wildcard, which only ever stands for the leftmost label.
    ///
    /// A candidate that yields nothing hands the key to the next one: the
    /// walk BACKTRACKS instead of ending the lookup. `accept` is what
    /// "yields something" means to the caller — `Router::lookup` accepts a
    /// leaf only when one of its rules serves this request's path AND
    /// method, so a hostname candidate whose rules all reject the request
    /// is skipped rather than answered with a 404 (sozu#1351).
    ///
    /// The two fallbacks are NOT interchangeable, and which one a change
    /// may drop is not guessable — the mutation tests measure it.
    ///
    /// - The **regex-loop** fallback is what the reorder requires. The
    ///   wildcard used to be consulted BEFORE the regex list, so a segment
    ///   that matches a label but holds no value for this host
    ///   (`/cdn[0-9]+/`, as opened by `images./cdn[0-9]+/.hello.com`)
    ///   reached the wildcard only because the wildcard went first. Move
    ///   the wildcard after the regex list without this fallback and that
    ///   case becomes a miss:
    ///   `a_regex_segment_holding_no_value_falls_back_to_the_wildcard`.
    /// - The **exact-child** fallback answers to the add-path fix instead.
    ///   An exact hostname now owns its own node, carrying only the paths
    ///   written for it, so without this fallback every other path on that
    ///   host stops resolving. Dropping it does NOT redden the regex case
    ///   above; it reddens
    ///   `a_hostname_candidate_that_serves_no_rule_falls_through_to_the_next`
    ///   and the leak test.
    ///
    /// Both still ship together — the add fix and the reorder each need
    /// one — but a future reader deciding whether they can be split should
    /// read that pairing, not assume it.
    ///
    /// The walk visits each node at most once: the trie is a tree, so two
    /// candidates never share a node. The cost is therefore bounded by the
    /// number of nodes matching the key, not by the product of the
    /// per-level candidate counts.
    fn lookup_recursive<'a, 'b, S: SubMatchSink<'a, 'b>>(
        &'b self,
        partial_key: &'a [u8],
        accept_wildcard: bool,
        trace: &mut S,
        accept: &mut dyn FnMut(&KeyValue<Key, V>) -> bool,
    ) -> Option<&'b KeyValue<Key, V>> {
        if partial_key.is_empty() {
            return match self.key_value.as_ref() {
                Some(key_value) if accept(key_value) => Some(key_value),
                _ => None,
            };
        }

        let pos = find_last_dot(partial_key);
        let (prefix, suffix) = match pos {
            None => (&b""[..], partial_key),
            Some(pos) => (&partial_key[..pos], &partial_key[pos..]),
        };
        // The dot-split partitions the key exactly: prefix ++ suffix is
        // the whole input, and a dotted split puts the `.` at the head
        // of the suffix (this is the byte the wildcard/regex arms strip).
        debug_assert_eq!(
            prefix.len() + suffix.len(),
            partial_key.len(),
            "dot-split must partition the key without losing or duplicating bytes",
        );
        debug_assert!(
            !suffix.is_empty(),
            "the suffix the trie matches children against must be non-empty",
        );
        debug_assert!(
            pos.is_none() || suffix.first() == Some(&b'.'),
            "a dotted split must place the separator at the head of the suffix",
        );

        // 1. the exact literal child.
        if let Some(child) = self.children.get(suffix) {
            let mark = trace.mark();
            if let Some(found) = child.lookup_recursive(prefix, accept_wildcard, trace, accept) {
                return Some(found);
            }
            trace.rewind(mark);
        }

        // The bytes a non-literal segment stands for: `suffix` without the
        // separator the dot-split kept at its head.
        let segment = if suffix[0] == b'.' {
            &suffix[1..]
        } else {
            suffix
        };

        // 2. the regex segments, in declaration order.
        for (regexp, child) in self.regexps.iter() {
            if !regexp.is_match(segment) {
                continue;
            }
            let mark = trace.mark();
            trace.push(TrieSubMatch::Regexp(segment, regexp));
            if let Some(found) = child.lookup_recursive(prefix, accept_wildcard, trace, accept) {
                return Some(found);
            }
            trace.rewind(mark);
        }

        // 3. the wildcard, which stands for one leftmost label, and only
        //    when the caller accepts one.
        if prefix.is_empty()
            && accept_wildcard
            && let Some(key_value) = self.wildcard.as_ref()
            && accept(key_value)
        {
            trace.push(TrieSubMatch::Wildcard(segment));
            return Some(key_value);
        }

        None
    }

    /// Look up `partial_key` and additionally collect the non-literal segments
    /// that matched along the way (`TrieMatches`).
    ///
    /// Equivalent to `lookup` for callers that don't need the captures, but
    /// frontends with `$HOST[n]` rewrite templates need the matched segments
    /// to fill the placeholders. The accumulator is passed in by value so
    /// callers can pre-size it (`Vec::with_capacity`) and we own the path
    /// returned alongside the value. It carries the segments of the
    /// candidate that ANSWERED: the walk rewinds the ones it tried and
    /// abandoned.
    ///
    /// `accept` filters candidates — see `TrieNode::lookup_recursive`
    /// for the precedence order it is applied in. Pass
    /// `&mut |_: &KeyValue<Key, V>| true` to take the first leaf found.
    pub fn lookup_with_path<'a, 'b>(
        &'b self,
        partial_key: &'a [u8],
        accept_wildcard: bool,
        mut trace: TrieMatches<'a, 'b>,
        accept: &mut dyn FnMut(&KeyValue<Key, V>) -> bool,
    ) -> Option<(&'b KeyValue<Key, V>, TrieMatches<'a, 'b>)> {
        self.lookup_recursive(partial_key, accept_wildcard, &mut trace, accept)
            .map(|key_value| (key_value, trace))
    }

    /// [`TrieNode::lookup_with_path`] recording into a caller-owned
    /// [`InlineTrieMatches`], so a lookup that records at most
    /// [`INLINE_TRIE_MATCHES`] segments allocates nothing. `trace` must be
    /// empty; it holds the segments of the candidate that answered.
    pub(crate) fn lookup_with_inline_path<'a, 'b>(
        &'b self,
        partial_key: &'a [u8],
        accept_wildcard: bool,
        trace: &mut InlineTrieMatches<'a, 'b>,
        accept: &mut dyn FnMut(&KeyValue<Key, V>) -> bool,
    ) -> Option<&'b KeyValue<Key, V>> {
        debug_assert_eq!(trace.mark(), 0, "a lookup starts from an empty record");
        self.lookup_recursive(partial_key, accept_wildcard, trace, accept)
    }

    /// Request-addressed lookup: which entry serves `partial_key`.
    ///
    /// Same walk and same precedence as [`TrieNode::lookup_with_path`] —
    /// stated once, in `TrieNode::lookup_recursive` — without recording
    /// the segments it matched. This is the resolver SNI goes through
    /// (`lib/src/tls.rs`, `lib/src/protocol/tcp_preread/`), so certificate
    /// selection follows the same order as HTTP routing.
    pub fn lookup(&self, partial_key: &[u8], accept_wildcard: bool) -> Option<&KeyValue<Key, V>> {
        self.lookup_recursive(
            partial_key,
            accept_wildcard,
            &mut NoTrace,
            &mut |_: &KeyValue<Key, V>| true,
        )
    }

    pub fn lookup_mut(
        &mut self,
        partial_key: &[u8],
        accept_wildcard: bool,
    ) -> Option<&mut KeyValue<Key, V>> {
        //println!("lookup: key == {}", std::str::from_utf8(partial_key).unwrap());

        if partial_key.is_empty() {
            return self.key_value.as_mut();
        }

        if partial_key == &b"*"[..] {
            return self.wildcard.as_mut();
        }

        if partial_key[partial_key.len() - 1] == b'/' {
            let pos = find_last_slash(&partial_key[..partial_key.len() - 1]);

            if let Some(pos) = pos {
                if pos > 0 && partial_key[pos - 1] != b'.' {
                    return None;
                }

                if let Ok(s) = str::from_utf8(&partial_key[pos + 1..partial_key.len() - 1]) {
                    // A segment `insert_recursive` refused was never stored,
                    // so it is addressable by nothing here.
                    let anchored_s = anchored_segment(s)?;
                    for t in self.regexps.iter_mut() {
                        if t.0.as_str() == anchored_s {
                            // `pos == 0` means the regex is the leftmost
                            // segment, so the value is that subtree's own
                            // `key_value`, reached via the empty-prefix
                            // recursion (`lookup_mut(b"")` returns
                            // `key_value`). The subtree is NOT necessarily
                            // value-bearing — the `pos > 0` create-path of
                            // `insert_recursive` opens a valueless
                            // `TrieNode::root()` — but nothing here assumes
                            // it is: the empty-key arm returns the `Option`
                            // as it finds it, which is `None` exactly when
                            // the leftmost host was never stored. The
                            // pre-fix `partial_key[..pos - 1]` underflowed to
                            // `usize::MAX` and panicked on `pos == 0` — the
                            // same latent bug as the insert dedup loop. Drop
                            // the leading `.` only when there is one.
                            let rest = if pos > 0 {
                                &partial_key[..pos - 1]
                            } else {
                                &partial_key[..0]
                            };
                            return t.1.lookup_mut(rest, accept_wildcard);
                        }
                    }
                }
            }

            return None;
        }

        let pos = find_last_dot(partial_key);
        let (prefix, suffix) = match pos {
            None => (&b""[..], partial_key),
            Some(pos) => (&partial_key[..pos], &partial_key[pos..]),
        };
        //println!("lookup: prefix|suffix: {} | {}", std::str::from_utf8(prefix).unwrap(), std::str::from_utf8(suffix).unwrap());
        debug_assert_eq!(
            prefix.len() + suffix.len(),
            partial_key.len(),
            "dot-split must partition the key without losing or duplicating bytes",
        );
        debug_assert!(
            !suffix.is_empty(),
            "the suffix the trie matches children against must be non-empty",
        );

        match self.children.get_mut(suffix) {
            Some(child) => child.lookup_mut(prefix, accept_wildcard),
            None => {
                //println!("no child found, testing wildcard and regexps");

                if prefix.is_empty() && self.wildcard.is_some() && accept_wildcard {
                    //println!("no dot, wildcard applies");
                    self.wildcard.as_mut()
                } else {
                    //println!("there's still a subdomain, wildcard does not apply");

                    // A literal segment resolves through a matching regex
                    // segment only when the caller accepts a non-literal
                    // entry for its key -- the guard `insert_sni_route`
                    // (`lib/src/tcp.rs`) already documents for the wildcard
                    // slot right above, extended to the regex list.
                    //
                    // `lookup_mut` addresses a KEY, not a request: it is
                    // how `add_tree_rule`/`remove_tree_rule` and
                    // `insert_sni_route`/`remove_sni_route` reach the node
                    // a configured hostname owns, and both its `*` case and
                    // its trailing-`/` case above match by IDENTITY. This
                    // arm did not, so adding `test4.example.com` after
                    // `/test[0-9]/.example.com` pushed the exact host's
                    // rule onto the REGEX segment's leaf and the whole
                    // family served it -- a routing leak the operator
                    // could not see, since `sozu query frontends` showed
                    // exactly what they had typed (sozu#1351). The
                    // request-addressed resolvers (`lookup`,
                    // `lookup_with_path`) are where a literal host is meant
                    // to match a regex segment.
                    if !accept_wildcard {
                        return None;
                    }

                    for &mut (ref regexp, ref mut child) in self.regexps.iter_mut() {
                        let suffix = if suffix[0] == b'.' {
                            &suffix[1..]
                        } else {
                            suffix
                        };
                        //println!("testing regexp: {} on suffix {}", r.as_str(), str::from_utf8(s).unwrap());

                        if regexp.is_match(suffix) {
                            //println!("matched");
                            return child.lookup_mut(prefix, accept_wildcard);
                        }
                    }

                    None
                }
            }
        }
    }

    pub fn print(&self) {
        self.print_recursive(b"", 0)
    }

    pub fn print_recursive(&self, partial_key: &[u8], indent: u8) {
        let raw_prefix: Vec<u8> = iter::repeat_n(b' ', 2 * indent as usize).collect();
        let prefix = str::from_utf8(&raw_prefix).unwrap();

        print!("{}{}: ", prefix, str::from_utf8(partial_key).unwrap());
        if let Some((ref key, ref value)) = self.key_value {
            print!("({}, {:?}) | ", str::from_utf8(key).unwrap(), value);
        } else {
            print!("None | ");
        }

        if let Some((key, value)) = &self.wildcard {
            println!("({}, {:?})", str::from_utf8(key).unwrap(), value);
        } else {
            println!("None");
        }

        for (child_key, child) in self.children.iter() {
            child.print_recursive(child_key, indent + 1);
        }

        for (regexp, child) in self.regexps.iter() {
            //print!("{}{}:", prefix, regexp.as_str());
            child.print_recursive(regexp.as_str().as_bytes(), indent + 1);
        }
    }

    /// Visit every stored value in the trie (the literal `key_value` and
    /// the leftmost `wildcard` slot of every node, plus all
    /// regex-subtree leaves) and invoke `f` on each. Used by the router
    /// to walk all routes for cross-cutting refreshes (e.g. listener-
    /// default HSTS reflow) without rebuilding the trie.
    pub fn for_each_value_mut<F: FnMut(&mut V)>(&mut self, f: &mut F) {
        if let Some((_, ref mut value)) = self.key_value {
            f(value);
        }
        if let Some((_, ref mut value)) = self.wildcard {
            f(value);
        }
        for child in self.children.values_mut() {
            child.for_each_value_mut(f);
        }
        for (_, child) in self.regexps.iter_mut() {
            child.for_each_value_mut(f);
        }
    }

    /// Count every value slot reachable from this node: the literal
    /// `key_value`, the leftmost `wildcard`, plus all values stored in
    /// child subtrees and regex subtrees. Used only by the
    /// `#[cfg(debug_assertions)]` invariant checks as the leaf-count
    /// accounting (`inserts − removes`); never called in release.
    #[cfg(debug_assertions)]
    fn count_values(&self) -> usize {
        let local = self.key_value.is_some() as usize + self.wildcard.is_some() as usize;
        let in_children: usize = self.children.values().map(TrieNode::count_values).sum();
        let in_regexps: usize = self.regexps.iter().map(|(_, c)| c.count_values()).sum();
        local + in_children + in_regexps
    }

    /// Full structural invariant sweep for the trie, asserted as a
    /// run-to-completion postcondition at the end of every mutating
    /// public operation. Encodes the cross-field invariants that the
    /// recursive insert/remove logic must preserve:
    ///
    /// - **No stranded interior node**: every non-root node reachable
    ///   through `children` / `regexps` must hold a value somewhere in
    ///   its subtree (`!is_empty()` and `count_values() > 0`). A node
    ///   that holds neither a value nor any descendant value is a leak —
    ///   `remove_recursive` is supposed to prune it via `is_empty()`.
    /// - **Unique regex segments**: the anchored pattern strings stored
    ///   in `regexps` are unique within a node (insert dedups by
    ///   `as_str()` before pushing a new subtree).
    /// - **Child-key invariant**: no child is keyed by the empty slice.
    ///
    /// `debug_assertions`-only; compiled out of release builds.
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        // Regex segment patterns are unique per node.
        for i in 0..self.regexps.len() {
            for j in (i + 1)..self.regexps.len() {
                debug_assert_ne!(
                    self.regexps[i].0.as_str(),
                    self.regexps[j].0.as_str(),
                    "trie node must not hold two subtrees for the same regex segment",
                );
            }
        }

        for (child_key, child) in self.children.iter() {
            debug_assert!(
                !child_key.is_empty(),
                "trie child must not be keyed by the empty segment",
            );
            // A child subtree that has been fully emptied must have been
            // pruned by remove_recursive; reaching it here means a
            // subtree was stranded.
            debug_assert!(
                !child.is_empty(),
                "trie must not strand an empty child subtree (remove must prune)",
            );
            debug_assert!(
                child.count_values() > 0,
                "trie child subtree must lead to at least one value",
            );
            child.check_invariants();
        }

        for (_, child) in self.regexps.iter() {
            debug_assert!(
                !child.is_empty(),
                "trie must not strand an empty regex subtree (remove must prune)",
            );
            debug_assert!(
                child.count_values() > 0,
                "trie regex subtree must lead to at least one value",
            );
            child.check_invariants();
        }
    }

    pub fn domain_insert(&mut self, key: Key, value: V) -> InsertResult {
        self.insert(key, value)
    }

    pub fn domain_remove(&mut self, key: &Key) -> RemoveResult {
        self.remove(key)
    }

    pub fn domain_lookup(&self, key: &[u8], accept_wildcard: bool) -> Option<&KeyValue<Key, V>> {
        self.lookup(key, accept_wildcard)
    }

    /// Key-addressed mutable accessor: the node the configured hostname
    /// `key` OWNS, never the node that would serve a request for it.
    ///
    /// With `accept_wildcard: false` a literal key may not resolve into a
    /// non-literal entry — neither the `*` slot nor a matching regex
    /// segment — which is what keeps `add_tree_rule` and `insert_sni_route`
    /// from pushing a rule onto an entry that serves a whole family of
    /// hosts (sozu#1351). Use [`TrieNode::domain_lookup`] to ask which
    /// entry serves a host.
    pub fn domain_lookup_mut(
        &mut self,
        key: &[u8],
        accept_wildcard: bool,
    ) -> Option<&mut KeyValue<Key, V>> {
        self.lookup_mut(key, accept_wildcard)
    }

    pub fn size(&self) -> usize {
        ::std::mem::size_of::<TrieNode<V>>()
            + ::std::mem::size_of::<Option<KeyValue<Key, V>>>() * 2
            + self
                .children
                .iter()
                .fold(0, |acc, c| acc + c.0.len() + c.1.size())
    }

    pub fn to_hashmap(&self) -> HashMap<Key, V> {
        let mut h = HashMap::new();

        self.to_hashmap_recursive(&mut h);

        h
    }

    pub fn to_hashmap_recursive(&self, h: &mut HashMap<Key, V>) {
        if let Some((key, value)) = &self.key_value {
            h.insert(key.clone(), value.clone());
        }

        if let Some((key, value)) = &self.wildcard {
            h.insert(key.clone(), value.clone());
        }

        for child in self.children.values() {
            child.to_hashmap_recursive(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quickcheck::quickcheck;

    #[test]
    fn insert() {
        let mut root: TrieNode<u8> = TrieNode::root();
        root.print();

        assert_eq!(
            root.domain_insert(Vec::from(&b"abcd"[..]), 1),
            InsertResult::Ok
        );
        root.print();
        assert_eq!(
            root.domain_insert(Vec::from(&b"abce"[..]), 2),
            InsertResult::Ok
        );
        root.print();
        assert_eq!(
            root.domain_insert(Vec::from(&b"abgh"[..]), 3),
            InsertResult::Ok
        );
        root.print();

        assert_eq!(
            root.domain_lookup(&b"abce"[..], true),
            Some(&(b"abce"[..].to_vec(), 2))
        );
        //assert!(false);
    }

    #[test]
    fn remove() {
        let mut root: TrieNode<u8> = TrieNode::root();
        println!("creating root:");
        root.print();

        println!("adding (abcd, 1)");
        assert_eq!(root.insert(Vec::from(&b"abcd"[..]), 1), InsertResult::Ok);
        root.print();
        println!("adding (abce, 2)");
        assert_eq!(root.insert(Vec::from(&b"abce"[..]), 2), InsertResult::Ok);
        root.print();
        println!("adding (abgh, 3)");
        assert_eq!(root.insert(Vec::from(&b"abgh"[..]), 3), InsertResult::Ok);
        root.print();

        let mut root2: TrieNode<u8> = TrieNode::root();

        assert_eq!(root2.insert(Vec::from(&b"abcd"[..]), 1), InsertResult::Ok);
        assert_eq!(root2.insert(Vec::from(&b"abgh"[..]), 3), InsertResult::Ok);

        println!("before remove");
        root.print();
        assert_eq!(root.remove(&Vec::from(&b"abce"[..])), RemoveResult::Ok);
        println!("after remove");
        root.print();

        println!("expected");
        root2.print();
        assert_eq!(root, root2);

        assert_eq!(root.remove(&Vec::from(&b"abgh"[..])), RemoveResult::Ok);
        println!("after remove");
        root.print();
        println!("expected");
        let mut root3: TrieNode<u8> = TrieNode::root();
        assert_eq!(root3.insert(Vec::from(&b"abcd"[..]), 1), InsertResult::Ok);
        root3.print();
        assert_eq!(root, root3);
    }

    #[test]
    fn insert_remove_through_regex() {
        let mut root: TrieNode<u8> = TrieNode::root();
        println!("creating root:");
        root.print();

        println!("adding (www./.*/.com, 1)");
        assert_eq!(
            root.insert(Vec::from(&b"www./.*/.com"[..]), 1),
            InsertResult::Ok
        );
        root.print();
        println!("adding (www.doc./.*/.com, 2)");
        assert_eq!(
            root.insert(Vec::from(&b"www.doc./.*/.com"[..]), 2),
            InsertResult::Ok
        );
        root.print();
        assert_eq!(
            root.domain_lookup(b"www.sozu.com".as_ref(), false),
            Some(&(b"www./.*/.com".to_vec(), 1))
        );
        assert_eq!(
            root.domain_lookup(b"www.doc.sozu.com".as_ref(), false),
            Some(&(b"www.doc./.*/.com".to_vec(), 2))
        );

        assert_eq!(
            root.domain_remove(&b"www./.*/.com".to_vec()),
            RemoveResult::Ok
        );
        root.print();
        assert_eq!(root.domain_lookup(b"www.sozu.com".as_ref(), false), None);
        assert_eq!(
            root.domain_lookup(b"www.doc.sozu.com".as_ref(), false),
            Some(&(b"www.doc./.*/.com".to_vec(), 2))
        );
    }

    /// Segment regexes must match the entire segment, not just a prefix.
    /// Without `\A...\z` anchoring the previous behaviour matched any
    /// segment whose prefix satisfied the pattern, silently widening the
    /// routing surface. This regression test exercises the exact-match
    /// invariant that anchoring guarantees.
    /// `insert` must REJECT a malformed domain, never panic. These keys
    /// come from the control plane (`AddHttpFrontend`, or a `LoadState`
    /// replay), so an `assert_ne!(_, Failed)` here -- which is what this
    /// used to be -- turned a bad hostname into a worker crash, repeated
    /// on every restart replay. A rejected key must also leave the trie
    /// completely untouched.
    #[test]
    fn insert_rejects_malformed_keys_without_panicking() {
        for key in [
            &b""[..],
            b".",
            b"..",
            b"...",
            b"/",
            b"///",
            b"a/",
            b".a",
            b".a.b",
            b"example.com/",
            b"www.example.com/",
            // A regex segment must be `.`-anchored on its left.
            b"abc/[0-9]+/.example.com",
            // ... and must actually compile as a regex.
            b"/[/.example.com",
            b"a/*/",
        ] {
            let mut root: TrieNode<u8> = TrieNode::root();
            assert_eq!(
                root.insert(key.to_vec(), 1),
                InsertResult::Failed,
                "{:?} must be rejected",
                String::from_utf8_lossy(key),
            );
            assert!(
                root.is_empty(),
                "{:?} was rejected but still mutated the trie",
                String::from_utf8_lossy(key),
            );
            assert_eq!(root.domain_lookup(key, false), None);
            assert_eq!(root.domain_lookup(key, true), None);
        }
    }

    /// A rejected key must not disturb the entries already in the trie
    /// either -- the failed insert has to be a no-op on a populated table.
    #[test]
    fn a_rejected_insert_leaves_existing_entries_intact() {
        let mut root: TrieNode<u8> = TrieNode::root();
        assert_eq!(
            root.insert(b"www.example.com".to_vec(), 1),
            InsertResult::Ok
        );
        assert_eq!(root.insert(b"*.wild.com".to_vec(), 2), InsertResult::Ok);
        assert_eq!(
            root.insert(b"www.example.com/".to_vec(), 3),
            InsertResult::Failed
        );
        assert_eq!(
            root.domain_lookup(b"www.example.com", false),
            Some(&(b"www.example.com".to_vec(), 1))
        );
        assert_eq!(
            root.domain_lookup(b"any.wild.com", true),
            Some(&(b"*.wild.com".to_vec(), 2))
        );
    }

    #[test]
    fn segment_regex_rejects_partial_matches() {
        let mut root: TrieNode<u8> = TrieNode::root();
        // The regex segment `cdn[0-9]+` must match `cdn1`, `cdn99`, etc.
        // exactly — never `cdn1xxx` or `xxxcdn1` as a prefix/suffix.
        assert_eq!(
            root.insert(Vec::from(&b"/cdn[0-9]+/.example.com"[..]), 7),
            InsertResult::Ok
        );

        // Exact-match cases still resolve.
        assert_eq!(
            root.domain_lookup(b"cdn1.example.com".as_ref(), false),
            Some(&(b"/cdn[0-9]+/.example.com".to_vec(), 7))
        );
        assert_eq!(
            root.domain_lookup(b"cdn123.example.com".as_ref(), false),
            Some(&(b"/cdn[0-9]+/.example.com".to_vec(), 7))
        );

        // Trailing characters past the digit run must fail. Pre-anchoring
        // the trie would have matched `cdn1xxx` because `cdn[0-9]+` ate
        // the `cdn1` prefix; with `\A...\z` the segment is rejected.
        assert_eq!(
            root.domain_lookup(b"cdn1xxx.example.com".as_ref(), false),
            None
        );
        // Leading characters likewise must fail.
        assert_eq!(
            root.domain_lookup(b"xxxcdn1.example.com".as_ref(), false),
            None
        );
        // Non-digit middle bytes break the digit run and the segment.
        assert_eq!(
            root.domain_lookup(b"cdnabc.example.com".as_ref(), false),
            None
        );
    }

    /// `anchored_segment` is the single source of a `regexps` entry's
    /// identity, so its exact output is pinned here rather than inferred
    /// from routing verdicts: `insert_recursive`'s dedup scan,
    /// `remove_recursive` and `lookup_mut` all address an entry by comparing
    /// `Regex::as_str()` against this string, and a second spelling would
    /// make a stored segment un-removable and un-addressable.
    ///
    /// The group is non-capturing, so `captures_len` — and therefore every
    /// `$HOST[n]` rewrite index `lookup_with_path` feeds
    /// `router/mod.rs`'s `RouteResult::new_with_trie` — is unchanged.
    ///
    /// To SEE THIS RED: build `format!("\\A{segment}\\z")` in
    /// `anchored_segment`, the form this tree carried up to 2.2.1, and relax
    /// the `debug_assert!` beside it to `starts_with("\\A")` /
    /// `ends_with("\\z")` — in a debug build that assertion fires first, in
    /// `anchored_segment` itself rather than in a test. The first assertion
    /// here then fails with
    /// `left: Some("\\Acdn[0-9]+\\z"), right: Some("\\A(?:cdn[0-9]+)\\z")`.
    /// Make the wrapper CAPTURING instead — `format!("\\A({segment})\\z")` —
    /// and the `captures_len` assertions catch it.
    #[test]
    fn anchored_segment_wraps_in_a_non_capturing_group() {
        assert_eq!(
            anchored_segment("cdn[0-9]+").as_deref(),
            Some("\\A(?:cdn[0-9]+)\\z"),
            "the wrapper must anchor AND group",
        );
        assert_eq!(
            Regex::new(&anchored_segment("a|b|c").expect("an alternation must wrap"))
                .expect("the wrapped alternation must compile")
                .captures_len(),
            1,
            "the group must be non-capturing, so `$HOST[n]` indices are untouched",
        );
        assert_eq!(
            Regex::new(&anchored_segment("cdn([0-9]+)").expect("a group must wrap"))
                .expect("the wrapped group must compile")
                .captures_len(),
            2,
            "the operator's own group must stay at index 1",
        );

        // A segment that is not a regex on its own is refused, and is NOT
        // rescued by the `(` and `)` the wrapper supplies: `a)(b` is
        // unbalanced, yet `\A(?:a)(b)\z` compiles and matches `ab`.
        // Assembled at run time because the invalidity is the point.
        let unbalanced = format!("a{}b", ")(");
        assert_eq!(
            anchored_segment(&unbalanced),
            None,
            "{unbalanced} must not be rescued into a valid pattern",
        );
        let rescued = Regex::new(&format!("\\A(?:a{}b)\\z", ")("))
            .expect("the wrapping balances the segment");
        assert!(
            rescued.is_match(b"ab"),
            "{} is what the missing pre-validation would have built",
            rescued.as_str(),
        );
    }

    /// sozu#1377 moved the case folding of a tree hostname regex OFF the
    /// source and ONTO the compile: `Router::add_tree_rule` no longer
    /// hands the segment to `idna::domain_to_ascii` (which folded `\D`
    /// into `\d` and inverted the class), and `compiled_segment` folds
    /// here instead, where it reaches literals only.
    ///
    /// Three claims, and all three are load-bearing:
    ///
    /// - the builder must not RESPELL the pattern, because `as_str()` is
    ///   the identity `insert_recursive`, `remove_recursive` and
    ///   `lookup_mut` address a `regexps` entry by — a second spelling
    ///   makes a stored segment un-removable;
    /// - `captures_len` must be untouched, because `$HOST[n]` rewrite
    ///   indices come from `caps.iter().skip(1)` with the buffer sized by
    ///   `captures_len()` in `router/mod.rs`'s `RouteResult::new_with_trie`;
    /// - the fold must reach literals and NOT escapes.
    ///
    /// To SEE THIS RED: drop `.case_insensitive(true)` from
    /// `compiled_segment` — the `API[0-9]` assertion fails. Prepend
    /// `(?i)` to the pattern handed to `RegexBuilder` instead of setting
    /// the flag — the `as_str()` assertion fails with
    /// `left: "(?i)\\A(?:cdn[0-9]+)\\z", right: "\\A(?:cdn[0-9]+)\\z"`.
    /// Make `anchored_segment`'s wrapper capturing — the `captures_len`
    /// assertions fail.
    #[test]
    fn compiled_segment_folds_case_without_respelling_its_identity() {
        let anchored = anchored_segment("cdn[0-9]+").expect("the segment must wrap");
        assert_eq!(
            compiled_segment("cdn[0-9]+")
                .expect("the segment must compile")
                .as_str(),
            anchored,
            "the compiled entry must carry `anchored_segment`'s exact string",
        );

        assert_eq!(
            compiled_segment("a|b|c")
                .expect("an alternation must compile")
                .captures_len(),
            1,
            "folding must not add a capture group",
        );
        assert_eq!(
            compiled_segment("cdn([0-9]+)")
                .expect("a group must compile")
                .captures_len(),
            2,
            "the operator's own group must stay at index 1",
        );

        let uppercase = compiled_segment("API[0-9]").expect("the segment must compile");
        assert!(
            uppercase.is_match(b"api7"),
            "an uppercase literal must meet the ASCII-lowercased lookup key",
        );
        assert!(uppercase.is_match(b"API7"), "and its own spelling too",);
        assert!(
            !uppercase.is_match(b"apix"),
            "folding must not widen the class the operator wrote",
        );

        // Case insensitivity does not reach `\d`/`\D`, `\w`/`\W` or
        // `\s`/`\S`: that inversion only ever came from folding the
        // SOURCE, which is what sozu#1377 removed.
        let not_a_digit = compiled_segment("\\D+").expect("the segment must compile");
        assert!(not_a_digit.is_match(b"abc"), "`\\D+` matches non-digits");
        assert!(
            !not_a_digit.is_match(b"777"),
            "`\\D+` must never match digits — folded to `\\d+` it matched \
             exactly and only those",
        );

        // A segment that is not a regex on its own compiles to nothing,
        // exactly as `anchored_segment` refuses it.
        let unbalanced = format!("a{}b", ")(");
        assert!(
            compiled_segment(&unbalanced).is_none(),
            "{unbalanced} must not be rescued into a live entry",
        );
    }

    /// Three branches, because two cannot express the claim: with exactly
    /// two, "first and last" is every branch there is. Ungrouped, the MIDDLE
    /// branch keeps NEITHER anchor and matches as a bare substring anywhere
    /// in the label.
    ///
    /// To SEE THIS RED: build `format!("\\A{segment}\\z")` in
    /// `anchored_segment` and relax the `debug_assert!` beside it, which in
    /// a debug build fires first. `axx.example.com` then resolves and the
    /// assertion fails with
    /// `left: Some(([47, 97, 124, 98, 124, 99, 47, 46, …], 9)), right: None`.
    #[test]
    fn every_branch_of_an_alternating_segment_regex_matches_the_whole_label_only() {
        let mut root: TrieNode<u8> = TrieNode::root();
        assert_eq!(
            root.insert(Vec::from(&b"/a|b|c/.example.com"[..]), 9),
            InsertResult::Ok
        );

        for whole in ["a.example.com", "b.example.com", "c.example.com"] {
            assert_eq!(
                root.domain_lookup(whole.as_bytes(), false),
                Some(&(b"/a|b|c/.example.com".to_vec(), 9)),
                "{whole} is a whole-label match for one branch and must resolve",
            );
        }
        for leak in [
            "axx.example.com",
            "xxc.example.com",
            "zzbzz.example.com",
            "bzz.example.com",
            "zzb.example.com",
        ] {
            assert_eq!(
                root.domain_lookup(leak.as_bytes(), false),
                None,
                "{leak} matches no branch WHOLE and must not resolve",
            );
        }
    }

    #[test]
    fn add_child_to_leaf() {
        let mut root1: TrieNode<u8> = TrieNode::root();

        println!("creating root1:");
        root1.print();
        println!("adding (abcd, 1)");
        assert_eq!(root1.insert(Vec::from(&b"abcd"[..]), 1), InsertResult::Ok);
        root1.print();
        println!("adding (abce, 2)");
        assert_eq!(root1.insert(Vec::from(&b"abce"[..]), 2), InsertResult::Ok);
        root1.print();
        println!("adding (abc, 3)");
        assert_eq!(root1.insert(Vec::from(&b"abc"[..]), 3), InsertResult::Ok);

        println!("root1:");
        root1.print();

        let mut root2: TrieNode<u8> = TrieNode::root();

        assert_eq!(root2.insert(Vec::from(&b"abc"[..]), 3), InsertResult::Ok);
        assert_eq!(root2.insert(Vec::from(&b"abcd"[..]), 1), InsertResult::Ok);
        assert_eq!(root2.insert(Vec::from(&b"abce"[..]), 2), InsertResult::Ok);

        println!("root2:");
        root2.print();
        assert_eq!(root2.remove(&Vec::from(&b"abc"[..])), RemoveResult::Ok);

        println!("root2 after,remove:");
        root2.print();
        let mut expected: TrieNode<u8> = TrieNode::root();

        assert_eq!(
            expected.insert(Vec::from(&b"abcd"[..]), 1),
            InsertResult::Ok
        );
        assert_eq!(
            expected.insert(Vec::from(&b"abce"[..]), 2),
            InsertResult::Ok
        );

        println!("root2 after insert");
        root2.print();
        println!("expected");
        expected.print();
        assert_eq!(root2, expected);
    }

    #[test]
    fn domains() {
        let mut root: TrieNode<u8> = TrieNode::root();
        root.print();

        assert_eq!(
            root.domain_insert(Vec::from(&b"www.example.com"[..]), 1),
            InsertResult::Ok
        );
        root.print();
        assert_eq!(
            root.domain_insert(Vec::from(&b"test.example.com"[..]), 2),
            InsertResult::Ok
        );
        root.print();
        assert_eq!(
            root.domain_insert(Vec::from(&b"*.alldomains.org"[..]), 3),
            InsertResult::Ok
        );
        root.print();
        assert_eq!(
            root.domain_insert(Vec::from(&b"alldomains.org"[..]), 4),
            InsertResult::Ok
        );
        assert_eq!(
            root.domain_insert(Vec::from(&b"pouet.alldomains.org"[..]), 5),
            InsertResult::Ok
        );
        root.print();
        assert_eq!(
            root.domain_insert(Vec::from(&b"hello.com"[..]), 6),
            InsertResult::Ok
        );
        assert_eq!(
            root.domain_insert(Vec::from(&b"*.hello.com"[..]), 7),
            InsertResult::Ok
        );
        assert_eq!(
            root.domain_insert(Vec::from(&b"images./cdn[0-9]+/.hello.com"[..]), 8),
            InsertResult::Ok
        );
        root.print();
        assert_eq!(
            root.domain_insert(Vec::from(&b"/test[0-9]+/.www.hello.com"[..]), 9),
            InsertResult::Ok
        );
        root.print();

        assert_eq!(root.domain_lookup(&b"example.com"[..], true), None);
        assert_eq!(
            root.domain_lookup(&b"blah.test.example.com"[..], true),
            None
        );
        assert_eq!(
            root.domain_lookup(&b"www.example.com"[..], true),
            Some(&(b"www.example.com"[..].to_vec(), 1))
        );
        assert_eq!(
            root.domain_lookup(&b"alldomains.org"[..], true),
            Some(&(b"alldomains.org"[..].to_vec(), 4))
        );
        assert_eq!(
            root.domain_lookup(&b"test.hello.com"[..], true),
            Some(&(b"*.hello.com"[..].to_vec(), 7))
        );
        assert_eq!(
            root.domain_lookup(&b"images.cdn10.hello.com"[..], true),
            Some(&(b"images./cdn[0-9]+/.hello.com"[..].to_vec(), 8))
        );
        assert_eq!(
            root.domain_lookup(&b"test42.www.hello.com"[..], true),
            Some(&(b"/test[0-9]+/.www.hello.com"[..].to_vec(), 9))
        );
        assert_eq!(
            root.domain_lookup(&b"test.alldomains.org"[..], true),
            Some(&(b"*.alldomains.org"[..].to_vec(), 3))
        );
        assert_eq!(
            root.domain_lookup(&b"hello.alldomains.org"[..], true),
            Some(&(b"*.alldomains.org"[..].to_vec(), 3))
        );
        assert_eq!(
            root.domain_lookup(&b"pouet.alldomains.org"[..], true),
            Some(&(b"pouet.alldomains.org"[..].to_vec(), 5))
        );
        assert_eq!(
            root.domain_lookup(&b"blah.test.alldomains.org"[..], true),
            None
        );

        assert_eq!(
            root.domain_remove(&Vec::from(&b"alldomains.org"[..])),
            RemoveResult::Ok
        );
        println!("after remove");
        root.print();
        assert_eq!(root.domain_lookup(&b"alldomains.org"[..], true), None);
        assert_eq!(
            root.domain_lookup(&b"test.alldomains.org"[..], true),
            Some(&(b"*.alldomains.org"[..].to_vec(), 3))
        );
        assert_eq!(
            root.domain_lookup(&b"hello.alldomains.org"[..], true),
            Some(&(b"*.alldomains.org"[..].to_vec(), 3))
        );
        assert_eq!(
            root.domain_lookup(&b"pouet.alldomains.org"[..], true),
            Some(&(b"pouet.alldomains.org"[..].to_vec(), 5))
        );
        assert_eq!(
            root.domain_lookup(&b"test.hello.com"[..], true),
            Some(&(b"*.hello.com"[..].to_vec(), 7))
        );
        assert_eq!(
            root.domain_lookup(&b"blah.test.alldomains.org"[..], true),
            None
        );
    }

    #[test]
    fn wildcard() {
        let mut root: TrieNode<u8> = TrieNode::root();
        root.print();
        root.domain_insert("*.clever-cloud.com".as_bytes().to_vec(), 2u8);
        root.domain_insert("services.clever-cloud.com".as_bytes().to_vec(), 0u8);
        root.domain_insert("*.services.clever-cloud.com".as_bytes().to_vec(), 1u8);

        let res = root.domain_lookup(b"test.services.clever-cloud.com", true);
        println!("query result: {res:?}");

        assert_eq!(
            root.domain_lookup(b"pgstudio.services.clever-cloud.com", true),
            Some(&("*.services.clever-cloud.com".as_bytes().to_vec(), 1u8))
        );
    }

    /// A key `hm_insert` must skip on both the insert and the lookup pass:
    /// the wildcard slot, keyed by literal identity rather than by the
    /// child map. `TrieNode::insert_recursive`'s dotless arm stores a value
    /// in `self.wildcard` whenever the LEFTMOST label of the key is exactly
    /// `"*"` — not only when the whole key is `"*"` — because the
    /// recursive dot-split always keeps the original first byte at index 0
    /// of every prefix it recurses on, so only the leftmost label can ever
    /// reach that arm. `hm_insert`'s lookups pass `accept_wildcard: false`
    /// (it is testing plain literal lookup, not `*` resolution), which
    /// structurally can never see `self.wildcard` — see
    /// `TrieNode::lookup_recursive`'s step 3. A key like `"*."` or
    /// `"*.example.com"` therefore inserts `Ok` into the wildcard slot and
    /// then reports "did not find key" on lookup: not a router defect, a
    /// gap in this oracle's own exclusion list. This was `qc_insert`'s
    /// shrunk failure (`{"*.": 0}`) before this filter closed it; see the
    /// commit message for the investigation.
    fn hm_insert_skips(k: &str) -> bool {
        k.is_empty()
            || k.as_bytes()[0] == b'.'
            || k.contains('/')
            || k.split('.').next() == Some("*")
    }

    fn hm_insert(h: std::collections::HashMap<String, u32>) -> bool {
        let mut root: TrieNode<u32> = TrieNode::root();

        for (k, v) in h.iter() {
            if hm_insert_skips(k) {
                continue;
            }

            //println!("inserting key: '{}', value: '{}'", k, v);
            //assert_eq!(root.domain_insert(Vec::from(k.as_bytes()), *v), InsertResult::Ok);
            assert_eq!(
                root.insert(Vec::from(k.as_bytes()), *v),
                InsertResult::Ok,
                "could not insert ({k}, {v})"
            );
            //root.print();
        }

        //root.print();
        for (k, v) in h.iter() {
            if hm_insert_skips(k) {
                continue;
            }

            //match root.domain_lookup(k.as_bytes()) {
            match root.lookup(k.as_bytes(), false) {
                None => {
                    println!("did not find key '{k}'");
                    return false;
                }
                Some(&(ref k1, v1)) => {
                    if k.as_bytes() != &k1[..] || *v != v1 {
                        println!(
                            "request ({}, {}), got ({}, {})",
                            k,
                            v,
                            str::from_utf8(&k1[..]).unwrap(),
                            v1
                        );
                        return false;
                    }
                }
            }
        }

        true
    }

    quickcheck! {
      fn qc_insert(h: std::collections::HashMap<String, u32>) -> bool {
        hm_insert(h)
      }
    }

    #[test]
    fn insert_disappearing_tree() {
        let h: std::collections::HashMap<String, u32> = [
            (String::from("\n\u{3}"), 0),
            (String::from("\n\u{0}"), 1),
            (String::from("\n"), 2),
        ]
        .iter()
        .cloned()
        .collect();
        assert!(hm_insert(h));
    }

    #[test]
    fn size() {
        assert_size!(TrieNode<u32>, 136);
    }

    /// Regression: a hostname whose LEFTMOST segment is a regex
    /// (`/test[0-9]/.example.com`) used to underflow `pos - 1` (to
    /// `usize::MAX`) and panic on the second insert (the dedup loop) and
    /// on any `lookup_mut`. Both paths now special-case `pos == 0` (the
    /// regex is the leftmost/only segment, so the value is that subtree's
    /// own `key_value` — asked for, never inferred from how the subtree
    /// was built). This asserts the panic is gone and the entry resolves
    /// correctly. Here the subtree IS value-bearing because this test
    /// inserts the leftmost host first;
    /// `inserting_a_leftmost_regex_host_after_a_deeper_sibling_succeeds`
    /// covers the order where it is not.
    #[test]
    fn leftmost_regex_segment_reinsert_and_lookup_mut_do_not_panic() {
        let mut root: TrieNode<u8> = TrieNode::root();

        assert_eq!(
            root.insert(Vec::from(&b"/test[0-9]/.example.com"[..]), 7),
            InsertResult::Ok
        );
        // Second insert of the SAME leftmost-regex host: dedup loop with
        // pos == 0. Previously panicked; must now report Existing.
        assert_eq!(
            root.insert(Vec::from(&b"/test[0-9]/.example.com"[..]), 8),
            InsertResult::Existing
        );

        // lookup_mut on the existing leftmost-regex host: previously
        // panicked at `partial_key[..pos - 1]`; must now resolve the leaf
        // (value unchanged from the first insert — Existing did not
        // overwrite).
        //
        // The probe is the HOST KEY, not a host the segment matches: the
        // underflow lives in the trailing-`/` arm, which only a key ending
        // in a regex segment reaches. Probing `test4.example.com` instead
        // (as this test did before sozu#1351 made `lookup_mut`
        // key-addressed) never ran the arm under test at all — it left
        // through the `regexps` scan one level down.
        let resolved = root.domain_lookup_mut(b"/test[0-9]/.example.com", false);
        assert_eq!(
            resolved.map(|(_, v)| *v),
            Some(7),
            "leftmost-regex host must resolve via lookup_mut without panicking",
        );

        // The immutable lookup path (never buggy) agrees.
        assert_eq!(
            root.domain_lookup(b"test4.example.com", false),
            Some(&(b"/test[0-9]/.example.com"[..].to_vec(), 7))
        );

        // Removing the last rule clears the host.
        assert_eq!(
            root.domain_remove(&Vec::from(&b"/test[0-9]/.example.com"[..])),
            RemoveResult::Ok
        );
        assert_eq!(root.domain_lookup(b"test4.example.com", false), None);
    }
    /// A hostname whose leftmost segment is a regex and a DEEPER hostname
    /// sharing that segment live in the same `(regex, subtree)` entry: the
    /// first as the subtree's own `key_value`, the second as one of its
    /// children. Whichever arrives first OPENS the entry, so the second
    /// insert always lands on `insert_recursive`'s dedup loop — and when
    /// the deeper one opened it, the subtree is a valueless
    /// `TrieNode::root()` with a free `key_value` slot.
    ///
    /// That arm used to answer `InsertResult::Existing` unconditionally,
    /// on the (false) premise that a matched entry was necessarily built
    /// value-bearing by `TrieNode::new`. The leftmost host was then stored
    /// nowhere while being reported as already present — permanently
    /// un-insertable. The reverse order, which
    /// `removing_a_leftmost_regex_host_keeps_its_sibling_domains` uses,
    /// always worked, which is why the gap stayed invisible.
    ///
    /// To SEE THIS RED: in the `pos == 0` arm of the dedup loop in
    /// `TrieNode::insert_recursive`, replace
    /// `return t.1.insert_own_value(key, value);` with
    /// `return InsertResult::Existing;`. The second `assert_eq!` below then
    /// reports `left: Existing, right: Ok`, and the lookup after it returns
    /// `None`. Both fail in debug AND in release: nothing here depends on a
    /// `debug_assert`.
    #[test]
    fn inserting_a_leftmost_regex_host_after_a_deeper_sibling_succeeds() {
        let mut root: TrieNode<u8> = TrieNode::root();

        // The DEEPER domain opens the `(regex, subtree)` entry, leaving the
        // subtree's own `key_value` slot free.
        assert_eq!(
            root.domain_insert(Vec::from(&b"foo./test[0-9]/.example.com"[..]), 2),
            InsertResult::Ok
        );
        // The leftmost-regex host now claims that free slot.
        assert_eq!(
            root.domain_insert(Vec::from(&b"/test[0-9]/.example.com"[..]), 1),
            InsertResult::Ok,
            "a leftmost-regex host must be insertable after a deeper sibling opened its segment",
        );

        // BOTH must resolve. The store, not the return value, is what the
        // data plane serves.
        assert_eq!(
            root.domain_lookup(b"test4.example.com", false),
            Some(&(b"/test[0-9]/.example.com"[..].to_vec(), 1)),
            "the leftmost-regex host must resolve to the rule just inserted",
        );
        assert_eq!(
            root.domain_lookup(b"foo.test4.example.com", false),
            Some(&(b"foo./test[0-9]/.example.com"[..].to_vec(), 2)),
            "the deeper sibling must survive the second insert",
        );

        // A genuine duplicate is still `Existing`, and must not overwrite.
        assert_eq!(
            root.domain_insert(Vec::from(&b"/test[0-9]/.example.com"[..]), 9),
            InsertResult::Existing
        );
        assert_eq!(
            root.domain_lookup(b"test4.example.com", false),
            Some(&(b"/test[0-9]/.example.com"[..].to_vec(), 1)),
            "an Existing insert must not overwrite the stored value",
        );
    }

    /// The insertion order that ALREADY worked before the `pos == 0` fix —
    /// leftmost-regex host first, deeper sibling second — must not regress.
    /// Here the entry is opened by `TrieNode::new`, so the subtree's
    /// `key_value` is occupied when the deeper insert walks past it.
    ///
    /// To SEE THIS RED: make `TrieNode::insert_own_value` unconditional —
    /// `self.key_value = Some((key.to_vec(), value)); InsertResult::Ok`. In a
    /// debug build the production guard in `insert()` fires first, on
    /// "a fresh insert must add exactly one value to the trie, left: 2,
    /// right: 3" — the overwriting duplicate reported `Ok` without growing
    /// the value count. Strip that `debug_assert_eq!` too and the failure
    /// lands on the duplicate assertion below instead,
    /// `left: Ok, right: Existing`, which is the form a release build sees.
    #[test]
    fn inserting_a_deeper_sibling_after_a_leftmost_regex_host_still_works() {
        let mut root: TrieNode<u8> = TrieNode::root();

        assert_eq!(
            root.domain_insert(Vec::from(&b"/test[0-9]/.example.com"[..]), 1),
            InsertResult::Ok
        );
        assert_eq!(
            root.domain_insert(Vec::from(&b"foo./test[0-9]/.example.com"[..]), 2),
            InsertResult::Ok
        );

        assert_eq!(
            root.domain_lookup(b"test4.example.com", false),
            Some(&(b"/test[0-9]/.example.com"[..].to_vec(), 1))
        );
        assert_eq!(
            root.domain_lookup(b"foo.test4.example.com", false),
            Some(&(b"foo./test[0-9]/.example.com"[..].to_vec(), 2))
        );

        // Re-inserting either one is `Existing`, not a second value.
        assert_eq!(
            root.domain_insert(Vec::from(&b"/test[0-9]/.example.com"[..]), 8),
            InsertResult::Existing
        );
        assert_eq!(
            root.domain_insert(Vec::from(&b"foo./test[0-9]/.example.com"[..]), 8),
            InsertResult::Existing
        );
    }

    /// Removal must still behave once BOTH hosts are reachable, whichever
    /// order opened the segment and whichever order empties it. This guards
    /// the `remove_recursive` fix (which only ever saw the leftmost-first
    /// order) against the new insert path, and pins the `regexps` prune: the
    /// entry must disappear only when its last value is gone.
    ///
    /// To SEE THIS RED (insert side): in the `pos == 0` arm of
    /// `TrieNode::insert_recursive` replace
    /// `return t.1.insert_own_value(key, value);` with
    /// `return InsertResult::Existing;`. The test never reaches a
    /// `domain_remove` at all — its own deeper-first setup fails first, on
    /// the leftmost `assert_eq!(root.domain_insert(..), InsertResult::Ok)`,
    /// with `left: Existing, right: Ok`. No production guard fires on the
    /// way there: `insert()` returns `Existing`, whose postcondition asserts
    /// `after == before`, and that holds precisely because nothing was
    /// stored. That silence is the bug — which is why this test pins the
    /// insert, and not only the removals it is named for.
    ///
    /// To SEE THIS RED (remove side): restore the pre-fix `pos == 0` arm of
    /// `remove_recursive`, ahead of the `rest` binding:
    /// `let len = self.regexps.len();
    ///  self.regexps.retain(|(r, _)| r.as_str() != anchored_s);
    ///  if len > self.regexps.len() { return RemoveResult::Ok; }`
    /// The production guard in `remove()` fires first, on "a successful
    /// remove must drop exactly one value from the trie, left: 1, right: 2"
    /// — dropping the whole `regexps` entry took the sibling with it. Strip
    /// that `debug_assert_eq!` too and the failure lands on this test's own
    /// "the other host under the same regex segment must survive".
    #[test]
    fn removing_regex_segment_hosts_behaves_in_either_insertion_order() {
        // Insertion order A: deeper first (the order the insert fix opened).
        for remove_leftmost_first in [true, false] {
            let mut root: TrieNode<u8> = TrieNode::root();
            assert_eq!(
                root.domain_insert(Vec::from(&b"foo./test[0-9]/.example.com"[..]), 2),
                InsertResult::Ok
            );
            assert_eq!(
                root.domain_insert(Vec::from(&b"/test[0-9]/.example.com"[..]), 1),
                InsertResult::Ok
            );
            assert_regex_segment_pair_removes_cleanly(&mut root, remove_leftmost_first);
        }

        // Insertion order B: leftmost first (the pre-existing order).
        for remove_leftmost_first in [true, false] {
            let mut root: TrieNode<u8> = TrieNode::root();
            assert_eq!(
                root.domain_insert(Vec::from(&b"/test[0-9]/.example.com"[..]), 1),
                InsertResult::Ok
            );
            assert_eq!(
                root.domain_insert(Vec::from(&b"foo./test[0-9]/.example.com"[..]), 2),
                InsertResult::Ok
            );
            assert_regex_segment_pair_removes_cleanly(&mut root, remove_leftmost_first);
        }
    }

    /// Remove both hosts sharing a regex segment, one order or the other:
    /// each removal takes exactly its own host, the survivor still resolves,
    /// and the emptied trie is pruned back to a root with no `regexps` entry.
    fn assert_regex_segment_pair_removes_cleanly(
        root: &mut TrieNode<u8>,
        remove_leftmost_first: bool,
    ) {
        let leftmost = Vec::from(&b"/test[0-9]/.example.com"[..]);
        let deeper = Vec::from(&b"foo./test[0-9]/.example.com"[..]);

        let (first, second) = if remove_leftmost_first {
            (&leftmost, &deeper)
        } else {
            (&deeper, &leftmost)
        };
        let (first_host, second_host): (&[u8], &[u8]) = if remove_leftmost_first {
            (b"test4.example.com", b"foo.test4.example.com")
        } else {
            (b"foo.test4.example.com", b"test4.example.com")
        };

        assert_eq!(root.domain_remove(first), RemoveResult::Ok);
        assert_eq!(
            root.domain_lookup(first_host, false),
            None,
            "the removed host must be gone",
        );
        assert!(
            root.domain_lookup(second_host, false).is_some(),
            "the other host under the same regex segment must survive",
        );
        // A second removal of an absent host is NotFound, never a silent Ok
        // that takes the survivor with it.
        assert_eq!(root.domain_remove(first), RemoveResult::NotFound);
        assert!(root.domain_lookup(second_host, false).is_some());

        assert_eq!(root.domain_remove(second), RemoveResult::Ok);
        assert_eq!(root.domain_lookup(second_host, false), None);
        assert!(
            root.is_empty(),
            "the emptied regex subtree must be pruned back to an empty root",
        );
    }

    /// Removing a leftmost-regex host must drop that one host and nothing
    /// else. The `pos == 0` arm of `remove_recursive` used to `retain` the
    /// whole `(regex, subtree)` entry out of `regexps`, which also deleted
    /// every deeper domain registered under the same segment — and it
    /// reported `Ok` even when the leftmost host itself had never been
    /// stored. The value lives in that subtree's own `key_value`, so the
    /// removal must recurse into it with an empty key, exactly like the
    /// `pos > 0` arm recurses on the remaining prefix.
    ///
    /// To SEE THIS RED: restore the pre-fix `pos == 0` arm of
    /// `TrieNode::remove_recursive`:
    /// `let len = self.regexps.len();
    ///  self.regexps.retain(|(r, _)| r.as_str() != anchored_s);
    ///  if len > self.regexps.len() { return RemoveResult::Ok; }`
    /// — the sibling lookup below then returns `None`, and in a debug build
    /// `remove()` panics first on "a successful remove must drop exactly one
    /// value from the trie".
    #[test]
    fn removing_a_leftmost_regex_host_keeps_its_sibling_domains() {
        let mut root: TrieNode<u8> = TrieNode::root();

        assert_eq!(
            root.domain_insert(Vec::from(&b"/test[0-9]/.example.com"[..]), 1),
            InsertResult::Ok
        );
        assert_eq!(
            root.domain_insert(Vec::from(&b"foo./test[0-9]/.example.com"[..]), 2),
            InsertResult::Ok
        );
        assert_eq!(
            root.domain_lookup(b"test4.example.com", false),
            Some(&(b"/test[0-9]/.example.com"[..].to_vec(), 1))
        );
        assert_eq!(
            root.domain_lookup(b"foo.test4.example.com", false),
            Some(&(b"foo./test[0-9]/.example.com"[..].to_vec(), 2))
        );

        assert_eq!(
            root.domain_remove(&Vec::from(&b"/test[0-9]/.example.com"[..])),
            RemoveResult::Ok
        );
        assert_eq!(
            root.domain_lookup(b"test4.example.com", false),
            None,
            "the removed leftmost-regex host must be gone",
        );
        assert_eq!(
            root.domain_lookup(b"foo.test4.example.com", false),
            Some(&(b"foo./test[0-9]/.example.com"[..].to_vec(), 2)),
            "a sibling domain under the same regex segment must survive",
        );

        // A leftmost-regex host that is not stored is `NotFound`, not a
        // silent `Ok` that takes the surviving sibling with it.
        assert_eq!(
            root.domain_remove(&Vec::from(&b"/test[0-9]/.example.com"[..])),
            RemoveResult::NotFound
        );
        assert_eq!(
            root.domain_lookup(b"foo.test4.example.com", false),
            Some(&(b"foo./test[0-9]/.example.com"[..].to_vec(), 2))
        );
    }

    /// An emptied regex subtree must be pruned from `regexps`, exactly like
    /// an emptied `children` subtree is pruned from `children`. The `pos > 0`
    /// arm of `remove_recursive` recursed into the subtree and returned `Ok`
    /// without ever dropping the now valueless `(regex, subtree)` entry,
    /// stranding a node that keeps its whole parent chain reachable and
    /// non-empty. `insert_remove_through_regex` never empties the subtree,
    /// which is why the leak went unnoticed.
    ///
    /// To SEE THIS RED: delete the
    /// `if … .is_empty() { self.regexps.remove(index); }` prune from the
    /// regex arm of `TrieNode::remove_recursive` — in a debug build the trie
    /// panics first on "a retained child subtree must still hold at least one
    /// value", and in release `root.is_empty()` stays false.
    #[test]
    fn removing_the_last_host_under_a_regex_segment_prunes_the_subtree() {
        let mut root: TrieNode<u8> = TrieNode::root();

        assert_eq!(
            root.domain_insert(Vec::from(&b"www./.*/.com"[..]), 1),
            InsertResult::Ok
        );
        assert_eq!(
            root.domain_lookup(b"www.sozu.com", false),
            Some(&(b"www./.*/.com"[..].to_vec(), 1))
        );

        assert_eq!(
            root.domain_remove(&Vec::from(&b"www./.*/.com"[..])),
            RemoveResult::Ok
        );
        assert_eq!(root.domain_lookup(b"www.sozu.com", false), None);
        assert!(
            root.is_empty(),
            "dropping the last host under a regex segment must leave an empty trie",
        );
    }
    /// sozu#1351: `lookup_mut` addresses a KEY, `lookup` addresses a
    /// REQUEST. The two had drifted: `lookup_mut` resolved a literal
    /// segment through the first regex segment matching it, so
    /// `add_tree_rule` — which reaches a leaf through `domain_lookup_mut`
    /// before deciding whether to insert — attached an exact host's rule
    /// to the regex segment's leaf, and the whole regex family served it.
    ///
    /// `accept_wildcard: false` now means "this literal key may not
    /// resolve into a non-literal entry", covering the regex segments the
    /// way it already covered the wildcard slot (`tcp.rs`'s
    /// `insert_sni_route` documents the same reasoning for `*`).
    ///
    /// To SEE THIS RED: in `TrieNode::lookup_mut`, drop the
    /// `accept_wildcard &&` guard in front of the `self.regexps` scan.
    /// The first assertion then resolves to `Some(7)` — the regex leaf.
    #[test]
    fn lookup_mut_does_not_resolve_a_literal_host_through_a_regex_segment() {
        let mut root: TrieNode<u8> = TrieNode::root();
        assert_eq!(
            root.domain_insert(Vec::from(&b"/test[0-9]/.example.com"[..]), 7),
            InsertResult::Ok
        );

        assert_eq!(
            root.domain_lookup_mut(b"test4.example.com", false)
                .map(|(_, v)| *v),
            None,
            "a literal host is not a key of this trie just because a regex \
             segment matches it",
        );
        assert_eq!(
            root.domain_lookup_mut(b"/test[0-9]/.example.com", false)
                .map(|(_, v)| *v),
            Some(7),
            "the regex segment's own key still resolves, by regex identity",
        );
        assert_eq!(
            root.domain_lookup(b"test4.example.com", false),
            Some(&(b"/test[0-9]/.example.com"[..].to_vec(), 7)),
            "while the request-addressed lookup still matches the family",
        );

        // Which is the point: the exact host can now get its OWN node.
        assert_eq!(
            root.domain_insert(Vec::from(&b"test4.example.com"[..]), 4),
            InsertResult::Ok
        );
        assert_eq!(
            root.domain_lookup(b"test4.example.com", false),
            Some(&(b"test4.example.com"[..].to_vec(), 4)),
            "and it outranks the regex segment that also matches it",
        );
    }

    /// The request-addressed `lookup` walks its candidates
    /// most-specific-first — exact child, then regex segments in
    /// declaration order, then the wildcard — and a candidate that holds
    /// no value for the host hands it to the next one.
    ///
    /// This is the walk `lib/src/tls.rs` and
    /// `lib/src/protocol/tcp_preread/` resolve SNI with, so certificate
    /// selection follows the same order as HTTP routing.
    ///
    /// To SEE THIS RED: in `TrieNode::lookup_recursive`, move the wildcard
    /// block above the `self.regexps` loop — `test7.example.com` then
    /// answers with the wildcard entry.
    #[test]
    fn lookup_prefers_exact_then_regex_then_wildcard() {
        let mut root: TrieNode<u8> = TrieNode::root();
        assert_eq!(
            root.domain_insert(Vec::from(&b"*.example.com"[..]), 1),
            InsertResult::Ok
        );
        assert_eq!(
            root.domain_insert(Vec::from(&b"/test[0-9]/.example.com"[..]), 2),
            InsertResult::Ok
        );
        assert_eq!(
            root.domain_insert(Vec::from(&b"test4.example.com"[..]), 3),
            InsertResult::Ok
        );

        assert_eq!(
            root.domain_lookup(b"test4.example.com", true)
                .map(|(_, v)| *v),
            Some(3),
            "the exact child wins over both",
        );
        assert_eq!(
            root.domain_lookup(b"test7.example.com", true)
                .map(|(_, v)| *v),
            Some(2),
            "a regex segment wins over the wildcard",
        );
        assert_eq!(
            root.domain_lookup(b"other.example.com", true)
                .map(|(_, v)| *v),
            Some(1),
            "the wildcard answers what neither claims",
        );
        assert_eq!(
            root.domain_lookup(b"test7.example.com", false)
                .map(|(_, v)| *v),
            Some(2),
            "and the regex segment still answers when the wildcard is \
             refused, which is what makes the order above observable",
        );
    }

    /// A regex segment that matches the label but whose subtree holds
    /// nothing for this host must not swallow the lookup: the walk
    /// rewinds and tries the wildcard.
    ///
    /// `images./cdn[0-9]+/.hello.com` opens the `cdn[0-9]+` entry with a
    /// VALUELESS subtree (`TrieNode::root`) holding only `images`, so
    /// `cdn10.hello.com` matches the segment and finds no value behind it.
    ///
    /// To SEE THIS RED: in `TrieNode::lookup_recursive`, `return` the
    /// result of the recursive call inside the `self.regexps` loop instead
    /// of continuing on `None`. `cdn10.hello.com` then answers `None`.
    #[test]
    fn a_regex_segment_holding_no_value_falls_back_to_the_wildcard() {
        let mut root: TrieNode<u8> = TrieNode::root();
        assert_eq!(
            root.domain_insert(Vec::from(&b"images./cdn[0-9]+/.hello.com"[..]), 8),
            InsertResult::Ok
        );
        assert_eq!(
            root.domain_insert(Vec::from(&b"*.hello.com"[..]), 7),
            InsertResult::Ok
        );

        assert_eq!(
            root.domain_lookup(b"images.cdn10.hello.com", true)
                .map(|(_, v)| *v),
            Some(8),
            "the regex segment serves the host it was opened for",
        );
        assert_eq!(
            root.domain_lookup(b"cdn10.hello.com", true)
                .map(|(_, v)| *v),
            Some(7),
            "and the wildcard serves the one it was not",
        );
    }

    /// [`InlineTrieMatches`] is a drop-in for the `Vec` record: the same
    /// pushes and rewinds, including across the boundary between its inline
    /// slots and its spill, leave the same segments in the same order.
    #[test]
    fn inline_trie_matches_rewinds_across_the_spill_like_a_vec() {
        let labels: Vec<Vec<u8>> = (0..INLINE_TRIE_MATCHES + 8)
            .map(|index| format!("s{index}").into_bytes())
            .collect();
        let mut inline = InlineTrieMatches::new();
        let mut heap: TrieMatches<'_, '_> = Vec::new();
        let segments = |sub_matches: Vec<&TrieSubMatch<'_, '_>>| -> Vec<Vec<u8>> {
            sub_matches
                .into_iter()
                .map(|sub_match| match sub_match {
                    TrieSubMatch::Wildcard(segment) | TrieSubMatch::Regexp(segment, _) => {
                        segment.to_vec()
                    }
                })
                .collect()
        };

        // Fill past the inline slots, rewind into them, refill, then rewind
        // inside the spill only.
        let steps: [(usize, usize); 3] = [
            (INLINE_TRIE_MATCHES + 4, INLINE_TRIE_MATCHES - 3),
            (INLINE_TRIE_MATCHES + 8, INLINE_TRIE_MATCHES + 2),
            (INLINE_TRIE_MATCHES + 8, 0),
        ];
        for (fill, rewind) in steps {
            for label in &labels[inline.mark()..fill] {
                SubMatchSink::push(&mut inline, TrieSubMatch::Wildcard(label));
                SubMatchSink::push(&mut heap, TrieSubMatch::Wildcard(label));
            }
            assert_eq!(
                segments(inline.iter().collect()),
                segments(heap.iter().collect()),
                "after filling to {fill}",
            );
            SubMatchSink::rewind(&mut inline, rewind);
            SubMatchSink::rewind(&mut heap, rewind);
            assert_eq!(inline.mark(), heap.mark());
            assert_eq!(
                segments(inline.iter().collect()),
                segments(heap.iter().collect()),
                "after rewinding to {rewind}",
            );
        }
    }
}
