//! Query parsing and parallel execution (SPEC §2.6).
//!
//! Syntax: terms separated by spaces are ANDed, `|` separates alternatives (binds tighter
//! than AND), `!` negates a term, `"…"` quotes spaces. Filters: `ext:pdf;docx`,
//! `type:file|folder`, `path:text` (matched against the full path, evaluated last).
//! Terms containing `*` or `?` are wildcards matched against the whole name.

use memchr::memmem::Finder;
use rayon::prelude::*;

use crate::fold::{FoldOptions, fold, fold_into};
use crate::index::Index;
use crate::time;

/// Why a query could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    #[error("unknown filter `{0}:`")]
    UnknownFilter(String),
    /// Recognised filter that needs data the index does not have yet (sizes, dates).
    #[error("the `{0}:` filter is not supported yet")]
    NotSupportedYet(String),
    #[error("invalid regular expression: {0}")]
    InvalidRegex(String),
    #[error("`{0}:` needs a value")]
    MissingValue(String),
    #[error("invalid value `{value}` for `{filter}:`")]
    InvalidValue { filter: String, value: String },
    #[error("`|` or `!` without a term")]
    DanglingOperator,
}

/// A parsed, ready-to-run query. Cheap to share between threads.
#[derive(Debug, Clone)]
pub struct Query {
    root: Node,
    /// First plain positive term, used for ranking.
    primary: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
enum Node {
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Leaf(Term),
}

#[derive(Debug, Clone)]
enum Term {
    // Boxed: a Finder is large and would bloat every node.
    Substring(Box<Finder<'static>>),
    /// Whole-name pattern plus its longest literal run, used as a fast prefilter.
    Wildcard {
        pattern: Vec<u8>,
        literal: Option<Box<Finder<'static>>>,
    },
    /// Folded extensions without the dot.
    Ext(Vec<Vec<u8>>),
    Type {
        dir: bool,
    },
    Path(Box<Finder<'static>>),
    /// Matched against the folded name; compiled once and shared by all workers.
    Regex(Box<regex::bytes::Regex>),
    /// File size in bytes, inclusive range (folders never match).
    Size {
        min: u64,
        max: u64,
    },
    /// Last write time (FILETIME), inclusive range; entries without a time never match.
    Modified {
        min: u64,
        max: u64,
    },
}

impl Term {
    /// Path terms rebuild the full path, so they run after everything else.
    fn is_expensive(&self) -> bool {
        matches!(self, Self::Path(_))
    }
}

impl Node {
    fn is_expensive(&self) -> bool {
        match self {
            Self::Leaf(term) => term.is_expensive(),
            Self::Not(inner) => inner.is_expensive(),
            Self::And(nodes) | Self::Or(nodes) => nodes.iter().any(Self::is_expensive),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Word {
        text: String,
        /// Byte offset of the first `:` outside quotes.
        colon: Option<usize>,
        quoted: bool,
    },
    Or,
}

fn is_regex_word(text: &str) -> bool {
    let text = text.trim_start_matches('!').as_bytes();
    let starts = |prefix: &[u8]| {
        text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix)
    };
    starts(b"re:") || starts(b"regex:")
}

fn tokenize(input: &str) -> Vec<Token> {
    #[derive(Default)]
    struct Word {
        text: String,
        colon: Option<usize>,
        quoted: bool,
    }

    fn flush(tokens: &mut Vec<Token>, word: &mut Word) {
        let word = std::mem::take(word);
        if !word.text.is_empty() || word.quoted {
            tokens.push(Token::Word {
                text: word.text,
                colon: word.colon,
                quoted: word.quoted,
            });
        }
    }

    let mut tokens = Vec::new();
    let mut word = Word::default();
    let mut in_quotes = false;
    for c in input.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                word.quoted = true;
            }
            c if in_quotes => word.text.push(c),
            c if c.is_whitespace() => flush(&mut tokens, &mut word),
            // Inside `re:` the `|` belongs to the regex (alternation), not the query.
            '|' if is_regex_word(&word.text) => word.text.push('|'),
            '|' => {
                flush(&mut tokens, &mut word);
                tokens.push(Token::Or);
            }
            ':' if word.colon.is_none() => {
                word.colon = Some(word.text.len());
                word.text.push(':');
            }
            c => word.text.push(c),
        }
    }
    flush(&mut tokens, &mut word);
    tokens
}

impl Query {
    /// Parses `input`; names must be folded with the same `options` as the index.
    pub fn parse(input: &str, options: FoldOptions) -> Result<Self, QueryError> {
        let tokens = tokenize(input);
        let mut and = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            // One OR-group: unit ('|' unit)*
            let mut alternatives = Vec::new();
            loop {
                let Some(Token::Word {
                    text,
                    colon,
                    quoted,
                }) = tokens.get(i)
                else {
                    return Err(QueryError::DanglingOperator);
                };
                alternatives.push(parse_word(text, *colon, *quoted, options)?);
                i += 1;
                if tokens.get(i) == Some(&Token::Or) {
                    i += 1;
                } else {
                    break;
                }
            }
            and.push(if alternatives.len() == 1 {
                alternatives.remove(0)
            } else {
                Node::Or(alternatives)
            });
        }

