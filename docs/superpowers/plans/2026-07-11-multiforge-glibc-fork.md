# Multi-forge + glibc-independent Fork Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make herdr-reviewr run on any glibc (musl static Linux builds) and give the PR tab full parity on self-hosted GitLab (MRs) and Bitbucket Data Center (PRs), keeping GitHub.

**Architecture:** `src/forge.rs` splits into `src/forge/` — a forge-neutral core (`mod.rs`: snapshot model, candidate resolution policy, degraded views) plus three backends (`github.rs` moved unchanged, `gitlab.rs` via `glab api`, `bitbucket.rs` via `curl` against the DC REST API). Origin classification in `git.rs` becomes a host→forge mapping driven by two new config keys. The binary keeps doing zero networking itself — all forge access is subprocesses.

**Tech Stack:** Rust 2024 (toolchain pinned by `rust-toolchain.toml`), serde_json, no new crate dependencies. Subprocess tools: `gh`, `glab`, `curl`, `git credential`.

## Global Constraints

- **Spec:** `docs/superpowers/specs/2026-07-11-multiforge-glibc-fork-design.md` — read it first.
- No new `[dependencies]` in `Cargo.toml`. No networking in the binary. `unsafe_code = "forbid"` stays.
- Clippy pedantic is on and CI treats warnings as errors: run `just ci` (fmt-check + clippy `-D warnings` + tests + release build) before every commit. Plain `cargo test` is NOT enough.
- The existing GitHub behavior must not change: every pre-existing test passes unmodified (module paths in `use` lines may change, assertions may not).
- Plugin identity: `dcieslak19973.reviewr`; repo `dcieslak19973/herdr-reviewr`.
- Comment style: this codebase writes dense doc comments explaining *contracts*, not narration. Match it.
- All work happens on the `multiforge-fork` branch of `D:\git\herdr-reviewr`.
- Commit messages end with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

---

### Task 1: musl static Linux builds

**Files:**
- Modify: `.github/workflows/release.yml` (matrix targets)
- Modify: `.github/workflows/ci.yml` (add a staticness job)
- Modify: `herdr/install.sh:26-33` (target triple mapping)

**Interfaces:**
- Produces: release assets named `herdr-reviewr-x86_64-unknown-linux-musl.tar.gz` / `…-aarch64-unknown-linux-musl.tar.gz`, which Task 2's `install.sh` fetches.

- [ ] **Step 1: Switch release matrix to musl**

In `.github/workflows/release.yml`, replace the two Linux matrix rows:

```yaml
          - { target: x86_64-unknown-linux-musl, os: ubuntu-latest }
          - { target: aarch64-unknown-linux-musl, os: ubuntu-latest }
```

(`taiki-e/upload-rust-binary-action` auto-uses `cross` for non-host targets; musl x86_64 also needs it unless `musl-tools` is installed — keep it simple and let the action decide, it handles both.)

- [ ] **Step 2: Map Linux to musl in install.sh**

In `herdr/install.sh`, change the case arms:

```bash
  Linux-aarch64 | Linux-arm64) target="aarch64-unknown-linux-musl" ;;
  Linux-x86_64)              target="x86_64-unknown-linux-musl" ;;
```

- [ ] **Step 3: Add a CI staticness assertion**

Append a job to `.github/workflows/ci.yml`:

```yaml
  static-linux:
    name: musl binary is static
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v5
      - run: rustup target add x86_64-unknown-linux-musl && sudo apt-get update && sudo apt-get install -y musl-tools
      - run: cargo build --release --target x86_64-unknown-linux-musl
      - name: Assert no dynamic glibc linkage
        run: |
          out="$(ldd target/x86_64-unknown-linux-musl/release/herdr-reviewr 2>&1 || true)"
          echo "$out"
          echo "$out" | grep -q "not a dynamic executable\|statically linked" \
            || { echo "binary is dynamically linked"; exit 1; }
```

- [ ] **Step 4: Verify the crate still compiles and tests pass**

