//! `shuttle conflict` subcommand dispatcher.
//!
//! Maps `shuttle conflict <verb>` to the corresponding `shuttle_conflict::*` function
//! and formats output as JSON on stdout (or plain text for `resolve`).

use std::path::Path;

use shuttle_conflict::{
    auto3way, classify, diffstat, get_hunk, list, propose, resolve, stage, verify, ConflictLevel,
    TakeSide,
};

#[derive(Debug, clap::Subcommand)]
pub enum ConflictCmd {
    /// List all conflicted files in the repo.
    List {
        #[arg(long, default_value = ".")]
        root: String,
    },
    /// Get structured hunk data (base/ours/theirs) for a specific hunk.
    GetHunk {
        file: String,
        hunk_id: usize,
        #[arg(long, default_value = ".")]
        root: String,
    },
    /// Classify a hunk's conflict level (L0-L3, deterministic).
    Classify {
        file: String,
        hunk_id: usize,
        #[arg(long, default_value = ".")]
        root: String,
    },
    /// Run mergiraf three-way merge on a hunk.
    Auto3way {
        file: String,
        hunk_id: usize,
        #[arg(long, default_value = ".")]
        root: String,
    },
    /// Deterministic fast-path: take ours, theirs, or their union.
    Take {
        file: String,
        hunk_id: usize,
        #[arg(long)]
        ours: bool,
        #[arg(long)]
        theirs: bool,
        #[arg(long)]
        union: bool,
        #[arg(long, default_value = ".")]
        root: String,
    },
    /// Apply model-proposed merged text for a hunk (L2 tool -- conflict_propose).
    Propose {
        file: String,
        hunk_id: usize,
        /// The merged text to apply (conflict markers not allowed).
        #[arg(long)]
        text: String,
        #[arg(long, default_value = ".")]
        root: String,
    },
    /// Verify a file has no residual conflict markers (gate 1+2).
    Verify {
        file: String,
        #[arg(long, default_value = ".")]
        root: String,
    },
    /// Show diff stats for a file.
    Diffstat {
        file: String,
        #[arg(long, default_value = ".")]
        root: String,
    },
    /// Stage a resolved file (git add -- never commits).
    Stage {
        file: String,
        #[arg(long, default_value = ".")]
        root: String,
    },
    /// Drive the full conflict resolution state machine for a run.
    Resolve {
        /// Run ID (used in receipt filename and log).
        run_id: String,
        #[arg(long, default_value = ".")]
        root: String,
    },
}

pub fn run(cmd: ConflictCmd) -> anyhow::Result<()> {
    match cmd {
        ConflictCmd::List { root } => {
            let files = list(Path::new(&root))?;
            println!("{}", serde_json::to_string_pretty(&files)?);
        }
        ConflictCmd::GetHunk {
            file,
            hunk_id,
            root,
        } => {
            let hunk = get_hunk(Path::new(&root), &file, hunk_id)?;
            println!("{}", serde_json::to_string_pretty(&hunk)?);
        }
        ConflictCmd::Classify {
            file,
            hunk_id,
            root,
        } => {
            let hunk = get_hunk(Path::new(&root), &file, hunk_id)?;
            let level = classify(Path::new(&root), &hunk);
            let s = match level {
                ConflictLevel::Trivial => "trivial",
                ConflictLevel::Structural => "structural",
                ConflictLevel::Semantic => "semantic",
                ConflictLevel::Coupled => "coupled",
            };
            println!(
                "{}",
                serde_json::json!({ "level": s, "hunk_id": hunk_id, "file": file })
            );
        }
        ConflictCmd::Auto3way {
            file,
            hunk_id,
            root,
        } => {
            let hunk = get_hunk(Path::new(&root), &file, hunk_id)?;
            let result = auto3way(Path::new(&root), &hunk);
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        ConflictCmd::Take {
            file,
            hunk_id,
            ours,
            theirs,
            union,
            root,
        } => {
            let side = if ours {
                TakeSide::Ours
            } else if theirs {
                TakeSide::Theirs
            } else if union {
                TakeSide::Union
            } else {
                anyhow::bail!("specify --ours, --theirs, or --union");
            };
            take_hunk(Path::new(&root), &file, hunk_id, side)?;
            println!("{}", serde_json::json!({ "ok": true, "hunk_id": hunk_id }));
        }
        ConflictCmd::Propose {
            file,
            hunk_id,
            text,
            root,
        } => {
            propose(Path::new(&root), &file, hunk_id, &text)?;
            println!("{}", serde_json::json!({ "ok": true, "hunk_id": hunk_id }));
        }
        ConflictCmd::Verify { file, root } => {
            verify(Path::new(&root), &file)?;
            println!("{}", serde_json::json!({ "ok": true, "file": file }));
        }
        ConflictCmd::Diffstat { file, root } => {
            let stat = diffstat(Path::new(&root), &file)?;
            println!("{}", serde_json::to_string_pretty(&stat)?);
        }
        ConflictCmd::Stage { file, root } => {
            stage(Path::new(&root), &file)?;
            println!("{}", serde_json::json!({ "ok": true, "file": file }));
        }
        ConflictCmd::Resolve { run_id, root } => {
            // No LLM callback in CLI mode --- L2 hunks are skipped and marked
            let receipt = resolve::<fn(&shuttle_conflict::ConflictHunk) -> Option<String>>(
                Path::new(&root),
                &run_id,
                None,
            )?;
            println!("{}", serde_json::to_string_pretty(&receipt)?);
        }
    }
    Ok(())
}

// Re-export take with the right call signature
fn take_hunk(repo: &Path, file: &str, hunk_id: usize, side: TakeSide) -> anyhow::Result<()> {
    shuttle_conflict::take(repo, file, hunk_id, side)?;
    Ok(())
}
