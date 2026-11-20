//! Index build and query benchmarks on a synthetic volume (SPEC performance targets:
//! query over 2 M entries ≤ 30 ms for substring; index memory ≤ ~80 bytes/entry).

#![allow(clippy::expect_used, reason = "benchmark setup, like a test")]

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use ferro_core::{FoldOptions, Index, Name, NewEntry, Query, SizeTotals, match_mask, search};

const WORDS: &[&str] = &[
    "report",
    "invoice",
    "photo",
    "main",
    "lib",
    "config",
    "test",
    "backup",
    "draft",
    "notes",
    "Ödev",
    "çalışma",
    "ŞEHİR",
    "data",
    "index",
    "module",
    "readme",
    "setup",
    "build",
    "cache",
];
const EXTS: &[&str] = &[
    "pdf", "docx", "rs", "txt", "jpg", "png", "json", "toml", "dll", "exe",
];

/// Deterministic xorshift so every run sees the same tree.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[(self.next() % items.len() as u64) as usize]
    }
}

/// Synthetic entries: one folder per 32 files, folders nested up to 8 deep.
fn synthetic_names(count: usize) -> Vec<(u64, u64, String, u32)> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut out = Vec::with_capacity(count);
    let mut dirs: Vec<u64> = vec![5];
    for i in 0..count as u64 {
        let frn = 16 + i;
        let parent = dirs[(rng.next() % dirs.len() as u64) as usize];
        if i % 32 == 0 {
            let name = format!("{}_{}", rng.pick(WORDS), i % 1000);
            out.push((frn, parent, name, 0x10));
            if dirs.len() < 1 << 16 {
                dirs.push(frn);
            }
        } else {
            let name = format!(
                "{} {}-{}.{}",
                rng.pick(WORDS),
                rng.pick(WORDS),
                rng.next() % 10_000,
                rng.pick(EXTS)
            );
            out.push((frn, parent, name, 0x20));
        }
    }
    out
}

fn build(names: &[(u64, u64, String, u32)]) -> Index {
    let mut index = Index::with_capacity("C:\\", FoldOptions::default(), names.len());
    for (frn, parent, name, attributes) in names {
        index.upsert(&NewEntry {
            frn: *frn,
            parent_frn: *parent,
            name: Name::Utf8(name),
            attributes: *attributes,
            size: if *attributes & 0x10 == 0 {
                frn.wrapping_mul(2_654_435_761) % 50_000_000
            } else {
                0
            },
            alloc_size: 0,
            mtime: *frn,
        });
    }
    index
}

fn benches(c: &mut Criterion) {
    let names = synthetic_names(2_000_000);
    let index = build(&names);
    eprintln!(
        "index: {} entries, {:.1} bytes/entry without names, {:.1} MB total",
        index.len(),
        index.heap_bytes_without_names() as f64 / index.len() as f64,
        index.heap_bytes() as f64 / 1e6
    );

    let mut group = c.benchmark_group("build");
    group.sample_size(10);
    group.bench_function("upsert 1M entries", |b| {
        b.iter(|| black_box(build(&names[..1_000_000])))
    });
    group.finish();

    let mut group = c.benchmark_group("query 2M");
    for text in [
        "report",
        "şehir",
        "ext:pdf",
        "report pdf !draft",
        "*.toml",
        "invoice|photo jpg",
        "path:report_1 main",
        r"re:^report.*\.pdf$",
        r"re:\d{4}\.(jpg|png)$",
        "zzzz-no-match",
    ] {
        let query = Query::parse(text, FoldOptions::default()).expect("valid query");
        group.bench_function(text, |b| b.iter(|| black_box(search(&index, &query, 100))));
    }
    group.finish();

    let totals = SizeTotals::build(&index, None);
    eprintln!(
        "size totals: {:.1} bytes/entry",
        totals.heap_bytes() as f64 / index.len() as f64
    );
    let mut group = c.benchmark_group("size totals 2M");
    group.sample_size(10);
    group.bench_function("build", |b| {
        b.iter(|| black_box(SizeTotals::build(&index, None)))
    });
    let filter = Query::parse("ext:jpg;png", FoldOptions::default()).expect("valid query");
    group.bench_function("build ext:jpg;png", |b| {
        b.iter(|| {
            let mask = match_mask(&index, &filter);
            black_box(SizeTotals::build(&index, Some(mask)))
        })
    });
    group.bench_function("largest 100 + extensions 50", |b| {
        b.iter(|| {
            black_box(totals.largest_files(&index, None, 100));
            black_box(totals.extensions(&index, None, 50))
        })
    });
    group.finish();
}

criterion_group!(search_benches, benches);
criterion_main!(search_benches);
