//! Git plumbing.
//!
//! The point of this module is that you should not have to commit — let alone
//! push — just to find out whether your change builds on the other machine.
//! We capture the working tree as a patch against a commit the peer can obtain,
//! ship the patch, and apply it into a reusable worktree on the far side.
//!
//! The worktree is reused rather than recreated because the build cache in
//! `target/` is what makes remote verification fast enough to sit in the inner
//! development loop instead of feeling like CI.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// zstd level 3 is the knee of the curve here: a source diff compresses ~4x and
/// the CPU cost stays under the network round trip we are saving.
const ZSTD_LEVEL: i32 = 3;

// ---------------------------------------------------------------- helpers

async fn run(root: &Path, args: &[&str]) -> Result<std::process::Output> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .await
        .with_context(|| format!("failed to spawn `git {}`", args.join(" ")))?;
    Ok(out)
}

/// Run git, requiring success, and return trimmed stdout.
async fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = run(root, args).await?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run git for its exit status only, treating failure as `false` rather than an
/// error — used for the various `--verify` / `--is-ancestor` probes.
async fn git_probe(root: &Path, args: &[&str]) -> bool {
    matches!(run(root, args).await, Ok(o) if o.status.success())
}

// ---------------------------------------------------------------- discovery

/// Walk up from `start` to the repository root.
pub async fn discover_root(start: &Path) -> Result<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(start)
        .output()
        .await
        .context("failed to spawn git; is it on PATH?")?;
    if !out.status.success() {
        bail!("{} is not inside a git repository", start.display());
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(PathBuf::from(path))
}

pub async fn head_sha(root: &Path) -> Result<String> {
    git(root, &["rev-parse", "HEAD"]).await
}

pub async fn has_commit(root: &Path, sha: &str) -> bool {
    // `--verify <sha>^{commit}` is the check that the object exists *and* is a
    // commit; plain rev-parse succeeds on any 40-hex string.
    git_probe(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{sha}^{{commit}}"),
        ],
    )
    .await
}