        let primary = and.iter().find_map(|node| match node {
            Node::Leaf(Term::Substring(finder)) => Some(finder.needle().to_vec()),
            _ => None,
        });
        // Cheap terms first; path terms only run for entries that passed the rest.
        and.sort_by_key(Node::is_expensive);
        Ok(Self {
            root: Node::And(and),
            primary,
        })
    }

    /// True if the query has no terms (matches everything).
    pub fn is_empty(&self) -> bool {
        matches!(&self.root, Node::And(nodes) if nodes.is_empty())
    }

    fn matches(&self, index: &Index, slot: u32, scratch: &mut Scratch) -> bool {
        scratch.path_ready = false;
        eval(&self.root, index, slot, scratch)
    }

    /// Ranking key; smaller is better. Exact > prefix > word boundary > substring,
    /// then shorter names, then slot order.
    fn rank(&self, index: &Index, slot: u32) -> u64 {
        let name = index.folded_name(slot);
        let class: u64 = match &self.primary {
            None => 4,
            Some(term) if name == term.as_slice() => 0,
            Some(term) => match memchr::memmem::find(name, term) {
                Some(0) => 1,
                Some(pos) if is_boundary(name[pos - 1]) => 2,
                _ => 3,
            },
        };
        (class << 48) | ((name.len().min(0xFFFF) as u64) << 32) | u64::from(slot)
    }
}

/// Folds the literal characters of a regex the same way names are folded, so the
/// pattern matches the folded pool without `(?i)`. Escape sequences (`\D`, `\p{Lu}`,
/// `\x{130}`) and group flags (`(?U)`, `(?P<name>`) are copied verbatim, because folding
/// them would change their meaning.
fn fold_regex_literals(pattern: &str, options: FoldOptions) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut folded = Vec::with_capacity(8);
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                out.push(c);
                let Some(next) = chars.next() else { break };
                out.push(next);
                if matches!(next, 'p' | 'P' | 'x' | 'u' | 'U') && chars.peek() == Some(&'{') {
                    for c in chars.by_ref() {
                        out.push(c);
                        if c == '}' {
                            break;
                        }
                    }
                } else if matches!(next, 'p' | 'P')
                    && let Some(class) = chars.next()
                {
                    out.push(class); // one-letter class such as \pL
                }
            }
            '(' if chars.peek() == Some(&'?') => {
                out.push(c);
                for c in chars.by_ref() {
                    out.push(c);
                    if matches!(c, ':' | ')' | '>') {
                        break;
                    }
                }
            }
            c => {
                folded.clear();
                fold_into(c.encode_utf8(&mut [0; 4]), options, &mut folded);
                out.push_str(std::str::from_utf8(&folded).unwrap_or_default());
            }
        }
    }
    out
}