Run: `just ci`
Expected: all green (no Rust code changed; this catches YAML-adjacent mistakes like touched paths).

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/release.yml .github/workflows/ci.yml herdr/install.sh
git commit -m "build: static musl Linux binaries for glibc independence"
```

---

### Task 2: Fork identity — plugin id and repo

**Files:**
- Modify: `herdr-plugin.toml:1`, `herdr/install.sh:13`, `herdr/sidebar.sh:158`, `README.md` (all `persiyanov` occurrences), `Cargo.toml` (`repository`)

- [ ] **Step 1: Rename**

- `herdr-plugin.toml`: `id = "dcieslak19973.reviewr"`
- `herdr/install.sh`: `REPO="dcieslak19973/herdr-reviewr"`
- `herdr/sidebar.sh`: fallback `${HERDR_PLUGIN_ID:-dcieslak19973.reviewr}`
- `README.md`: replace every `persiyanov/herdr-reviewr` → `dcieslak19973/herdr-reviewr` and `persiyanov.reviewr` → `dcieslak19973.reviewr` (12 occurrences; verify with `grep -rn persiyanov README.md` → empty). Add a short "Fork" note at the top of the README naming the upstream project and this fork's two changes (musl builds; GitLab + Bitbucket DC support).
- `Cargo.toml`: `repository = "https://github.com/dcieslak19973/herdr-reviewr"`

- [ ] **Step 2: Verify no stragglers, tests pass**

Run: `grep -rn "persiyanov" --include="*.toml" --include="*.sh" --include="*.md" . | grep -v docs/superpowers | grep -v CHANGELOG`
Expected: empty (CHANGELOG and specs keep history as-is).
Run: `just ci` — green.

- [ ] **Step 3: Commit**

```bash
git add herdr-plugin.toml herdr/install.sh herdr/sidebar.sh README.md Cargo.toml
git commit -m "chore: rebrand fork as dcieslak19973.reviewr"
```

---

### Task 3: Config keys `gitlab_host` and `bitbucket_host`

**Files:**
- Modify: `src/config.rs` (struct `PluginConfig`, `KNOWN_KEYS` at line ~64, parse block at ~296, `to_json` at ~152, tests)

**Interfaces:**
- Produces: `PluginConfig::gitlab_host() -> Option<&str>` and `PluginConfig::bitbucket_host() -> Option<&str>`, exactly mirroring `github_host()` (lowercased bare hostname, rejects URLs/paths). Task 4 consumes these.

- [ ] **Step 1: Write failing tests** (in `src/config.rs` `#[cfg(test)]`, next to the existing `github_host` tests at ~415-470)

```rust
    #[test]
    fn gitlab_and_bitbucket_hosts_parse_and_lowercase() {
        let dir = tempdir_with_config(concat!(
            "gitlab_host = \"GitLab.Corp.COM\"\n",
            "bitbucket_host = \"bitbucket.corp.com\"\n",
        ));
        let config = plugin_config_in(dir.path()).unwrap();
        assert_eq!(config.gitlab_host(), Some("gitlab.corp.com"));
        assert_eq!(config.bitbucket_host(), Some("bitbucket.corp.com"));
    }

    #[test]
    fn forge_hosts_default_to_none() {
        let dir = tempdir_with_config("");
        let config = plugin_config_in(dir.path()).unwrap();
        assert_eq!(config.gitlab_host(), None);
        assert_eq!(config.bitbucket_host(), None);
    }

    #[test]
    fn forge_hosts_reject_urls() {
        for bad in
            ["gitlab_host = \"https://gitlab.corp.com\"\n", "bitbucket_host = \"host/path\"\n"]
        {
            let dir = tempdir_with_config(bad);
            assert!(plugin_config_in(dir.path()).is_err(), "{bad} should be rejected");
        }
    }
```

Adapt the fixture helper name to whatever the existing `github_host` tests use (read them first — there is an existing pattern for writing a temp `config.toml`; reuse it verbatim).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config`
Expected: FAIL — `gitlab_host` method not found.

- [ ] **Step 3: Implement**

Mirror `github_host` exactly: two `Option<String>` fields (default `None`), two accessors, two entries in `KNOWN_KEYS` (`"gitlab_host"`, `"bitbucket_host"`), two parse blocks reusing the same `string_value` + bare-hostname validation the `github_host` block uses (copy its shape; the error message names the right key), and two lines in `to_json`.

- [ ] **Step 4: Run tests**

Run: `just ci`
Expected: PASS, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): gitlab_host and bitbucket_host keys"
```

---

### Task 4: Forge-aware origin classification

**Files:**
- Modify: `src/git.rs` (`RepoTarget` ~line 69, `classify_remote` ~line 93, `origin_identity` ~line 259, `pr_local` ~line 228, tests from ~line 891)
- Modify: callers of `pr_local` — `src/forge.rs:292` (`fetch_input`) passes the whole config instead of just `github_host`.

**Interfaces:**
- Produces:
  ```rust
  /// Which forge a classified origin belongs to.
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub enum Forge { GitHub, GitLab, Bitbucket }

  pub struct RepoTarget {
      pub forge: Forge,
      pub host: String,
      /// GitHub/GitLab: owner (GitLab may nest: "group/subgroup"). Bitbucket DC: project key.
      pub owner: String,
      /// Repository name (GitHub/GitLab) or repo slug (Bitbucket DC).
      pub name: String,
  }

  pub struct ForgeHosts<'a> {
      pub github: Option<&'a str>,
      pub gitlab: Option<&'a str>,
      pub bitbucket: Option<&'a str>,
  }

  fn classify_remote(url: &str, hosts: &ForgeHosts<'_>) -> OriginIdentity;
  pub fn pr_local(repo: &Path, base: Option<&str>, base_branches: &[String], hosts: &ForgeHosts<'_>) -> Result<PrLocal, GitFail>;
  ```
- Consumes: `PluginConfig::{github_host, gitlab_host, bitbucket_host}` from Task 3.

**Classification rules** (spec §Config): case-insensitive host match with the existing SSH-alias rule per host. Built-ins: `github.com` → GitHub, `gitlab.com` → GitLab. Configured: `github_host` → GitHub, `gitlab_host` → GitLab, `bitbucket_host` → Bitbucket. Anything else → `Unsupported(host)`.

