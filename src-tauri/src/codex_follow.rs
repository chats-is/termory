//! Codex "follow sessions across provider switch" support.
//!
//! Codex tags every thread row in `~/.codex/state_5.sqlite` with the
//! `model_provider` id that was active when it was created, and
//! `codex resume` only lists threads whose `model_provider` equals the
//! CURRENTLY configured provider (see resume_picker.rs `picker_provider_filter`
//! → `MatchDefault(config.model_provider_id)`). So after Termory switches
//! Codex to a custom API platform (model_provider `termory`), a project's
//! prior official-era sessions (model_provider `openai`) stop appearing in
//! `codex resume`.
//!
//! This module lets the user, at switch time, pick which RECENT projects'
//! sessions should "follow" the switch — i.e. have their `model_provider`
//! re-tagged to the now-active provider so `codex resume` finds them again.
//!
//! What we rewrite (verified against Codex source):
//!   * The `threads` table is only a CACHE. Codex rebuilds each row's
//!     `model_provider` from the rollout JSONL's first-line `session_meta`
//!     (`state/src/extract.rs` `apply_session_meta_from_item`; upsert with no
//!     COALESCE guard at `runtime/threads.rs:754`) on startup backfill / resume
//!     reconcile — so editing the table alone is reverted the moment Codex
//!     runs. The authoritative source is the rollout file.
//!   * So we rewrite the file's first-line `payload.model_provider` (durable)
//!     AND update the table row (immediate visibility). The original mtime is
//!     restored — the resume picker shows session time from file mtime.
//!
//! No per-session journal is kept: the official bucket is ALWAYS `openai` and
//! Termory's custom bucket is ALWAYS `termory`, so the fold target is fully
//! determined by switch direction. Reversal is the symmetric switch-back —
//! exactly like Claude project migrate, which also keeps no journal.

use std::error::Error;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// One recent Codex project offered in the switch-time follow picker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecentCodexProject {
    /// Project working directory (the `threads.cwd` value).
    pub project: String,
    /// Most recent `updated_at` (ms epoch) across that project's threads.
    pub updated_at: i64,
    /// Number of live (non-archived) threads under the project.
    pub session_count: u64,
    /// Distinct `model_provider` buckets currently present under the project,
    /// so the UI can show e.g. "12 sessions · openai" and the caller knows
    /// whether a switch would actually hide anything.
    pub providers: Vec<String>,
}

/// Result of a follow operation, surfaced back to the toast.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FollowResult {
    /// How many thread rows were re-tagged.
    pub moved: u64,
}

fn home() -> Result<PathBuf, Box<dyn Error>> {
    dirs::home_dir().ok_or_else(|| "home directory not available".into())
}

fn state_db_path() -> Result<PathBuf, Box<dyn Error>> {
    Ok(crate::providers::codex_root(&home()?).join("state_5.sqlite"))
}

/// List distinct Codex project cwds newest first. `limit == 0` means no cap
/// (return every project; the picker scrolls). Read-only — never mutates the DB.
pub fn recent_projects(limit: usize) -> Result<Vec<RecentCodexProject>, Box<dyn Error>> {
    let path = state_db_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    recent_projects_in(&conn, limit)
}