/// `1.5mb`, `10 kb`, `300` (bytes); 1024-based units.
fn parse_bytes(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(text.len());
    let number: f64 = text[..split].trim().parse().ok()?;
    let unit: u64 = match text[split..].to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        "t" | "tb" => 1 << 40,
        _ => return None,
    };
    if !number.is_finite() || number < 0.0 {
        return None;
    }
    // Bounded by the checks above; rounding to whole bytes is intended.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let bytes = (number * unit as f64).round() as u64;
    Some(bytes)
}

/// `size:` value -> inclusive byte range: `>1mb`, `>=1mb`, `<10kb`, `<=10kb`, `=5`, `5`,
/// `1mb..10mb`, `..1kb`, `empty`.
fn parse_size_range(value: &str) -> Option<(u64, u64)> {
    if value.eq_ignore_ascii_case("empty") {
        return Some((0, 0));
    }
    if let Some((low, high)) = value.split_once("..") {
        let low = if low.is_empty() { 0 } else { parse_bytes(low)? };
        let high = if high.is_empty() {
            u64::MAX
        } else {
            parse_bytes(high)?
        };
        return (low <= high).then_some((low, high));
    }
    if let Some(rest) = value.strip_prefix(">=") {
        return Some((parse_bytes(rest)?, u64::MAX));
    }
    if let Some(rest) = value.strip_prefix("<=") {
        return Some((0, parse_bytes(rest)?));
    }
    if let Some(rest) = value.strip_prefix('>') {
        return Some((parse_bytes(rest)?.saturating_add(1), u64::MAX));
    }
    if let Some(rest) = value.strip_prefix('<') {
        return parse_bytes(rest)?.checked_sub(1).map(|high| (0, high));
    }
    let exact = parse_bytes(value.strip_prefix('=').unwrap_or(value))?;
    Some((exact, exact))
}

/// Start and end (inclusive) FILETIME of a local calendar day.
fn day_range((y, m, d): (u16, u16, u16)) -> Option<(u64, u64)> {
    Some((
        time::local_midnight(y, m, d)?,
        time::next_local_midnight(y, m, d)? - 1,
    ))
}

/// `dm:` value -> inclusive FILETIME range: `today`, `yesterday`, `week`, `month`, `year`
/// (the last 7/30/365 days), `2025-01-31`, `2025-01-01..2025-02-01`, `..2025-02-01`,
/// `>2025-01-01`, `>=…`, `<…`, `<=…`.
fn parse_date_range(value: &str) -> Option<(u64, u64)> {
    let now = time::now();
    let last_days = |days: u64| Some((now.saturating_sub(days * time::DAY), u64::MAX));
    match value.to_ascii_lowercase().as_str() {
        "today" => return day_range(time::today()),
        "yesterday" => {
            let (y, m, d) = time::today();
            return day_range(time::previous_day(y, m, d));
        }
        "week" => return last_days(7),
        "month" => return last_days(30),
        "year" => return last_days(365),
        _ => {}
    }
    let day = |text: &str| time::parse_date(text).and_then(day_range);
    if let Some((low, high)) = value.split_once("..") {
        let low = if low.is_empty() { 0 } else { day(low)?.0 };
        let high = if high.is_empty() {
            u64::MAX
        } else {
            day(high)?.1
        };
        return (low <= high).then_some((low, high));
    }
    if let Some(rest) = value.strip_prefix(">=") {
        return Some((day(rest)?.0, u64::MAX));
    }
    if let Some(rest) = value.strip_prefix("<=") {
        return Some((0, day(rest)?.1));
    }
    if let Some(rest) = value.strip_prefix('>') {
        return Some((day(rest)?.1 + 1, u64::MAX));
    }
    if let Some(rest) = value.strip_prefix('<') {
        return Some((0, day(rest)?.0.checked_sub(1)?));
    }
    day(value)
}

fn is_boundary(byte: u8) -> bool {
    byte.is_ascii() && !byte.is_ascii_alphanumeric()
}