**Path rules:**
- GitHub: exactly `owner/name` (unchanged).
- GitLab: **two or more** segments; `name` = last, `owner` = the rest joined by `/` (nested groups). One segment → `Malformed`.
- Bitbucket DC: HTTPS/hosted URLs must be `scm/<KEY>/<slug>` (strip the leading `scm`); SSH URLs are `<KEY>/<slug>`. Anything else → `Malformed`. Key/slug map to `owner`/`name`.

- [ ] **Step 1: Write failing table tests** (extend the existing `classify_remote` test block in `src/git.rs`; keep every existing assertion, updating only the call signature)

```rust
    fn hosts(gh: Option<&str>, gl: Option<&str>, bb: Option<&str>) -> ForgeHosts<'static> { /* helper via Box::leak or inline construction per test — simplest: build ForgeHosts inline */ }

    #[test]
    fn classifies_gitlab_hosts() {
        let h = ForgeHosts { github: None, gitlab: Some("gitlab.corp.com"), bitbucket: None };
        assert_eq!(
            classify_remote("git@gitlab.com:group/repo.git", &h),
            OriginIdentity::Repository(RepoTarget {
                forge: Forge::GitLab, host: "gitlab.com".into(),
                owner: "group".into(), name: "repo".into(),
            })
        );
        // nested groups keep their full path as owner
        assert_eq!(
            classify_remote("https://gitlab.corp.com/group/sub/repo.git", &h),
            OriginIdentity::Repository(RepoTarget {
                forge: Forge::GitLab, host: "gitlab.corp.com".into(),
                owner: "group/sub".into(), name: "repo".into(),
            })
        );
        // one segment is malformed, not owner-less
        assert_eq!(
            classify_remote("https://gitlab.com/repo.git", &h),
            OriginIdentity::Malformed("gitlab.com".into())
        );
    }

    #[test]
    fn classifies_bitbucket_dc_hosts() {
        let h = ForgeHosts { github: None, gitlab: None, bitbucket: Some("bitbucket.corp.com") };
        // HTTPS clone URLs carry /scm/
        assert_eq!(
            classify_remote("https://bitbucket.corp.com/scm/PROJ/repo.git", &h),
            OriginIdentity::Repository(RepoTarget {
                forge: Forge::Bitbucket, host: "bitbucket.corp.com".into(),
                owner: "PROJ".into(), name: "repo".into(),
            })
        );
        // SSH clone URLs do not
        assert_eq!(
            classify_remote("ssh://git@bitbucket.corp.com:7999/PROJ/repo.git", &h),
            OriginIdentity::Repository(RepoTarget {
                forge: Forge::Bitbucket, host: "bitbucket.corp.com".into(),
                owner: "PROJ".into(), name: "repo".into(),
            })
        );
        // an https path without the /scm/ prefix is malformed for Bitbucket
        assert_eq!(
            classify_remote("https://bitbucket.corp.com/PROJ/repo.git", &h),
            OriginIdentity::Malformed("bitbucket.corp.com".into())
        );
        // bitbucket.org (Cloud) stays unsupported — different API family
        assert_eq!(
            classify_remote("git@bitbucket.org:owner/repo.git", &h),
            OriginIdentity::Unsupported("bitbucket.org".into())
        );
    }
```

NOTE the SSH port: the existing code rejects hosted-transport URLs with ports but allows SSH ports (`ssh://git@github.com:22/...` test at line ~920). Bitbucket DC SSH uses port 7999 conventionally — the existing SSH-port allowance covers it; keep that behavior.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib git`
Expected: FAIL — `Forge`/`ForgeHosts` unresolved, `RepoTarget` missing field.

- [ ] **Step 3: Implement**

In `classify_remote`, replace the two-way canonical-host match with a lookup that yields `(canonical_host, Forge)`:

```rust
    let matches_host = |canonical: &str| {
        host_lower == canonical
            || (transport == RemoteTransport::Ssh && is_alias(&host_lower, canonical))
    };
    let hit: Option<(&str, Forge)> = [
        (Some("github.com"), Forge::GitHub),
        (hosts.github, Forge::GitHub),
        (Some("gitlab.com"), Forge::GitLab),
        (hosts.gitlab, Forge::GitLab),
        (hosts.bitbucket, Forge::Bitbucket),
    ]
    .into_iter()
    .filter_map(|(h, f)| h.map(|h| (h, f)))
    .find(|(h, _)| matches_host(h));
```

Then split the path per forge as specified above (GitHub arm identical to today; GitLab `rsplit_once('/')`; Bitbucket strips a leading `scm/` for non-SSH transports and requires exactly two remaining segments). Update `origin_identity` and `pr_local` to take `&ForgeHosts<'_>`; update `fetch_input` in `src/forge.rs` to build it from the config:

```rust
    let hosts = crate::git::ForgeHosts {
        github: config.github_host(),
        gitlab: config.gitlab_host(),
        bitbucket: config.bitbucket_host(),
    };
```

