//! `gitview probe-reply <pane> "<text>"` — the day-one de-risk harness for
//! reply capture. Not part of the plugin surface; it exists to answer one
//! question before any of the thread model is built:
//!
//!   can we send one question to one agent and recover EXACTLY that turn's
//!   reply, or can we not?
//!
//! It runs the real cycle against a live agent and prints both extractions
//! side by side with byte counts, so the transcript path can be compared
//! against the screen fallback on the same turn. Ship nothing else until the
//! transcript path returns the exact reply ≥9/10 and the screen fallback
//! visibly degrades rather than lying.

use std::ffi::OsString;
use std::time::Instant;

use anyhow::{Result, bail};

use crate::agentio::{self, PromptError};

/// `gitview probe-harvest <transcript> <offset> "<prompt>"` — exercise the
/// transcript parser alone, against a real file at a real watermark, with no
/// agent and nothing sent. This is how the harvest gets its ≥10 runs without
/// needing ≥10 live turns.
pub fn run_harvest() -> Result<()> {
    let mut args = std::env::args().skip(2);
    let (Some(path), Some(offset)) = (args.next(), args.next()) else {
        bail!("usage: gitview probe-harvest <transcript> <offset> [\"<prompt>\"]");
    };
    let handle = agentio::Handle {
        pane: String::new(),
        seq0: 0,
        transcript: Some(std::path::PathBuf::from(path)),
        offset: offset.parse()?,
        prompt: args.next().unwrap_or_default(),
    };
    match agentio::harvest_transcript(&handle) {
        Some(t) => {
            println!("{} bytes, {} lines", t.len(), t.lines().count());
            println!("---8<---\n{t}\n--->8---");
        }
        None => println!("<nothing captured>"),
    }
    Ok(())
}

pub fn run() -> Result<()> {
    let mut args = std::env::args().skip(2);
    let (Some(pane), Some(text)) = (args.next(), args.next()) else {
        bail!("usage: gitview probe-reply <pane> \"<text>\" [timeout_ms]");
    };
    let timeout: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(600_000);
    let bin: OsString = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());

    // ---- 1. pre-send gate + watermark ------------------------------------
    let before = agentio::agent_get(&bin, &pane).map_err(|e| anyhow::anyhow!(e))?;
    println!("== before ==");
    println!("  pane          {}", before.pane);
    println!("  agent         {}", before.agent);
    println!("  status        {}", before.status.as_str());
    println!("  seq0          {}", before.state_change_seq);
    match &before.session {
        Some(s) => println!("  session       {} kind={} {}", s.agent, s.kind, s.value),
        None => println!("  session       <none>"),
    }

    let handle = match agentio::begin_turn(&bin, &pane, &text) {
        Ok(h) => h,
        Err(e) => {
            // A refusal here is a PASS for the gate, not a failure of the
            // probe: prompting a working agent is the mis-attribution bug.
            println!("\n== GATE REFUSED ==");
            println!("  {e}");
            println!("  delivered: {:?}", e.delivered());
            return Ok(());
        }
    };
    match handle.transcript.as_deref() {
        Some(p) => println!("  transcript    {} (offset {})", p.display(), handle.offset),
        None => println!("  transcript    <unresolved — screen fallback only>"),
    }

    // ---- 2. submit through the agent surface ------------------------------
    println!("\n== prompting (timeout {timeout}ms) ==");
    let started = Instant::now();
    let after = match agentio::prompt_wait(&bin, &handle, timeout) {
        Ok(info) => info,
        Err(e) => {
            println!("  FAILED: {e}");
            println!("  delivered: {:?}", e.delivered());
            if matches!(e, PromptError::Timeout | PromptError::Stalled) {
                println!("  -> delivery UNPROVEN; checking whether the transcript grew anyway");
                report_harvest(&bin, &handle);
            }
            return Ok(());
        }
    };
    let elapsed = started.elapsed();

    // ---- 3. did a real turn happen? ---------------------------------------
    println!("  elapsed       {:?}", elapsed);
    println!("  status        {}", after.status.as_str());
    println!("  seqN          {}", after.state_change_seq);
    let advanced = agentio::turn_advanced(&handle, &after);
    println!(
        "  turn advanced {}  ({} -> {})",
        if advanced { "YES" } else { "NO — STALE MATCH" },
        handle.seq0,
        after.state_change_seq
    );
    if !advanced {
        println!("  -> --wait matched a stale state; text must NOT be attributed to this turn");
    }

    // ---- 4. harvest both ways ---------------------------------------------
    report_harvest(&bin, &handle);
    Ok(())
}

fn report_harvest(bin: &OsString, handle: &agentio::Handle) {
    let transcript = agentio::harvest_transcript(handle);
    let screen = agentio::harvest_screen(bin, handle);

    println!("\n== transcript harvest ==");
    match &transcript {
        Some(t) => {
            println!("  {} bytes, {} lines", t.len(), t.lines().count());
            println!("  ---8<---");
            for line in t.lines() {
                println!("  {line}");
            }
            println!("  --->8---");
        }
        None => println!("  <nothing — unresolved path, or no assistant text after the watermark>"),
    }

    println!("\n== screen harvest (fallback) ==");
    match &screen {
        Some(s) => {
            println!("  {} bytes, {} lines", s.len(), s.lines().count());
            println!("  ---8<---");
            for line in s.lines() {
                println!("  {line}");
            }
            println!("  --->8---");
        }
        None => println!("  <nothing>"),
    }

    println!("\n== verdict ==");
    match (&transcript, &screen) {
        (Some(t), Some(s)) => {
            let ratio = if t.is_empty() {
                0.0
            } else {
                s.len() as f64 / t.len() as f64
            };
            println!(
                "  transcript {} bytes vs screen {} bytes (screen kept {:.0}%)",
                t.len(),
                s.len(),
                ratio * 100.0
            );
            if ratio < 0.9 {
                println!("  -> screen LOST content, as expected for a reply over the viewport");
            }
        }
        (Some(t), None) => println!("  transcript only ({} bytes); screen returned nothing", t.len()),
        (None, Some(s)) => println!("  SCREEN ONLY ({} bytes) — transcript path failed", s.len()),
        (None, None) => println!("  NOTHING CAPTURED — this is the failure mode that kills item 4"),
    }
}