fn parse_word(
    text: &str,
    colon: Option<usize>,
    quoted: bool,
    options: FoldOptions,
) -> Result<Node, QueryError> {
    if let Some(rest) = text.strip_prefix('!') {
        if rest.is_empty() {
            return Err(QueryError::DanglingOperator);
        }
        let inner = parse_word(rest, colon.map(|c| c - 1), quoted, options)?;
        return Ok(Node::Not(Box::new(inner)));
    }

    if let Some(at) = colon {
        let filter = text[..at].to_ascii_lowercase();
        let value = &text[at + 1..];
        if value.is_empty() {
            return Err(QueryError::MissingValue(filter));
        }
        let invalid = || QueryError::InvalidValue {
            filter: filter.clone(),
            value: value.to_owned(),
        };
        let term = match filter.as_str() {
            "ext" => Term::Ext(
                value
                    .split([';', ','])
                    .map(|e| e.trim().trim_start_matches('.'))
                    .filter(|e| !e.is_empty())
                    .map(|e| fold(e, options))
                    .collect(),
            ),
            "type" => match value.to_ascii_lowercase().as_str() {
                "file" | "files" => Term::Type { dir: false },
                "folder" | "folders" | "dir" | "dirs" | "directory" => Term::Type { dir: true },
                _ => return Err(invalid()),
            },
            "path" => Term::Path(Box::new(Finder::new(&fold(value, options)).into_owned())),
            "re" | "regex" => {
                let pattern = fold_regex_literals(value, options);
                let regex = regex::bytes::Regex::new(&pattern)
                    .map_err(|e| QueryError::InvalidRegex(e.to_string()))?;
                Term::Regex(Box::new(regex))
            }
            "size" => {
                let (min, max) = parse_size_range(value).ok_or_else(invalid)?;
                Term::Size { min, max }
            }
            "dm" | "datemodified" => {
                let (min, max) = parse_date_range(value).ok_or_else(invalid)?;
                Term::Modified { min, max }
            }
            // Creation and access times are not kept in the index.
            "dc" | "da" => return Err(QueryError::NotSupportedYet(filter)),
            _ => return Err(QueryError::UnknownFilter(filter)),
        };
        return Ok(Node::Leaf(term));
    }

    let folded = fold(text, options);
    if !quoted && folded.iter().any(|&b| b == b'*' || b == b'?') {
        let literal = folded
            .split(|&b| b == b'*' || b == b'?')
            .max_by_key(|run| run.len())
            .filter(|run| run.len() >= 2)
            .map(|run| Box::new(Finder::new(run).into_owned()));
        return Ok(Node::Leaf(Term::Wildcard {
            pattern: folded,
            literal,
        }));
    }
    Ok(Node::Leaf(Term::Substring(Box::new(
        Finder::new(&folded).into_owned(),
    ))))
}

/// Per-thread buffers reused across entries so evaluation does not allocate.
#[derive(Default)]
struct Scratch {
    chain: Vec<u32>,
    path: Vec<u8>,
    /// `path` holds the folded path of the entry being evaluated.
    path_ready: bool,
}

fn eval(node: &Node, index: &Index, slot: u32, scratch: &mut Scratch) -> bool {
    match node {
        Node::And(nodes) => nodes.iter().all(|n| eval(n, index, slot, scratch)),
        Node::Or(nodes) => nodes.iter().any(|n| eval(n, index, slot, scratch)),
        Node::Not(inner) => !eval(inner, index, slot, scratch),
        Node::Leaf(term) => match term {
            Term::Substring(finder) => finder.find(index.folded_name(slot)).is_some(),
            Term::Wildcard { pattern, literal } => {
                let name = index.folded_name(slot);
                literal.as_ref().is_none_or(|f| f.find(name).is_some())
                    && wildcard_match(pattern, name)
            }
            Term::Type { dir } => index.is_dir(slot) == *dir,
            Term::Ext(exts) => {
                if index.is_dir(slot) {
                    return false;
                }
                let name = index.folded_name(slot);
                let Some(dot) = memchr::memrchr(b'.', name) else {
                    return false;
                };
                let ext = &name[dot + 1..];
                exts.iter().any(|e| e.as_slice() == ext)
            }
            Term::Regex(regex) => regex.is_match(index.folded_name(slot)),
            Term::Size { min, max } => {
                !index.is_dir(slot) && (*min..=*max).contains(&index.size(slot))
            }
            Term::Modified { min, max } => {
                let mtime = index.mtime(slot);
                mtime != 0 && (*min..=*max).contains(&mtime)
            }
            Term::Path(finder) => {
                if !scratch.path_ready {
                    index.folded_path_into(slot, &mut scratch.chain, &mut scratch.path);
                    scratch.path_ready = true;
                }
                finder.find(&scratch.path).is_some()
            }
        },
    }
}