pub async fn current_branch(root: &Path) -> Result<String> {
    let b = git(root, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    if b == "HEAD" {
        bail!("HEAD is detached; check out a branch to push");
    }
    Ok(b)
}

/// Resolve any revision the user typed (`HEAD~2`, a branch, a short sha) to a
/// full commit sha, since that is what travels on the wire.
pub async fn resolve_ref(root: &Path, rev: &str) -> Result<String> {
    git(
        root,
        &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
    )
    .await
    .with_context(|| format!("`{rev}` does not name a commit in this repo"))
}

/// Push the current branch, setting upstream on first push.
pub async fn push_current(root: &Path, remote: &str) -> Result<()> {
    let branch = current_branch(root).await?;
    let out = run(root, &["push", "--set-upstream", remote, &branch]).await?;
    if !out.status.success() {
        bail!(
            "git push failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Newest ancestor of HEAD that also exists on the remote, i.e. the newest
/// commit the peer can obtain with a plain fetch. Used as a fallback base when
/// the peer turns out not to have HEAD (local-only commits).
pub async fn shared_base(root: &Path, remote: &str) -> Result<String> {
    // Candidate remote tips, most specific first: the branch's own upstream,
    // then the remote's default branch, then the conventional names.
    let upstream = git(root, &["rev-parse", "--abbrev-ref", "@{upstream}"])
        .await
        .ok();
    let mut candidates: Vec<String> = Vec::new();
    if let Some(u) = upstream {
        candidates.push(u);
    }
    for name in ["HEAD", "main", "master"] {
        candidates.push(format!("{remote}/{name}"));
    }

    for cand in &candidates {
        if !git_probe(root, &["rev-parse", "--verify", "--quiet", cand]).await {
            continue;
        }
        if let Ok(base) = git(root, &["merge-base", "HEAD", cand]).await
            && !base.is_empty()
        {
            return Ok(base);
        }
    }
    bail!(
        "cannot find a commit shared with remote `{remote}`; push the branch once, \
         or pass --base <sha>"
    )
}

// ---------------------------------------------------------------- snapshot

#[derive(Debug)]
pub struct Snapshot {
    /// zstd-compressed unified diff. Empty when there is nothing to apply.
    pub patch: Vec<u8>,
    /// Uncompressed size, reported so the compression ratio is visible.
    pub raw_len: usize,
    pub files_changed: usize,
}

impl Snapshot {
    pub fn is_clean(&self) -> bool {
        self.patch.is_empty()
    }
}

/// Capture everything that differs between `base` and the current working tree,
/// including staged, unstaged, and untracked files.
///
/// This deliberately builds the tree in a *scratch* index file. Using the real
/// index would mean `sw verify` mutates the user's staging area as a side
/// effect, which would be an unacceptable surprise in the middle of a commit.
pub async fn snapshot(root: &Path, base: &str, state_dir: &Path) -> Result<Snapshot> {
    tokio::fs::create_dir_all(state_dir).await?;
    let index_path = state_dir.join("snapshot.index");
    // A stale index from a previous run would silently poison the tree we build.
    let _ = tokio::fs::remove_file(&index_path).await;

    let idx = index_path.as_os_str();
    // `add -A` honours .gitignore, so build outputs never enter the patch.
    for args in [vec!["read-tree", base], vec!["add", "-A"]] {
        let out = Command::new("git")
            .args(&args)
            .current_dir(root)
            .env("GIT_INDEX_FILE", idx)
            .output()
            .await?;
        if !out.status.success() {
            bail!(
                "git {} failed while snapshotting: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }

    let tree = {
        let out = Command::new("git")
            .args(["write-tree"])
            .current_dir(root)
            .env("GIT_INDEX_FILE", idx)
            .output()
            .await?;
        if !out.status.success() {
            bail!(
                "git write-tree failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    // --binary implies --full-index, which is what makes the patch reliably
    // applicable on a machine with different line-ending settings.
    let out = run(root, &["diff", "--binary", "--no-color", base, &tree]).await?;
    if !out.status.success() {
        bail!(
            "git diff failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let raw = out.stdout;
    let files_changed = raw
        .split(|&b| b == b'\n')
        .filter(|l| l.starts_with(b"diff --git "))
        .count();

    let _ = tokio::fs::remove_file(&index_path).await;

    let patch = if raw.is_empty() {
        Vec::new()
    } else {
        zstd::encode_all(&raw[..], ZSTD_LEVEL)?
    };
    Ok(Snapshot {
        patch,
        raw_len: raw.len(),
        files_changed,
    })
}

// ---------------------------------------------------------------- peer side

/// Why a worktree could not be prepared.
///
/// `MissingBase` is separated out because it is the one failure the *client* can
/// recover from automatically: it re-snapshots against an older commit that the
/// peer does have. Every other failure is terminal.
#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error("peer does not have commit {0}")]
    MissingBase(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Prepare a worktree at `base` with `patch` applied, ready for the recipe.
///
/// Returns the worktree path. Progress strings are reported through `stage` so
/// the requesting side can see what its peer is doing.
pub async fn prepare_worktree<F>(
    repo: &Path,
    base: &str,
    patch: &[u8],
    remote: &str,
    label: &str,
    mut stage: F,
) -> std::result::Result<PathBuf, PrepareError>
where
    F: FnMut(String),
{
    // The recoverable check happens out here so the rest of the routine can use
    // ordinary anyhow error propagation.
    if !has_commit(repo, base).await {
        stage(format!("fetching {base:.12} from {remote}"));
        // Untagged, unreferenced commits are not fetchable, so this only helps
        // when the client pushed a branch containing the base.
        let _ = run(repo, &["fetch", "--quiet", remote]).await;
        if !has_commit(repo, base).await {
            return Err(PrepareError::MissingBase(base.to_string()));
        }
    }
    prepare_checked(repo, base, patch, label, stage)
        .await
        .map_err(PrepareError::Other)
}

/// Everything after the base commit is known to be present.
async fn prepare_checked<F>(
    repo: &Path,
    base: &str,
    patch: &[u8],
    label: &str,
    mut stage: F,
) -> Result<PathBuf>
where
    F: FnMut(String),
{
    let wt = repo.join(".sw").join("wt").join(label);

    if wt.join(".git").exists() {
        stage("reusing worktree".into());
    } else {
        stage("creating worktree".into());
        tokio::fs::create_dir_all(wt.parent().unwrap()).await?;
        // `worktree add` refuses a non-empty directory, so clear any debris from
        // an interrupted previous run.
        if wt.exists() {
            let _ = tokio::fs::remove_dir_all(&wt).await;
        }
        let out = run(
            repo,
            &[
                "worktree",
                "add",
                "--detach",
                wt.to_str().context("non-utf8 worktree path")?,
                base,
            ],
        )
        .await?;
        if !out.status.success() {
            bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }

    stage(format!("checkout {base:.12}"));
    git(&wt, &["checkout", "--detach", "--force", base]).await?;

    // Note the absence of -x: ignored files (crucially `target/`) survive, so
    // the build cache is preserved, while leftovers from the previous patch are
    // removed. Patches never contain ignored files, so this is consistent.
    git(&wt, &["clean", "-fd", "--quiet"]).await?;

    if !patch.is_empty() {
        stage("applying patch".into());
        let raw = zstd::decode_all(patch).context("patch is not valid zstd")?;
        apply_patch(&wt, &raw).await?;
    }

    Ok(wt)
}

async fn apply_patch(wt: &Path, raw: &[u8]) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut child = Command::new("git")
        .args(["apply", "--binary", "--whitespace=nowarn", "-"])
        .current_dir(wt)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn git apply")?;

    let mut stdin = child.stdin.take().context("no stdin on git apply")?;
    stdin.write_all(raw).await?;
    stdin.shutdown().await?;
    drop(stdin);

    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!(
            "git apply failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// `git worktree` records absolute paths, so a repo moved on disk leaves stale
/// administrative entries behind. Cheap to prune, confusing when skipped.
pub async fn prune_worktrees(repo: &Path) {
    let _ = run(repo, &["worktree", "prune"]).await;
}

pub fn state_dir(repo: &Path) -> PathBuf {
    repo.join(crate::config::STATE_DIR)
}

/// Ensure `.sw/` is ignored. The worktree and scratch index live there, and
/// committing either would be actively harmful.
pub async fn ensure_ignored(repo: &Path) -> Result<()> {
    let path = repo.join(".gitignore");
    let existing = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    if existing
        .lines()
        .any(|l| l.trim() == ".sw/" || l.trim() == ".sw")
    {
        return Ok(());
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(".sw/\n");
    tokio::fs::write(&path, next).await?;
    Ok(())
}
