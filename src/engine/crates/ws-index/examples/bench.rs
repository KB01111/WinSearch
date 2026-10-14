//! Index/search benchmark: measures the production retrieval path on a
//! synthetic, realistically-shaped dataset.
//!
//! Run: `cargo run --release -p ws-index --example bench [entries]`
//!
//! The numbers this prints are the ones quoted in `docs/benchmarks.md`.

use std::time::Instant;

use ws_index::model::{EntryRecord, SearchRequest};
use ws_index::IndexStore;

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);

    let dir = std::env::temp_dir().join("winsearch-bench-index");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("bench.db");

    let cpus = std::thread::available_parallelism().map(|p| p.get()).unwrap_or(0);
    println!("winsearch index/search benchmark");
    println!("entries: {n}, cpus: {cpus}");

    let mut store = IndexStore::open(&db_path).expect("open bench db");
    let root = store.ensure_root("C:\\bench").unwrap();
    let gen = store.begin_generation(root).unwrap();

    // Realistic-ish mix: documents, media, dev trees, deep paths.
    let records: Vec<EntryRecord> = (0..n)
        .map(|i| {
            let name = synthetic_name(i);
            let path = format!(
                "C:\\bench\\{}\\{}\\{}",
                synthetic_area(i),
                (i % 40),
                name
            );
            EntryRecord {
                root_id: root,
                parent_id: None,
                path,
                name,
                ext: synthetic_ext(i),
                is_dir: false,
                size: (i as u64 * 977) % 5_000_000,
                mtime: 1_700_000_000 + (i as i64 * 37),
                volume_serial: 12345,
                file_id: i as u64 + 1,
                is_app: i % 500 == 0,
                app_target: String::new(),
                keywords: String::new(),
            }
        })
        .collect();

    // ---- bulk insert ----
    let t0 = Instant::now();
    for chunk in records.chunks(1000) {
        store.apply_batch(root, gen, chunk).unwrap();
    }
    let insert = t0.elapsed();

    // ---- incremental re-upsert (everything unchanged) ----
    let t0 = Instant::now();
    for chunk in records.chunks(1000) {
        store.apply_batch(root, gen, chunk).unwrap();
    }
    let reupsert = t0.elapsed();

    // ---- modified re-upsert (every 10th row changed) ----
    let mut changed = records.clone();
    for (i, r) in changed.iter_mut().enumerate() {
        if i % 10 == 0 {
            r.mtime += 1;
        }
    }
    let t0 = Instant::now();
    for chunk in changed.chunks(1000) {
        store.apply_batch(root, gen, chunk).unwrap();
    }
    let update = t0.elapsed();

    // ---- query latency ----
    let queries = [
        "report",
        "report-2024",
        "vacation photos",
        "img",
        "notes",
        "quarterly",
        "s",
        "presentation final",
        "invoice",
        "zzz-no-match",
    ];
    let mut latencies = Vec::new();
    for q in &queries {
        for _ in 0..50 {
            let t = Instant::now();
            let hits = store
                .search(&SearchRequest {
                    text: (*q).into(),
                    limit: 20,
                    ..Default::default()
                })
                .unwrap();
            latencies.push((t.elapsed(), hits.len()));
        }
    }
    latencies.sort_by_key(|(d, _)| *d);
    let p = |percentile: f64| {
        let idx = ((latencies.len() as f64 - 1.0) * percentile).round() as usize;
        latencies[idx].0
    };
    let matched = latencies.iter().filter(|(_, h)| *h > 0).count();

    // ---- per-source breakdown (what the query latency is spent on) ----
    {
        use rusqlite::{params, Connection};
        let conn = Connection::open(&db_path).unwrap();

        let mut prefix_times = Vec::new();
        let mut tri_times = Vec::new();
        let mut fts_times = Vec::new();
        for q in &queries {
            let tokens: Vec<String> = q
                .split(|c: char| !c.is_alphanumeric())
                .filter(|t| !t.is_empty())
                .map(|t| format!("\"{t}\"*"))
                .collect();
            let match_expr = tokens.join(" ");
            for _ in 0..50 {
                let t = Instant::now();
                let mut chars: Vec<char> = q.to_lowercase().chars().collect();
                let end = chars
                    .pop()
                    .and_then(|c| char::from_u32(c as u32 + 1))
                    .map(|bumped| {
                        chars.push(bumped);
                        chars.iter().collect::<String>()
                    })
                    .unwrap_or_default();
                let n: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM (
                           SELECT id FROM entries
                           WHERE name_lower >= ?1 AND name_lower < ?2 LIMIT 500)",
                        params![q.to_lowercase(), end],
                        |r| r.get(0),
                    )
                    .unwrap();
                let _ = n;
                prefix_times.push(t.elapsed());

                let t = Instant::now();
                let n: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM (
                           SELECT rowid FROM entries_tri WHERE entries_tri MATCH ?1 LIMIT 500)",
                        params![format!("\"{}\"", q.to_lowercase().replace('\"', "\"\""))],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                let _ = n;
                tri_times.push(t.elapsed());

                let t = Instant::now();
                let n: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM (
                           SELECT rowid FROM entries_fts WHERE entries_fts MATCH ?1 LIMIT 500)",
                        params![match_expr],
                        |r| r.get(0),
                    )
                    .unwrap();
                let _ = n;
                fts_times.push(t.elapsed());
            }
        }
        let mid = |v: &mut Vec<std::time::Duration>| {
            v.sort();
            v[v.len() / 2]
        };
        println!();
        println!(
            "source breakdown (median): prefix_like {:.2?}, trigram_match {:.2?}, fts_tokens {:.2?}",
            mid(&mut prefix_times),
            mid(&mut tri_times),
            mid(&mut fts_times),
        );
    }

    let stats = store.stats().unwrap();

    println!();
    println!("bulk insert ({n} entries):      {:>10.2?}  ({:.0} entries/s)", insert, n as f64 / insert.as_secs_f64());
    println!("re-upsert (all unchanged):      {:>10.2?}  ({:.0} entries/s)", reupsert, n as f64 / reupsert.as_secs_f64());
    println!("re-upsert (10% modified):       {:>10.2?}  ({:.0} entries/s)", update, n as f64 / update.as_secs_f64());
    println!();
    println!("query latency ({} queries, {}/{} return hits):", latencies.len(), matched, latencies.len());
    println!("  p50: {:>8.2?}", p(0.50));
    println!("  p95: {:>8.2?}", p(0.95));
    println!("  p99: {:>8.2?}", p(0.99));
    println!("  max: {:>8.2?}", latencies.last().unwrap().0);
    println!();
    println!(
        "index size: {:.1} MiB ({} entries)",
        stats.index_size_bytes as f64 / 1_048_576.0,
        stats.entries_total
    );

    let _ = std::fs::remove_dir_all(&dir);
}