/// Inner, connection-injected form for tests. `limit == 0` → no LIMIT clause.
pub fn recent_projects_in(
    conn: &Connection,
    limit: usize,
) -> Result<Vec<RecentCodexProject>, Box<dyn Error>> {
    // One row per project: newest updated_at, live count, and the set of
    // provider buckets present. group_concat gives us the distinct providers.
    // `limit` is a trusted usize (not user text), so formatting it directly is
    // injection-safe; 0 omits the clause entirely.
    let limit_clause = if limit == 0 {
        String::new()
    } else {
        format!(" limit {limit}")
    };
    let sql = format!(
        "select cwd, \
                max(updated_at) as recent, \
                count(*) as cnt, \
                group_concat(distinct model_provider) as providers \
         from threads \
         where archived = 0 and cwd <> '' \
         group by cwd \
         order by recent desc{limit_clause}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        let project: String = row.get(0)?;
        let updated_at: i64 = row.get(1)?;
        let session_count: i64 = row.get(2)?;
        let providers_csv: Option<String> = row.get(3)?;
        let mut providers: Vec<String> = providers_csv
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        providers.sort();
        Ok(RecentCodexProject {
            project,
            updated_at,
            session_count: session_count.max(0) as u64,
            providers,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// A live thread that is a candidate to follow the switch.
struct Candidate {
    id: String,
    rollout_path: String,
}

/// Make the selected projects' live sessions follow the provider switch so
/// `codex resume` lists them under `target_provider_id`. See the module header
/// for WHY both the rollout file and the table are rewritten.
///
/// Safety: backs up the DB first, opens read-write with a busy timeout, and
/// fails fast (without writing) when Codex holds the DB lock.
pub fn follow_projects(
    projects: &[String],
    target_provider_id: &str,
) -> Result<FollowResult, Box<dyn Error>> {
    if projects.is_empty() {
        return Ok(FollowResult { moved: 0 });
    }
    if target_provider_id.is_empty() {
        return Err("target provider id is empty".into());
    }
    let path = state_db_path()?;
    if !path.exists() {
        return Err("Codex state database not found".into());
    }

    // Open RW with a short busy timeout so a running Codex surfaces as a clean
    // "locked" error instead of a panic or partial write. No DB backup is taken:
    // the table is a self-healing cache (Codex rebuilds it from the authoritative
    // rollout files), so snapshotting it protects nothing — reversal is the
    // symmetric switch-back.
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
    )?;
    conn.busy_timeout(std::time::Duration::from_millis(1500))?;

    let result = (|| -> Result<u64, Box<dyn Error>> {
        let candidates = select_candidates_in(&conn, projects, target_provider_id)?;
        if candidates.is_empty() {
            return Ok(0);
        }

        // Rewrite the authoritative rollout file for each candidate FIRST. Only
        // those whose file rewrite succeeds get their table row updated — a file
        // we couldn't rewrite would be reverted by Codex anyway, so don't claim
        // it moved.
        let mut succeeded_ids: Vec<String> = Vec::new();
        for c in &candidates {
            match rewrite_rollout_provider(Path::new(&c.rollout_path), target_provider_id) {
                Ok(_) => succeeded_ids.push(c.id.clone()),
                Err(err) => {
                    log::warn!(
                        "follow: failed rewriting rollout {} : {err}",
                        c.rollout_path
                    );
                }
            }
        }
        if succeeded_ids.is_empty() {
            return Ok(0);
        }

        update_thread_providers_in(&conn, &succeeded_ids, target_provider_id)?;
        Ok(succeeded_ids.len() as u64)
    })();
    match result {
        Ok(moved) => Ok(FollowResult { moved }),
        Err(err) => Err(map_locked(err)),
    }
}

/// SELECT the live threads under `projects` whose `model_provider` differs from
/// the target. Read-only — connection-injected for tests.
fn select_candidates_in(
    conn: &Connection,
    projects: &[String],
    target_provider_id: &str,
) -> Result<Vec<Candidate>, Box<dyn Error>> {
    if projects.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; projects.len()].join(",");
    let select_sql = format!(
        "select id, rollout_path from threads \
         where archived = 0 and model_provider <> ?1 and cwd in ({placeholders})"
    );
    let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(projects.len() + 1);
    params.push(&target_provider_id);
    for p in projects {
        params.push(p);
    }
    let mut stmt = conn.prepare(&select_sql)?;
    let rows = stmt
        .query_map(params.as_slice(), |row| {
            Ok(Candidate {
                id: row.get::<_, String>(0)?,
                rollout_path: row.get::<_, String>(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// UPDATE `threads.model_provider = target` for the given ids (immediate
/// visibility; the rollout-file rewrite keeps it durable). Connection-injected.
fn update_thread_providers_in(
    conn: &Connection,
    ids: &[String],
    target_provider_id: &str,
) -> Result<usize, Box<dyn Error>> {
    if ids.is_empty() {
        return Ok(0);
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    let update_sql = format!("update threads set model_provider = ?1 where id in ({placeholders})");
    let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(ids.len() + 1);
    params.push(&target_provider_id);
    for id in ids {
        params.push(id);
    }
    Ok(conn.execute(&update_sql, params.as_slice())?)
}

/// Rewrite a rollout JSONL's first-line `payload.model_provider` to `target`,
/// streaming the (potentially 100+ MB) remainder unchanged. Returns Ok(false)
/// when there's nothing to change (no payload.model_provider, or already the
/// target). Atomic via temp + rename, and the original mtime is restored
/// afterwards (the resume picker shows session time from file mtime). Durability
/// does NOT rely on the mtime: the table is updated directly and Codex's resume
/// reconcile re-reads the rewritten file content, so the value sticks.
fn rewrite_rollout_provider(path: &Path, target: &str) -> Result<bool, Box<dyn Error>> {
    use std::io::{BufRead, BufReader, BufWriter, Write};

    if !path.exists() {
        return Err(format!("rollout file not found: {}", path.display()).into());
    }
    // Capture the original mtime so we can restore it after the rewrite. The
    // Codex resume picker displays each session's time from the rollout file's
    // mtime, NOT from threads.updated_at — so a naive rewrite would make every
    // kept session read "just now" and destroy the chronological order.
    let original_mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut first_line = String::new();
    if reader.read_line(&mut first_line)? == 0 {
        return Ok(false); // empty file
    }
    let trimmed = first_line.trim_end_matches(['\n', '\r']);
    let mut value: JsonValue = serde_json::from_str(trimmed)?;
    let payload = match value.get_mut("payload").and_then(|p| p.as_object_mut()) {
        Some(p) => p,
        None => return Ok(false),
    };
    match payload.get("model_provider") {
        Some(JsonValue::String(s)) if s == target => return Ok(false),
        Some(JsonValue::String(_)) => {}
        _ => return Ok(false),
    }
    payload.insert(
        "model_provider".into(),
        JsonValue::String(target.to_string()),
    );
    // serde_json has preserve_order enabled (Cargo.toml), so key order survives.
    let new_first = serde_json::to_string(&value)?;

    let mut tmp_name = path.file_name().ok_or("invalid rollout path")?.to_owned();
    tmp_name.push(".termory-tmp");
    let tmp_path = path.with_file_name(tmp_name);
    {
        let out = std::fs::File::create(&tmp_path)?;
        let mut writer = BufWriter::new(out);
        writer.write_all(new_first.as_bytes())?;
        writer.write_all(b"\n")?;
        // Copy the rest of the file (BufReader resumes right after line 1).
        std::io::copy(&mut reader, &mut writer)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
    }
    std::fs::rename(&tmp_path, path)?;

    // Restore the original mtime so the resume picker keeps showing the
    // session's real last-activity time instead of "just now".
    if let Some(mtime) = original_mtime {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
            let _ = f.set_modified(mtime);
        }
    }
    Ok(true)
}

/// Turn a rusqlite "database is locked / busy" error into a friendly message
/// telling the user to quit the running Codex first.
fn map_locked(err: Box<dyn Error>) -> Box<dyn Error> {
    let msg = err.to_string();
    if msg.contains("locked") || msg.contains("busy") {
        return "Codex state database is locked — quit any running Codex and try again.".into();
    }
    err
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(conn: &Connection) {
        conn.execute_batch(
            "create table threads (\
                id text primary key, \
                cwd text not null, \
                model_provider text not null, \
                rollout_path text not null default '', \
                archived integer not null default 0, \
                updated_at integer not null default 0\
             );",
        )
        .unwrap();
        let rows = [
            ("a", "/proj/ip125", "openai", 0, 100),
            ("b", "/proj/ip125", "openai", 0, 200),
            ("c", "/proj/ip125", "custom", 0, 150),
            ("d", "/proj/other", "openai", 0, 300),
            ("e", "/proj/ip125", "openai", 1, 50), // archived — never touched
        ];
        for (id, cwd, mp, arch, upd) in rows {
            conn.execute(
                "insert into threads (id, cwd, model_provider, rollout_path, archived, updated_at) \
                 values (?1,?2,?3,?4,?5,?6)",
                rusqlite::params![id, cwd, mp, format!("/tmp/{id}.jsonl"), arch, upd],
            )
            .unwrap();
        }
    }

    #[test]
    fn recent_projects_groups_and_orders() {
        let conn = Connection::open_in_memory().unwrap();
        seed(&conn);
        let got = recent_projects_in(&conn, 5).unwrap();
        // /proj/other has the newest thread (300) so it leads.
        assert_eq!(got[0].project, "/proj/other");
        assert_eq!(got[1].project, "/proj/ip125");
        // ip125 has 3 live threads (archived one excluded) across two buckets.
        assert_eq!(got[1].session_count, 3);
        assert_eq!(got[1].providers, vec!["custom", "openai"]);
    }

    #[test]
    fn recent_projects_limit_zero_returns_all() {
        let conn = Connection::open_in_memory().unwrap();
        seed(&conn);
        // Two distinct projects exist (ip125, other); limit 0 returns both,
        // limit 1 caps to the most recent.
        assert_eq!(recent_projects_in(&conn, 0).unwrap().len(), 2);
        assert_eq!(recent_projects_in(&conn, 1).unwrap().len(), 1);
    }

    #[test]
    fn select_candidates_picks_only_live_nonmatching_rows() {
        let conn = Connection::open_in_memory().unwrap();
        seed(&conn);
        // ip125 → termory: a,b (openai) + c (custom) are candidates; d (other
        // project) and e (archived) are not.
        let got = select_candidates_in(&conn, &["/proj/ip125".to_string()], "termory").unwrap();
        let mut ids: Vec<&str> = got.iter().map(|c| c.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn select_candidates_empty_when_already_on_target() {
        let conn = Connection::open_in_memory().unwrap();
        seed(&conn);
        // Everything in /proj/other is already openai.
        let got = select_candidates_in(&conn, &["/proj/other".to_string()], "openai").unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn update_thread_providers_sets_only_named_ids() {
        let conn = Connection::open_in_memory().unwrap();
        seed(&conn);
        let n = update_thread_providers_in(&conn, &["a".to_string(), "c".to_string()], "termory")
            .unwrap();
        assert_eq!(n, 2);
        let check = |id: &str| -> String {
            conn.query_row(
                "select model_provider from threads where id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(check("a"), "termory");
        assert_eq!(check("c"), "termory");
        assert_eq!(check("b"), "openai"); // not named → untouched
    }

    #[test]
    fn rewrite_rollout_provider_changes_first_line_keeps_rest() {
        let dir = std::env::temp_dir().join(format!("termory-follow-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout.jsonl");
        // First line is the session_meta; subsequent lines are message records
        // that must survive byte-for-byte.
        let body = concat!(
            "{\"timestamp\":\"t\",\"type\":\"session_meta\",\"payload\":{\"id\":\"x\",\"model_provider\":\"openai\",\"cwd\":\"/p\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"text\":\"hello\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"text\":\"world\"}}\n"
        );
        std::fs::write(&path, body).unwrap();

        let changed = rewrite_rollout_provider(&path, "termory").unwrap();
        assert!(changed);

        let out = std::fs::read_to_string(&path).unwrap();
        let mut lines = out.lines();
        let first: JsonValue = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(first["payload"]["model_provider"], "termory");
        assert_eq!(first["payload"]["cwd"], "/p"); // other fields intact
                                                   // Remaining lines unchanged.
        assert_eq!(
            lines.next().unwrap(),
            "{\"type\":\"response_item\",\"payload\":{\"text\":\"hello\"}}"
        );
        assert_eq!(
            lines.next().unwrap(),
            "{\"type\":\"response_item\",\"payload\":{\"text\":\"world\"}}"
        );

        // Idempotent: rewriting to the same target is a no-op.
        assert!(!rewrite_rollout_provider(&path, "termory").unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }
}