/// `*` matches any run of bytes, `?` exactly one UTF-8 character; the whole name must match.
fn wildcard_match(pattern: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(b'?') => {
                p += 1;
                n += utf8_len(name[n]);
            }
            Some(&c) if c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((sp, sn)) => {
                    p = sp + 1;
                    let next = sn + utf8_len(name[sn]);
                    star = Some((sp, next));
                    n = next;
                }
                None => return false,
            },
        }
    }
    pattern[p.min(pattern.len())..].iter().all(|&c| c == b'*')
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

/// Result order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SortBy {
    /// Exact > prefix > word boundary > substring, then shorter names.
    #[default]
    Relevance,
    Name,
    Size,
    Modified,
}

/// Ordering and paging for [`search_volumes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchOptions {
    pub sort: SortBy,
    pub descending: bool,
    pub offset: usize,
    pub limit: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            sort: SortBy::Relevance,
            descending: false,
            offset: 0,
            limit: 100,
        }
    }
}

/// One match: which index (position in the slice given to [`search_volumes`]) and slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub volume: usize,
    pub slot: u32,
}

/// Result of [`search_volumes`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VolumesResult {
    /// Number of matching entries over all volumes.
    pub total: usize,
    /// The requested page, in order.
    pub hits: Vec<Hit>,
}

/// Result of [`search`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResult {
    /// Number of matching entries.
    pub total: usize,
    /// Best `limit` matches, best first.
    pub slots: Vec<u32>,
}

/// Slots per parallel work item.
const CHUNK: usize = 1 << 15;

/// Runs `query` over one index and returns the best `limit` matches by relevance.
pub fn search(index: &Index, query: &Query, limit: usize) -> SearchResult {
    let options = SearchOptions {
        limit,
        ..SearchOptions::default()
    };
    let result = search_volumes(&[index], query, &options);
    SearchResult {
        total: result.total,
        slots: result.hits.into_iter().map(|hit| hit.slot).collect(),
    }
}

/// Runs `query` over several volume indexes in parallel and returns one sorted page.
pub fn search_volumes(indexes: &[&Index], query: &Query, options: &SearchOptions) -> VolumesResult {
    let keep = options.offset.saturating_add(options.limit);
    match options.sort {
        SortBy::Relevance => collect(indexes, query, keep, options, |i, slot| {
            query.rank(indexes[i], slot)
        }),
        SortBy::Name => collect(indexes, query, keep, options, |i, slot| {
            indexes[i].folded_name(slot)
        }),
        SortBy::Size => collect(indexes, query, keep, options, |i, slot| {
            indexes[i].size(slot)
        }),
        SortBy::Modified => collect(indexes, query, keep, options, |i, slot| {
            indexes[i].mtime(slot)
        }),
    }
}

/// Matches as `(sort key, volume, slot)`.
type Found<K> = Vec<(K, usize, u32)>;