Every existing GitHub test gets `&ForgeHosts { github: <old enterprise arg>, gitlab: None, bitbucket: None }` — assertions unchanged except `RepoTarget` gaining `forge: Forge::GitHub`.

- [ ] **Step 4: Run tests**

Run: `just ci`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/git.rs src/forge.rs
git commit -m "feat(git): classify GitLab and Bitbucket DC origins"
```

---

### Task 5: Split forge.rs into forge/ with a per-forge dispatch (no behavior change for GitHub)

**Files:**
- Create: `src/forge/mod.rs` (from `src/forge.rs`: everything except the gh/GraphQL machinery)
- Create: `src/forge/github.rs` (the gh/GraphQL machinery, moved)
- Create: `src/forge/proc.rs` (the generalized subprocess runner)
- Delete: `src/forge.rs`
- Modify: `src/ui.rs:1569-1595` (degraded-state copy), `src/app.rs` (only if `PrView` variant names require it)

**Interfaces:**
- Produces (in `forge/mod.rs`, consumed by Tasks 6-8):
  ```rust
  /// A classified backend failure, mapped to a PrView degraded state by the core.
  #[derive(Debug, PartialEq, Eq)]
  pub(crate) enum ForgeError {
      /// The forge's CLI tool is not on PATH ("gh", "glab", "curl").
      NoCli(&'static str),
      /// The tool is present but not authenticated for this host.
      NotAuthed { forge: crate::git::Forge, host: String },
      /// Bitbucket only: no token in BITBUCKET_TOKEN or git-credential for this host.
      NoToken(String),
      Other(String),
  }

  /// Everything a backend needs for one fetch. Candidates are already derived and capped.
  pub(crate) struct FetchTarget<'a> {
      pub repo: &'a Path,
      pub host: &'a str,
      pub owner: &'a str,
      pub name: &'a str,
      pub cancelled: &'a AtomicBool,
  }

  /// Backend contract: resolve + read one PR/MR for the input, or a typed error. The core
  /// handles Missing/Unsupported/Malformed origins and empty candidates before dispatch.
  /// Returns the snapshot-or-empty PrView variants only (Pr / NoPr / Ambiguous).
  pub(crate) fn backend_fetch(
      forge: crate::git::Forge,
      target: &FetchTarget<'_>,
      input: &PrFetchInput,
  ) -> Result<PrView, ForgeError>;   // matches on Forge → github::fetch / gitlab::fetch / bitbucket::fetch
  ```
- Produces (in `forge/proc.rs`):
  ```rust
  pub(crate) enum RunFail { NotFound, Failed { stderr: String }, Cancelled, Io(String) }
  /// Spawn `tool` with `args` in `repo`, optionally writing `stdin` to the child, draining
  /// both pipes while polling `cancelled` (the existing gh() loop, generalized).
  pub(crate) fn run_tool(
      tool: &str, repo: &Path, args: &[&str], stdin: Option<&str>, cancelled: &AtomicBool,
  ) -> Result<String, RunFail>;
  ```
- `PrView` changes (public, rendered by ui.rs):
  - `NoGh` → `NoCli(&'static str)` (carries the tool name)
  - `NotAuthed(String)` → `NotAuthed { forge: crate::git::Forge, host: String }`
  - `NeedsGitHubOrigin` → `NeedsSupportedOrigin`
  - new: `NoToken(String)` (Bitbucket host needing a token)
  - `retry_remedy()` per variant:
    - `NoCli(tool)` → `format!("{tool} not found — install `{tool}`, then press r")`
    - `NotAuthed{forge: GitHub, host}` → `gh auth login --hostname {host}` (existing copy)
    - `NotAuthed{forge: GitLab, host}` → `glab auth login --hostname {host}`
    - `NotAuthed{forge: Bitbucket, host}` → `check BITBUCKET_TOKEN for {host}`
    - `NoToken(host)` → `format!("no token for {host} — set BITBUCKET_TOKEN or add it to git credentials, then press r")`
    - `Error(message)` → `format!("forge unavailable — {message}; press r to retry now")`

- [ ] **Step 1: Mechanical move**

`git mv src/forge.rs src/forge/mod.rs`, then extract into `src/forge/github.rs`: `gh()` (rewritten as a thin wrapper over `proc::run_tool("gh", …)` mapping `RunFail::NotFound → GhError::NoGh` and classifying stderr as today), `classify_failure`, `GhError`, `FetchTarget` usage, `resolve_candidates`, `Pick`, `select_open`, `select_historical`, `pr_detail`, `graphql`, `graphql_args`, `build_resolve_query`, `parse_resolve`, `build_snapshot`, `parse_state`, `derive_merge`, `normalize_checks`, `check_status`, `merge_comments`, `prose_comment`, `dedup_bot_prose`, `is_bot`, `OPEN`, `HISTORICAL` — plus their unit tests. Public entry: `pub(crate) fn fetch(target: &FetchTarget<'_>, input: &PrFetchInput) -> Result<PrView, ForgeError>` (the body of today's `fetch_inner` from the `resolve_candidates` call down, with `GhError` mapped into `ForgeError`).

`select_open`, `select_historical`, `Pick`, and `derive_sync` are forge-neutral policy — keep them in `mod.rs` and have backends call them (`pub(crate)` within the module tree). `Sync` derivation (`git::ahead_behind_oids` + `derive_sync`) also stays in `mod.rs`: after a backend returns the raw snapshot with the PR head OID, the core computes `sync`. Simplest split that preserves behavior: backends return the finished `PrView` (as today) and call `super::derive_sync` themselves — choose this; it keeps the diff mechanical.

- [ ] **Step 2: Create `src/forge/proc.rs`**

Generalize today's `gh()` body (lines 191-244 of the old file): parameterize the program name, add optional stdin:

```rust
    let mut cmd = Command::new(tool);
    cmd.current_dir(repo).args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(RunFail::NotFound),
        Err(e) => return Err(RunFail::Io(e.to_string())),
    };
    if let Some(input) = stdin {
        use std::io::Write;
        let mut pipe = child.stdin.take().expect("piped stdin");
        let _ = pipe.write_all(input.as_bytes());
        drop(pipe); // close so the child sees EOF
    }
    // …existing drain/poll/kill loop verbatim, returning RunFail::Cancelled / ::Failed…
```

- [ ] **Step 3: Rework the dispatch in `mod.rs`**

`fetch_inner` keeps its origin/candidate pre-checks (`NeedsSupportedOrigin`, `UnsupportedHost`, `Malformed`, detached-HEAD empty state), then:

```rust
    let target = FetchTarget { repo, host: &t.host, owner: &t.owner, name: &t.name, cancelled };
    match backend_fetch(t.forge, &target, input) {
        Ok(view) => Ok(view),
        Err(e) => Ok(e.into()),   // impl From<ForgeError> for PrView
    }
```

with `gitlab`/`bitbucket` arms returning `Err(ForgeError::Other("not yet implemented".into()))` until Tasks 6/7 (they are unreachable anyway until classification admits those forges — it already does after Task 4, so the copy matters: make it `"<forge> support is not built yet"`). Wire the new `PrView` variants through `ui.rs` (lines ~1569-1595: the empty-state copy becomes forge-neutral — `"the PR tab needs a supported forge origin (GitHub, GitLab, or Bitbucket Data Center)"`; `UnsupportedHost` copy now points at all three config keys).

- [ ] **Step 4: Run the full suite**

Run: `just ci`
Expected: PASS — every pre-move test passes with only `use` paths updated. `git log --stat` shows `src/forge.rs` → `src/forge/…` as renames where possible.

- [ ] **Step 5: Commit**

```bash
git add -A src/forge* src/ui.rs src/app.rs
git commit -m "refactor(forge): split into neutral core + github backend behind forge dispatch"
```

---

### Task 6: GitLab backend (`glab`)

**Files:**
- Create: `src/forge/gitlab.rs`
- Modify: `src/forge/mod.rs` (dispatch arm)
- Test fixtures: inline `serde_json::json!` literals (follow `github.rs`'s test style)

**Interfaces:**
- Consumes: `proc::run_tool`, `ForgeError`, `FetchTarget`, `select_open`/`select_historical`/`Pick`, `derive_sync`, snapshot types from `mod.rs`.
- Produces: `pub(crate) fn fetch(target: &FetchTarget<'_>, input: &PrFetchInput) -> Result<PrView, ForgeError>`

**API calls** (all via `glab api --hostname <host> <path>`; project path = percent-encoded `owner/name` with `/`→`%2F`):
1. Resolution, per candidate (≤8): `projects/<proj>/merge_requests?source_branch=<enc(branch)>&state=opened&per_page=100` → array of `{iid, sha}`.
2. Historical fallback, per candidate: `projects/<proj>/merge_requests?source_branch=<enc(branch)>&order_by=created_at&sort=desc&per_page=20&state=all` → newest entry whose `state` is `merged`/`closed`.
3. Detail: `projects/<proj>/merge_requests/<iid>` → identity, `detailed_merge_status`, `draft`, `sha`, `source_branch`, `target_branch`, `source_project_id`/`target_project_id`, `web_url`, `title`, `state`, `head_pipeline.id`.
4. Checks: `projects/<proj>/pipelines/<head_pipeline.id>/jobs?per_page=100` (skip when no pipeline) → `[{name, status}]`.
5. Comments: `projects/<proj>/merge_requests/<iid>/discussions?per_page=100` → notes.

**Mappings** (each is a pure function with a fixture test):

```rust
/// opened → Open, merged → Merged, closed → Closed (default Open, like GitHub's parse_state).
fn parse_state(s: &str) -> PrState;

/// detailed_merge_status: "conflict"/"broken_status" → Conflicting;
/// "blocked_status"/"discussions_not_resolved"/"policies_denied"/"draft_status" → Blocked
/// (draft_status only when !draft — a draft MR's own flag already shows);
/// everything else ("mergeable", "checking", "unchecked", "ci_must_pass", …) → Clean.
fn derive_merge(detailed: Option<&str>, is_draft: bool) -> Merge;

/// Job status: success → Success; failed → Failure; running → Running;
/// created/pending/waiting_for_resource/scheduled → Pending;
/// skipped/manual/canceled → Skipped.
fn job_status(s: &str) -> CheckStatus;

/// Discussions → Comments. A note with `system:true` is dropped. A note whose discussion
/// carries a `position` (inline) → Finding with anchor "new_path:new_line" (fall back to
/// old_path/old_line when new_* is null → also is_outdated=true), resolved from `resolved`,
/// reply_count = notes.len()-1 on its discussion, body/author from the FIRST note.
/// A non-positioned, non-system note → Comment (individual_note discussions have one note).
/// author_is_bot: username contains "_bot" (GitLab service-account convention) or the
/// author object carries `"bot": true` when present.
fn map_discussions(discussions: &Value) -> Vec<Comment>;

/// Snapshot assembly from detail + jobs + comments + sync. truncated when any surface
/// returned exactly per_page rows (REST has no pageInfo; a full page means "maybe more").
fn build_snapshot(detail: &Value, jobs: &[Check], comments: Vec<Comment>, sync: Sync, truncated: bool) -> PrSnapshot;
```

`number` = `iid`; `head_is_fork` = `source_project_id != target_project_id` (both present); `created_at` for notes is GitLab's ISO-8601 with fractional seconds (`2026-07-11T10:00:00.000Z`) — `parse_iso` in `mod.rs` accepts ≥20 bytes with fixed prefix positions, and fractional suffixes sort correctly lexically only against same-precision strings; GitLab is uniformly fractional so sorting is consistent. Verify `relative_age`'s `parse_iso` still parses the first 19 chars (it does — it ignores the tail).

**Error classification:** `RunFail::NotFound` → `ForgeError::NoCli("glab")`; stderr containing (lowercased) `"not authenticated"`, `"glab auth login"`, `"401"` → `NotAuthed { forge: Forge::GitLab, host }`; else `Other`.

**Percent-encoding** (no new deps — write it):

```rust
/// Percent-encode for a URL path segment or query value: unreserved chars pass, all else %XX.
fn enc(s: &str) -> String {
    s.bytes()
        .flat_map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                vec![b as char]
            }
            _ => format!("%{b:02X}").chars().collect(),
        })
        .collect()
}
```

- [ ] **Step 1: Write failing fixture tests** — one per mapping function, real GitLab REST shapes. Minimum set:
  - `derive_merge`: conflict→Conflicting, blocked_status→Blocked, draft_status with draft=true→Clean, mergeable/checking→Clean.
  - `job_status`: all arms above.
  - `map_discussions`: a system note is dropped; an inline resolved discussion with 3 notes → Finding{is_resolved, reply_count:2, anchor:"src/a.rs:12"}; a plain note → Comment; a `project_42_bot_abc` author → author_is_bot.
  - `build_snapshot`: iid→number, draft, cross-project fork flag, state mapping.
  - `enc`: `"group/sub"` → `"group%2Fsub"`, `"feat/x#1"` → `"feat%2Fx%231"`.
- [ ] **Step 2: Run** `cargo test --lib forge::gitlab` — Expected: FAIL (module doesn't exist → compile error first; add skeleton with `todo!()` bodies to get red tests, then implement).
- [ ] **Step 3: Implement** the mappings and the `fetch` orchestration (resolution loop over candidates reusing `select_open`/`select_historical` — shape the per-candidate results as `Vec<Vec<(u64, String)>>` with `(iid, sha)` open / `(iid, created_at)` historical, exactly the structure GitHub's `Pick` policy consumes).
- [ ] **Step 4: Run** `just ci` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(forge): GitLab MR backend via glab"`

---

### Task 7: Bitbucket Data Center backend (`curl`)

**Files:**
- Create: `src/forge/bitbucket.rs`
- Modify: `src/forge/mod.rs` (dispatch arm)

**Interfaces:**
- Consumes: same core items as Task 6.
- Produces: `pub(crate) fn fetch(target: &FetchTarget<'_>, input: &PrFetchInput) -> Result<PrView, ForgeError>`

**Auth resolution** (order):
1. `std::env::var("BITBUCKET_TOKEN")` — non-empty wins.
2. `git credential fill` via `proc::run_tool("git", repo, &["credential", "fill"], Some("protocol=https\nhost=<host>\n\n"), cancelled)` — parse the `password=` line.
3. Neither → `ForgeError::NoToken(host)`.

**curl invocation** — the token never appears in argv; it rides a curl config on stdin:

```rust
/// GET `url` with the bearer token via a stdin curl config, so the token is invisible to
/// `ps`/`/proc`. `--fail` maps HTTP errors to exit 22 with the status line on stderr.
fn curl_get(target: &FetchTarget<'_>, token: &str, url: &str) -> Result<Value, ForgeError> {
    let config = format!("header = \"Authorization: Bearer {token}\"\n");
    let out = super::proc::run_tool(
        "curl",
        target.repo,
        &["--silent", "--show-error", "--fail", "--config", "-", url],
        Some(&config),
        target.cancelled,
    )
    .map_err(|f| classify(f, target.host))?;
    serde_json::from_str(&out).map_err(|e| ForgeError::Other(e.to_string()))
}
/// NotFound → NoCli("curl"); stderr containing "401"/"403" → NotAuthed{Bitbucket, host}; else Other.
fn classify(f: super::proc::RunFail, host: &str) -> ForgeError;
```

**API calls** (base `https://<host>/rest/api/latest/projects/<KEY>/repos/<slug>`):
1. Resolution, per candidate: `<base>/pull-requests?state=OPEN&direction=OUTGOING&at=refs/heads/<enc(branch)>&limit=100` → `values[]` of `{id, fromRef.latestCommit}`.
2. Historical: same with `state=ALL&limit=20`, newest (`createdDate` max) whose `state != "OPEN"`.
3. Detail: `<base>/pull-requests/<id>` → `title`, `state` (OPEN/MERGED/DECLINED), `draft` (bool; absent on older DC → false), `fromRef{displayId, latestCommit, repository{slug, project{key}}}`, `toRef.displayId`, `links.self[0].href` (URL).
4. Merge state (only when state is OPEN): `<base>/pull-requests/<id>/merge` → `{canMerge, conflicted, vetoes[]}`: `conflicted` → Conflicting; `!canMerge && !vetoes.is_empty()` → Blocked; else Clean.
5. Checks: `https://<host>/rest/build-status/latest/commits/<fromRef.latestCommit>?limit=100` → `values[]{name|key, state}`: SUCCESSFUL→Success, FAILED→Failure, INPROGRESS→Running, CANCELLED→Skipped, UNKNOWN/other→Pending.
6. Comments: `<base>/pull-requests/<id>/activities?limit=100` → `values[]` where `action=="COMMENTED"`.

**Mappings** (pure, fixture-tested):

```rust
/// OPEN → Open, MERGED → Merged, DECLINED → Closed.
fn parse_state(s: &str) -> PrState;

/// Activities → Comments. An activity with a commentAnchor{path, line} → Finding
/// (anchor "path:line"; line absent → path only; anchor.orphaned==true → is_outdated).
/// Resolution: comment.state=="RESOLVED" or comment.threadResolved==true → is_resolved.
/// reply_count = total nested comment.comments[] entries (recursive count).
/// Without an anchor → Comment. Bitbucket has no review-body surface → no Review kind.
/// author = comment.author.name; author_is_bot = author.user type=="SERVICE" if present,
/// else name ends_with("-bot")/"_bot".
fn map_activities(activities: &Value) -> Vec<Comment>;

/// createdDate is epoch milliseconds → ISO-8601 "YYYY-MM-DDTHH:MM:SSZ" so sorting and
/// relative_age work unchanged. Inverse of mod.rs's parse_iso civil-date algorithm.
fn epoch_ms_to_iso(ms: i64) -> String;

fn build_snapshot(detail: &Value, merge: Merge, checks: Vec<Check>, comments: Vec<Comment>, sync: Sync, truncated: bool) -> PrSnapshot;
```

`number` = `id`; `head_ref` = `fromRef.displayId`; `head_is_fork` = `fromRef.repository.{project.key, slug}` ≠ target's `(owner, name)` (case-insensitive on the key); `truncated` = any listing returned `isLastPage == false`.

For `epoch_ms_to_iso`, days-from-civil inverts with Howard Hinnant's `civil_from_days`; write it and unit-test round-trips through `parse_iso`:

```rust
    #[test]
    fn epoch_ms_round_trips_through_parse_iso() {
        for ms in [0i64, 1_752_192_000_000, 4_102_444_799_000] {
            let iso = epoch_ms_to_iso(ms);
            assert_eq!(super::super::parse_iso(&iso), Some(ms / 1000));
        }
    }
```

(`parse_iso` is private to `mod.rs` today — make it `pub(crate)` within the forge module tree.)

- [ ] **Step 1: Write failing fixture tests** — `parse_state`, `map_activities` (anchored finding with 2 nested replies; orphaned anchor → outdated; RESOLVED state; general comment; SERVICE user → bot), `epoch_ms_to_iso` round-trip, `build_snapshot` (fork detection by differing project key, draft default false when field absent), merge folding (conflicted / vetoed / clean), check-state mapping, and token resolution order (env beats credential helper — test via a `fn token_from(env: Option<String>, credential_password: Option<String>) -> Result<String, ForgeError>` pure function; the env/subprocess reads happen in one thin untested wrapper).
- [ ] **Step 2: Run** `cargo test --lib forge::bitbucket` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `just ci` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(forge): Bitbucket Data Center backend via curl"`

---

### Task 8: Forge-aware UI copy

**Files:**
- Modify: `src/ui.rs:396` (tab label), `src/ui.rs:1513`, `src/app.rs` (expose the forge), `src/ui.rs` empty/degraded states from Task 5 (verify copy end-to-end)

**Interfaces:**
- Consumes: `crate::git::Forge` (Task 4), `App.pr` fetch input.
- Produces: `App::forge_noun(&self) -> &'static str` — `"MR"` when the current fetch input's origin classified as GitLab, `"PR"` otherwise (including unclassified).

- [ ] **Step 1: Write the failing test** (in `src/app.rs` tests, following the existing App test setup pattern — find one that constructs an App with a repo fixture):

```rust
    #[test]
    fn forge_noun_says_mr_for_gitlab_origins() {
        // Arrange an App whose PrFetchInput origin is a GitLab RepoTarget (construct the
        // input directly — no subprocess), then assert the noun.
        assert_eq!(app_with_origin(Forge::GitLab).forge_noun(), "MR");
        assert_eq!(app_with_origin(Forge::GitHub).forge_noun(), "PR");
        assert_eq!(app_with_origin(Forge::Bitbucket).forge_noun(), "PR");
    }
```

The App stores the latest `PrFetchInput` (find the field `fetch_input`/similar near `app.rs:208` — read how `apply_pr` and the refresh path keep it; add storage if none exists, set wherever the input is derived).

- [ ] **Step 2: Run** `cargo test --lib app::` — Expected: FAIL.
- [ ] **Step 3: Implement** `forge_noun`, then use it: `ui.rs:396` tab title becomes dynamic (`format!("3 {}", app.forge_noun())` — this array is const today; make the third label computed where it is rendered), `ui.rs:1513`'s `None => "PR".to_string()` → `app.forge_noun().to_string()`. Grep `src/ui.rs` for remaining literal `"PR"` user-facing strings and swap those that name the artifact (not the tab-key hints) for the noun.
- [ ] **Step 4: Run** `just ci` — Expected: PASS.
- [ ] **Step 5: Commit** `git commit -m "feat(ui): PR/MR noun and per-forge remedies"`

---

### Task 9: Specs and README

**Files:**
- Modify: `specs/forge-host.md` (multi-forge contract), `specs/config.md` (two new keys), `README.md` (Requirements + PR-tab + Configuration sections)

- [ ] **Step 1: Update `specs/forge-host.md`**
  - Title/intro: "How reviewr reads one pull/merge request from its forge — GitHub via `gh`, GitLab via `glab`, Bitbucket Data Center via `curl`."
  - Replace the "GitHub hosts" section with the three-forge host table (built-ins `github.com`, `gitlab.com`; config keys `github_host`, `gitlab_host`, `bitbucket_host`; SSH-alias rule unchanged; Bitbucket `/scm/` path rule; bitbucket.org named unsupported).
  - Add the concept-mapping table from the design doc (snapshot field ↔ GitLab ↔ Bitbucket).
  - Resolution section: note GitLab/Bitbucket resolve per-candidate with one list call each (≤8), same `Pick` policy; GitHub keeps the aliased single call.
  - Failure semantics: per-forge remedies (`glab auth login`, `BITBUCKET_TOKEN` / git credential), `NoToken` state.
  - Remove the "No second forge" non-goal; keep "no writes", extend to all forges.
- [ ] **Step 2: Update `specs/config.md`** with `gitlab_host` / `bitbucket_host` — copy the `github_host` contract's wording shape (bare hostname, lowercased, rejects URLs).
- [ ] **Step 3: Update `README.md`** — Requirements: `gh`/`glab`/`curl` each optional, only its forge's PR tab needs it; Bitbucket token setup paragraph (`BITBUCKET_TOKEN` or git credentials); Configuration section gains the two keys with examples.
- [ ] **Step 4: Self-check** — `grep -n "No second forge" specs/` → empty; `just ci` still green (doc-only).
- [ ] **Step 5: Commit** `git commit -m "docs: multi-forge contract in specs and README"`

---

### Task 10: End-to-end verification and push

- [ ] **Step 1: Full local gate** — `just ci` → green. `cargo test 2>&1 | tail -3` shows the total test count grew vs. upstream's baseline (record both numbers in the report).
- [ ] **Step 2: Windows-side musl cross-check is not possible** — instead verify the workflow YAML parses: `gh workflow list` after push, and confirm the `static-linux` CI job passes on GitHub.
- [ ] **Step 3: Push the branch** — `git push -u origin multiforge-fork`, confirm CI green on the fork (`gh run watch` or `gh run list --branch multiforge-fork`).
- [ ] **Step 4: Report** — surface: CI link, test-count delta, and the release step left for the user (tag `v0.12.0` when ready; the release workflow then publishes musl assets that `install.sh` consumes).

---

## Self-Review Notes

- **Spec coverage:** musl (T1), identity (T2), config keys (T3), classification incl. `/scm/` and nested groups (T4), module split + parameterized degradation (T5), GitLab backend (T6), Bitbucket backend incl. stdin token + credential fallback + epoch→ISO (T7), PR/MR noun (T8), spec/README updates (T9), verification (T10). Non-goals respected: no new deps, no writes, no Bitbucket Cloud.
- **Type consistency:** `ForgeError`, `FetchTarget`, `Forge`, `ForgeHosts`, `run_tool` signatures are defined once (T4/T5) and consumed by name in T6-T8.
- **Known judgment calls for implementers:** exact GitLab `detailed_merge_status` and Bitbucket activity field names should be checked against current API docs if a fixture seems off — the mapping *policy* (what folds to Conflicting/Blocked/Clean, what makes a Finding) is fixed by this plan; field spellings are verifiable details.
