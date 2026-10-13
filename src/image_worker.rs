//! The single coalescing image-preparation worker.

use std::{
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    thread,
};

use crate::{
    git,
    image_preview::{self, Prepared, Provenance, Side},
    wake,
};

/// Stable view identity is separate from this content/geometry generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub view_key: ViewIdentity,
    pub content_generation: u64,
    pub session_epoch: u64,
    pub before: Source,
    pub after: Source,
    pub cells: (u16, u16),
    pub ids: (u32, u32),
    pub graphics: bool,
    pub metrics: (u16, u16),
    pub metrics_estimated: bool,
}

/// User-selected identity, independent of resolved HEAD and turn snapshots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewIdentity {
    pub repo: PathBuf,
    pub scope: crate::model::Scope,
    pub base: Option<String>,
    pub selector_epoch: u64,
    pub pick: Option<crate::model::CommitPick>,
    pub path: String,
    pub previous_path: Option<String>,
}

/// Exact bytes are selected from landed source provenance, never a text diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Missing,
    Blob { oid: String },
    Worktree { path: String },
    Unsupported { detail: String },
}

#[derive(Clone, Debug)]
pub struct ResultPair {
    pub request: Request,
    pub before: Side,
    pub after: Side,
    pub prepared: (Option<Prepared>, Option<Prepared>),
}

#[derive(Debug, Default)]
struct State {
    request: Option<Request>,
    generation: u64,
    result: Option<ResultPair>,
    closed: bool,
}