fn synthetic_area(i: usize) -> &'static str {
    match i % 5 {
        0 => "Documents",
        1 => "Downloads",
        2 => "Pictures",
        3 => "Projects\\winsearch\\src",
        _ => "Music",
    }
}

fn synthetic_ext(i: usize) -> String {
    match i % 8 {
        0 => "pdf",
        1 => "txt",
        2 => "png",
        3 => "mp3",
        4 => "rs",
        5 => "docx",
        6 => "mp4",
        _ => "log",
    }
    .into()
}

fn synthetic_name(i: usize) -> String {
    let kind = i % 8;
    let ext = synthetic_ext(i);
    // The trailing index guarantees unique paths while keeping the naming
    // shapes realistic (report-2024-03-1234.pdf etc.).
    match kind {
        0 => format!("report-{:04}-{:02}-{:06}.{}", 2020 + i % 6, i % 12, i, ext),
        1 => format!("meeting notes {:06}.{}", i, ext),
        2 => format!("IMG_{:06}.{}", i, ext),
        3 => format!("track-{:02}-{:06}.{}", i % 20, i, ext),
        4 => format!("module_{:06}.{}", i, ext),
        5 => format!("Presentation final ({:02})-{:06}.{}", i % 30, i, ext),
        6 => format!("vacation-{:04}-{:06}.{}", 2018 + i % 8, i, ext),
        _ => format!("archive-copy-{:06}.{}", i, ext),
    }
}