/// Matches every live slot, keeps the best `keep` per work item by `(key, volume, slot)`
/// and returns the requested page. Ties are broken by volume and slot, so pages are stable.
fn collect<K: Ord + Send>(
    indexes: &[&Index],
    query: &Query,
    keep: usize,
    options: &SearchOptions,
    key: impl Fn(usize, u32) -> K + Sync,
) -> VolumesResult {
    let descending = options.descending;
    let order = |a: &(K, usize, u32), b: &(K, usize, u32)| {
        if descending { b.cmp(a) } else { a.cmp(b) }
    };
    let work: Vec<(usize, usize)> = indexes
        .iter()
        .enumerate()
        .flat_map(|(volume, index)| {
            (0..index.slot_count().div_ceil(CHUNK)).map(move |chunk| (volume, chunk))
        })
        .collect();

    let partials: Vec<(usize, Found<K>)> = work
        .into_par_iter()
        .map(|(volume, chunk)| {
            let index = indexes[volume];
            let start = chunk * CHUNK;
            let end = (start + CHUNK).min(index.slot_count());
            let mut found = Vec::new();
            let mut scratch = Scratch::default();
            for slot in start..end {
                // Slot counts are bounded by u32 (see Index::alloc_slot).
                #[allow(clippy::cast_possible_truncation)]
                let slot = slot as u32;
                if index.is_live(slot) && query.matches(index, slot, &mut scratch) {
                    found.push((key(volume, slot), volume, slot));
                }
            }
            let total = found.len();
            keep_best(&mut found, keep, order);
            (total, found)
        })
        .collect();

    let total = partials.iter().map(|(t, _)| t).sum();
    let mut found: Found<K> = partials.into_iter().flat_map(|(_, f)| f).collect();
    keep_best(&mut found, keep, order);
    found.sort_unstable_by(order);
    VolumesResult {
        total,
        hits: found
            .into_iter()
            .skip(options.offset)
            .take(options.limit)
            .map(|(_, volume, slot)| Hit { volume, slot })
            .collect(),
    }
}

/// One flag per slot of `index`: true for live slots matching `query`. Runs in parallel;
/// used to restrict size totals to matching files (e.g. `ext:mp4`).
pub fn match_mask(index: &Index, query: &Query) -> Vec<bool> {
    let mut mask = vec![false; index.slot_count()];
    mask.par_chunks_mut(CHUNK)
        .enumerate()
        .for_each(|(chunk, flags)| {
            let mut scratch = Scratch::default();
            let start = chunk * CHUNK;
            for (i, flag) in flags.iter_mut().enumerate() {
                // Slot counts are bounded by u32 (see Index::alloc_slot).
                #[allow(clippy::cast_possible_truncation)]
                let slot = (start + i) as u32;
                *flag = index.is_live(slot) && query.matches(index, slot, &mut scratch);
            }
        });
    mask
}