/// At most one admitted request and one active decode exist. New demand replaces pending demand.
#[derive(Debug)]
pub struct Worker {
    state: Arc<(Mutex<State>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Worker {
    pub fn new(output: wake::Sender<()>) -> std::io::Result<Self> {
        let state = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let thread_state = Arc::clone(&state);
        let handle = thread::Builder::new().name("reviewr-image".into()).spawn(move || {
            let mut cached = None;
            loop {
                let (request, serial_at) = {
                    let (lock, ready) = &*thread_state;
                    let mut state = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    while state.request.is_none() && !state.closed {
                        state =
                            ready.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                    if state.closed {
                        return;
                    }
                    (state.request.take().expect("request checked"), state.generation)
                };
                let current = || {
                    thread_state
                        .0
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .generation
                        == serial_at
                };
                let Some(built) = build(request, cached.as_ref(), &current) else { continue };
                cached = Some(built.clone());
                let mut state =
                    thread_state.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.generation != serial_at {
                    continue;
                }
                state.result = Some(built);
                drop(state);
                if output.send(()).is_err() {
                    return;
                }
            }
        })?;
        Ok(Self { state, thread: Some(handle) })
    }

    /// Coalesce before work starts; an in-flight decoder is non-preemptible by codec contract.
    pub fn request(&self, request: Request) {
        let (lock, ready) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.generation = state.generation.wrapping_add(1);
        state.request = Some(request);
        ready.notify_one();
    }

    /// Stop demand at a mask/view boundary; codec work already running ends at its next checkpoint.
    pub fn cancel(&self) {
        let (lock, _) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.generation = state.generation.wrapping_add(1);
        state.request = None;
    }

    /// Consume the newest completed pair. Older completions are intentionally coalesced.
    pub fn take_result(&self) -> Option<ResultPair> {
        self.state.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).result.take()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let (lock, ready) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.generation = state.generation.wrapping_add(1);
        state.closed = true;
        state.request = None;
        ready.notify_one();
        drop(state);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn build(
    request: Request,
    cached: Option<&ResultPair>,
    current: &impl Fn() -> bool,
) -> Option<ResultPair> {
    if !current() {
        return None;
    }
    let cached = cached.filter(|pair| pair.request.view_key.repo == request.view_key.repo);
    let mut before = read_cached(
        &request.view_key.repo,
        &request.before,
        cached.map(|p| (&p.request.before, &p.before)),
        request.graphics,
        current,
    );
    if !current() {
        return None;
    }
    let mut after = read_cached(
        &request.view_key.repo,
        &request.after,
        cached.map(|p| (&p.request.after, &p.after)),
        request.graphics,
        current,
    );
    if !current() {
        return None;
    }
    let reusable =
        cached.filter(|p| p.request.cells == request.cells && p.request.metrics == request.metrics);
    let left = prepare(
        &mut before,
        request.ids.0,
        &request,
        reusable.map(|p| (&p.before, &p.prepared.0, p.request.session_epoch)),
    );
    if !current() {
        return None;
    }
    let right = prepare(
        &mut after,
        request.ids.1,
        &request,
        reusable.map(|p| (&p.after, &p.prepared.1, p.request.session_epoch)),
    );
    if !current() {
        return None;
    }
    Some(ResultPair { request, before, after, prepared: (left, right) })
}

fn read_cached(
    repo: &std::path::Path,
    source: &Source,
    cached: Option<(&Source, &Side)>,
    pixels: bool,
    current: &impl Fn() -> bool,
) -> Side {
    if let Some((old, side)) = cached
        && old == source
        && matches!(source, Source::Blob { .. })
        && let Some(side) = cached_side(side, pixels)
    {
        return side;
    }
    match source {
        Source::Missing => Side::Missing,
        Source::Unsupported { detail } => Side::Unsupported {
            detail: detail.clone(),
            provenance: Provenance::Unavailable { detail: "source not read".into() },
        },
        Source::Blob { oid } => {
            match git::blob_bytes_current(repo, oid, image_preview::MAX_SOURCE_BYTES, current) {
                Ok(bytes) => parse(&bytes, Provenance::Blob { oid: oid.clone() }, pixels),
                Err(error) => Side::Error {
                    detail: error.0,
                    provenance: Provenance::Blob { oid: oid.clone() },
                },
            }
        }
        Source::Worktree { path } => {
            match git::guarded_worktree_bytes(repo, path, image_preview::MAX_SOURCE_BYTES) {
                Ok(bytes) => {
                    let provenance = Provenance::Worktree { digest: digest(&bytes) };
                    if let Some((_, old)) = cached
                        && provenance == old.provenance()
                        && let Some(side) = cached_side(old, pixels)
                    {
                        return side;
                    }
                    parse(&bytes, provenance, pixels)
                }
                Err(error) => Side::Error {
                    detail: error.0,
                    provenance: Provenance::Unavailable { detail: format!("worktree {path}") },
                },
            }
        }
    }
}

fn cached_side(side: &Side, pixels: bool) -> Option<Side> {
    match side {
        Side::Ready(decoded) if !pixels => Some(Side::Metadata {
            format: decoded.format,
            width: decoded.width,
            height: decoded.height,
            provenance: decoded.provenance.clone(),
        }),
        Side::Ready(_) => Some(side.clone()),
        Side::Metadata { .. } if !pixels => Some(side.clone()),
        _ => None,
    }
}

fn parse(bytes: &[u8], provenance: Provenance, pixels: bool) -> Side {
    if pixels {
        image_preview::decode(bytes, provenance)
    } else {
        image_preview::metadata(bytes, provenance)
    }
}

fn prepare(
    side: &mut Side,
    id: u32,
    request: &Request,
    cached: Option<(&Side, &Option<Prepared>, u64)>,
) -> Option<Prepared> {
    if !request.graphics {
        return None;
    }
    let label = side.label();
    let Side::Ready(decoded) = side else { return None };
    let result = match cached {
        Some((old, Some(prepared), epoch)) if decoded.provenance == old.provenance() => {
            if epoch == request.session_epoch {
                return Some(prepared.clone());
            }
            image_preview::reconstruct(prepared, id)
        }
        _ => image_preview::prepare(decoded, id, request.cells.0, request.cells.1, request.metrics),
    };
    match result {
        Ok(prepared) => Some(prepared),
        Err(detail) => {
            let provenance = decoded.provenance.clone();
            *side = Side::Error { detail: format!("{label}: {detail}"), provenance };
            None
        }
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}
