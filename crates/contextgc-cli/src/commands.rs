//! Implementations of the `contextgc` subcommands.

use anyhow::{Context as _, Result};
use contextgc_core::SessionId;
use contextgc_store::Store;
use std::io::Read;
use std::path::Path;

use crate::common;

/// `contextgc ingest` — load JSONL context items into a session.
pub fn ingest(
    db: Option<&Path>,
    session: &str,
    config: Option<&Path>,
    file: Option<&Path>,
    model_name: Option<String>,
    context_window: Option<u64>,
    reserved_output: Option<u64>,
) -> Result<()> {
    let db_path = common::resolve_db_path(db);
    let mut sess = common::open_session(
        &db_path,
        session,
        config,
        model_name,
        context_window,
        reserved_output,
    )?;

    let lines: Vec<String> = match file {
        Some(path) => {
            let mut buf = String::new();
            std::fs::File::open(path)
                .with_context(|| format!("open {}", path.display()))?
                .read_to_string(&mut buf)?;
            buf.lines().map(|l| l.to_string()).collect()
        }
        None => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf.lines().map(|l| l.to_string()).collect()
        }
    };
    let count = common::ingest_lines(&mut sess, lines.into_iter())?;
    println!("ingested {count} items into session '{session}'");
    Ok(())
}

/// `contextgc status` — human-readable session report.
pub fn status(
    db: Option<&Path>,
    session: &str,
    config: Option<&Path>,
    json: bool,
    model_name: Option<String>,
    context_window: Option<u64>,
    reserved_output: Option<u64>,
) -> Result<()> {
    let db_path = common::resolve_db_path(db);
    let mut sess = common::open_session(
        &db_path,
        session,
        config,
        model_name,
        context_window,
        reserved_output,
    )?;
    let status = sess.status()?;

    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }

    let rule = "─".repeat(40);
    println!("ContextGC");
    println!("{rule}");
    println!("Model      {}", status.model_name);
    println!("Window     {}", status.context_window);
    println!(
        "Current    {} tokens ({:.1}%)",
        status.current_tokens,
        status.pressure * 100.0
    );
    println!(
        "Predicted  {:.0} tokens ({:.1}%)",
        status.predicted_pressure * status.usable_context as f32,
        status.predicted_pressure * 100.0
    );
    println!("Pressure   {:?}", status.pressure_state);
    println!("Items      {} active", status.item_count);
    println!("Composition");
    println!("{rule}");
    for (kind, tokens) in &status.composition {
        println!("  {kind:<24} {tokens}");
    }
    println!("Top reclaim candidates");
    println!("{rule}");
    for (i, c) in status.top_candidates.iter().enumerate() {
        let savings = c.tokens_before.saturating_sub(c.tokens_after);
        println!(
            "{}. {:?} {} — {} → {} tokens (saves {savings})",
            i + 1,
            c.action,
            c.context_id.as_str(),
            c.tokens_before,
            c.tokens_after
        );
        println!("   importance {:.2} — {}", c.importance, c.reason);
    }
    if status.top_candidates.is_empty() {
        println!("  (none — context is within budget)");
    }
    Ok(())
}

/// `contextgc plan` — print the compaction plan without applying it.
pub fn plan(
    db: Option<&Path>,
    session: &str,
    config: Option<&Path>,
    predicted_extra: u64,
    model_name: Option<String>,
    context_window: Option<u64>,
    reserved_output: Option<u64>,
) -> Result<()> {
    let db_path = common::resolve_db_path(db);
    let mut sess = common::open_session(
        &db_path,
        session,
        config,
        model_name,
        context_window,
        reserved_output,
    )?;
    let prediction = if predicted_extra > 0 {
        Some(contextgc_core::TokenPrediction {
            expected_next_input: 0,
            expected_next_tool_output: predicted_extra,
            expected_next_assistant_output: 0,
            confidence: 0.5,
        })
    } else {
        None
    };
    let plan = sess.plan(prediction)?;
    println!("{}", serde_json::to_string_pretty(&plan)?);
    Ok(())
}

/// `contextgc compact` — plan, apply, and print the resulting working set.
#[allow(clippy::too_many_arguments)]
pub fn compact(
    db: Option<&Path>,
    session: &str,
    config: Option<&Path>,
    predicted_extra: u64,
    json: bool,
    model_name: Option<String>,
    context_window: Option<u64>,
    reserved_output: Option<u64>,
) -> Result<()> {
    let db_path = common::resolve_db_path(db);
    let mut sess = common::open_session(
        &db_path,
        session,
        config,
        model_name,
        context_window,
        reserved_output,
    )?;
    let prediction = if predicted_extra > 0 {
        Some(contextgc_core::TokenPrediction {
            expected_next_input: 0,
            expected_next_tool_output: predicted_extra,
            expected_next_assistant_output: 0,
            confidence: 0.5,
        })
    } else {
        None
    };
    let before = sess.status()?.current_tokens;
    let ws = sess.compact(prediction)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&ws)?);
        return Ok(());
    }

    let reclaimed = before.saturating_sub(ws.token_count);
    println!("compacted session '{session}'");
    println!(
        "  tokens: {before} → {} (reclaimed {reclaimed})",
        ws.token_count
    );
    println!("  items in working set: {}", ws.items.len());
    Ok(())
}

/// `contextgc stats` — local telemetry for a session.
pub fn stats(
    db: Option<&Path>,
    session: &str,
    config: Option<&Path>,
    json: bool,
    model_name: Option<String>,
    context_window: Option<u64>,
    reserved_output: Option<u64>,
) -> Result<()> {
    let db_path = common::resolve_db_path(db);
    let sess = common::open_session(
        &db_path,
        session,
        config,
        model_name,
        context_window,
        reserved_output,
    )?;

    // Re-open the store read-only for aggregate queries.
    let store = Store::open(&db_path)?;
    let sid = SessionId::new(session);
    let runs = store.compaction_history(&sid)?;
    let (item_count, total_tokens) = store.history_totals(&sid)?;

    let compactions = runs.len() as u64;
    let emergency = runs
        .iter()
        .filter(|r| r.pressure_state == "Emergency")
        .count() as u64;
    let reclaimed: u64 = runs
        .iter()
        .map(|r| r.before_tokens.saturating_sub(r.after_tokens))
        .sum();

    let json_out = serde_json::json!({
        "session_id": session,
        "model": sess.model().name,
        "context_window": sess.model().context_window,
        "history_items": item_count,
        "history_tokens": total_tokens,
        "compactions": compactions,
        "emergency_compactions": emergency,
        "tokens_reclaimed": reclaimed,
    });

    if json {
        println!("{}", serde_json::to_string_pretty(&json_out)?);
        return Ok(());
    }

    println!("ContextGC stats — {session}");
    println!("  history items      {item_count}");
    println!("  history tokens     {total_tokens}");
    println!("  compactions        {compactions}");
    println!("  emergency runs     {emergency}");
    println!("  tokens reclaimed   {reclaimed}");
    Ok(())
}