fn keep_best<T>(items: &mut Vec<T>, keep: usize, order: impl Fn(&T, &T) -> std::cmp::Ordering) {
    if items.len() > keep {
        if keep > 0 {
            items.select_nth_unstable_by(keep - 1, &order);
        }
        items.truncate(keep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::{DIR, entry, sample};
    use crate::index::{Index, NewEntry};

    fn names(query: &str) -> Vec<String> {
        let mut index = sample();
        index
            .upsert(&entry(60, 5, "My Report 2024.docx", 0))
            .unwrap();
        index.upsert(&entry(61, 5, "report", DIR)).unwrap();
        index.upsert(&entry(62, 22, "IŞIK.png", 0)).unwrap();
        let q = Query::parse(query, FoldOptions::default()).unwrap();
        search(&index, &q, 100)
            .slots
            .into_iter()
            .map(|s| index.name(s).to_owned())
            .collect()
    }

    #[test]
    fn substring_ranking() {
        assert_eq!(
            names("report"),
            [
                "report",
                "Report.PDF",
                "report-old.pdf",
                "My Report 2024.docx"
            ]
        );
    }

    #[test]
    fn and_or_not() {
        assert_eq!(names("report pdf"), ["Report.PDF", "report-old.pdf"]);
        assert_eq!(names("report !old pdf"), ["Report.PDF"]);
        assert_eq!(names("notes|docx"), ["notes.txt", "My Report 2024.docx"]);
        assert_eq!(names("notes | docx"), ["notes.txt", "My Report 2024.docx"]);
    }

    #[test]
    fn filters() {
        assert_eq!(names("ext:pdf"), ["Report.PDF", "report-old.pdf"]);
        assert_eq!(names("ext:.txt;docx"), ["notes.txt", "My Report 2024.docx"]);
        assert_eq!(names("report type:folder"), ["report"]);
        assert_eq!(names("type:dir").len(), 4);
        assert_eq!(names("path:docs pdf"), ["Report.PDF"]);
        assert_eq!(
            names("path:\"me\\docs\""),
            ["Docs", "notes.txt", "IŞIK.png", "Report.PDF"]
        );
    }

    #[test]
    fn wildcards_and_quotes() {
        assert_eq!(names("*.pdf"), ["Report.PDF", "report-old.pdf"]);
        assert_eq!(names("r?port*"), ["report", "Report.PDF", "report-old.pdf"]);
        assert_eq!(names("\"my report\""), ["My Report 2024.docx"]);
        assert_eq!(names("ışık"), Vec::<String>::new()); // dotless i needs the Turkish option
        assert_eq!(names("işik"), ["IŞIK.png"]);
    }

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(names("").len(), 9);
    }

    #[test]
    fn parse_errors() {
        let parse = |q| Query::parse(q, FoldOptions::default()).map(|_| ());
        assert_eq!(
            parse("dc:today"),
            Err(QueryError::NotSupportedYet("dc".into()))
        );
        assert!(matches!(
            parse("size:big"),
            Err(QueryError::InvalidValue { .. })
        ));
        assert!(matches!(
            parse("dm:2024-02-30"),
            Err(QueryError::InvalidValue { .. })
        ));
        assert!(matches!(
            parse("size:10mb..1kb"),
            Err(QueryError::InvalidValue { .. })
        ));
        assert_eq!(
            parse("foo:bar"),
            Err(QueryError::UnknownFilter("foo".into()))
        );
        assert!(matches!(
            parse("re:(unclosed"),
            Err(QueryError::InvalidRegex(_))
        ));
        assert_eq!(parse("ext:"), Err(QueryError::MissingValue("ext".into())));
        assert!(matches!(
            parse("type:pdf"),
            Err(QueryError::InvalidValue { .. })
        ));
        assert_eq!(parse("a |"), Err(QueryError::DanglingOperator));
        assert_eq!(parse("!"), Err(QueryError::DanglingOperator));
    }

    #[test]
    fn regex_filter() {
        assert_eq!(names(r"re:^REPORT\.pdf$"), ["Report.PDF"]);
        assert_eq!(names(r"re:\d{4}"), ["My Report 2024.docx"]);
        assert_eq!(names(r"re:\D+-old"), ["report-old.pdf"]);
        assert_eq!(names(r#"re:"^my report""#), ["My Report 2024.docx"]);
        assert_eq!(
            names(r"re:^(?:notes|docs)$|^NOTES\."),
            ["Docs", "notes.txt"]
        );
    }

    #[test]
    fn regex_literals_are_folded_but_escapes_are_kept() {
        let tr = FoldOptions { turkish_i: true };
        let d = FoldOptions::default();
        assert_eq!(fold_regex_literals(r"^ABC\D\W[A-Z]", d), r"^abc\D\W[a-z]");
        assert_eq!(
            fold_regex_literals(r"\p{Lu}\pL\x{130}İ", d),
            r"\p{Lu}\pL\x{130}i"
        );
        assert_eq!(
            fold_regex_literals(r"(?U)A+(?P<Name>B)", d),
            r"(?U)a+(?P<Name>b)"
        );
        assert_eq!(fold_regex_literals("IŞIK", tr), "işik");
    }

    #[test]
    fn size_values() {
        assert_eq!(parse_size_range(">1mb"), Some((1_048_577, u64::MAX)));
        assert_eq!(parse_size_range(">=1MB"), Some((1_048_576, u64::MAX)));
        assert_eq!(parse_size_range("<1kb"), Some((0, 1023)));
        assert_eq!(parse_size_range("1.5k"), Some((1536, 1536)));
        assert_eq!(parse_size_range("10kb..1mb"), Some((10_240, 1_048_576)));
        assert_eq!(parse_size_range("..100"), Some((0, 100)));
        assert_eq!(parse_size_range("empty"), Some((0, 0)));
        assert_eq!(parse_size_range("<0"), None);
        assert_eq!(parse_size_range("5 parsecs"), None);
    }

    #[test]
    fn size_and_date_filters() {
        let mut index = Index::new("C:\\", FoldOptions::default());
        let june_noon = time::local_midnight(2024, 6, 1).unwrap() + 12 * 3600 * 10_000_000;
        let add = |index: &mut Index, frn, name: &str, size, mtime, attributes| {
            index
                .upsert(&NewEntry {
                    size,
                    mtime,
                    ..entry(frn, 5, name, attributes)
                })
                .unwrap();
        };
        add(&mut index, 10, "tiny.txt", 10, june_noon, 0);
        add(&mut index, 11, "big.iso", 3 << 30, june_noon + time::DAY, 0);
        add(&mut index, 12, "empty.log", 0, time::now(), 0);
        add(&mut index, 13, "folder", 0, time::now(), DIR);
        add(&mut index, 14, "no-time.bin", 5, 0, 0);
        let names = |q: &str| {
            let query = Query::parse(q, FoldOptions::default()).unwrap();
            let mut names: Vec<String> = search(&index, &query, 100)
                .slots
                .into_iter()
                .map(|s| index.name(s).to_owned())
                .collect();
            names.sort();
            names
        };
        assert_eq!(names("size:>1gb"), ["big.iso"]);
        assert_eq!(names("size:empty"), ["empty.log"]);
        assert_eq!(names("size:<1kb"), ["empty.log", "no-time.bin", "tiny.txt"]);
        assert_eq!(names("dm:2024-06-01"), ["tiny.txt"]);
        assert_eq!(names("dm:2024-06-01..2024-06-02"), ["big.iso", "tiny.txt"]);
        assert_eq!(names("dm:>2024-06-01 size:>0"), ["big.iso"]);
        assert_eq!(names("dm:today"), ["empty.log", "folder"]);
        assert_eq!(names("dm:<2024-06-02"), ["tiny.txt"]);
    }

    #[test]
    fn wildcard_matcher() {
        assert!(wildcard_match(b"*", b""));
        assert!(wildcard_match(b"a*c", b"abbbc"));
        assert!(!wildcard_match(b"a*c", b"abbbd"));
        assert!(wildcard_match("?ehir".as_bytes(), "şehir".as_bytes()));
        assert!(wildcard_match(b"*.tar.*", b"x.tar.gz"));
    }

    #[test]
    fn multi_volume_sorting_and_paging() {
        let c = sample();
        let mut d = Index::new("D:\\", FoldOptions::default());
        d.upsert(&NewEntry {
            size: 900,
            ..entry(10, 5, "report-big.pdf", 0)
        })
        .unwrap();
        d.upsert(&NewEntry {
            size: 5,
            ..entry(11, 5, "a-report.txt", 0)
        })
        .unwrap();
        let query = Query::parse("report", FoldOptions::default()).unwrap();
        let run = |sort, descending, offset, limit| {
            let options = SearchOptions {
                sort,
                descending,
                offset,
                limit,
            };
            let indexes = [&c, &d];
            let result = search_volumes(&indexes, &query, &options);
            let paths: Vec<String> = result
                .hits
                .iter()
                .map(|h| indexes[h.volume].path(h.slot))
                .collect();
            (result.total, paths)
        };

        let (total, by_name) = run(SortBy::Name, false, 0, 10);
        assert_eq!(total, 4);
        assert_eq!(
            by_name,
            [
                r"D:\a-report.txt",
                r"D:\report-big.pdf",
                r"C:\report-old.pdf",
                r"C:\Users\me\Docs\Report.PDF"
            ]
        );
        let (_, page) = run(SortBy::Name, false, 1, 2);
        assert_eq!(page, by_name[1..3]);
        let (_, by_size) = run(SortBy::Size, true, 0, 1);
        assert_eq!(by_size, [r"D:\report-big.pdf"]);
        let (_, relevance) = run(SortBy::Relevance, false, 0, 10);
        assert_eq!(relevance[0], r"C:\Users\me\Docs\Report.PDF");
        assert_eq!(run(SortBy::Name, false, 10, 10).1.len(), 0);
    }

    #[test]
    fn limit_and_total() {
        let index = sample();
        let q = Query::parse("", FoldOptions::default()).unwrap();
        let result = search(&index, &q, 2);
        assert_eq!(result.total, 6);
        assert_eq!(result.slots.len(), 2);
        assert!(search(&index, &q, 0).slots.is_empty());
    }
}
