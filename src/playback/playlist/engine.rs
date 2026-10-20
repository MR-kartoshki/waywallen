use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;

use super::cursor::PlaylistCursor;
use super::port::{ApplyAssignment, ApplyPort, ApplyRequest, ApplySource, Target, TargetId};
use crate::error::{Error, Result};
use crate::playback::rotation::{make_handle, RotationConfig, RotationHandle};
use crate::playback::Mode;
use crate::wallframe::scheduler::DisplayId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub id: i64,
    pub mode: Mode,
    pub interval_secs: u32,
    pub synchronized_selection: bool,
    pub items: Vec<String>,
    pub keep_empty: bool,
}

#[derive(Debug, Clone)]
pub struct Activation {
    pub definition: Definition,
    pub targets: Vec<Target>,
    pub resume_by_display: HashMap<DisplayId, String>,
    pub first_frame_timeout: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct DisplayStatus {
    pub display_id: DisplayId,
    pub active_id: i64,
    pub mode: Mode,
    pub interval_secs: u32,
    pub current_id: Option<String>,
    pub position: u32,
    pub count: u32,
    pub remaining_secs: u32,
    pub waiting: bool,
    pub error: String,
}

#[derive(Clone)]
pub struct RefreshTicket {
    session: Arc<PlaylistSession>,
    definition: Definition,
}

impl RefreshTicket {
    pub fn playlist_id(&self) -> i64 {
        self.definition.id
    }
}

struct SessionTarget {
    display_ids: Vec<DisplayId>,
    cursor: PlaylistCursor,
    current: Option<String>,
}

struct SessionData {
    definition: Definition,
    seed: u64,
    synchronized_cursor: PlaylistCursor,
    targets: BTreeMap<TargetId, SessionTarget>,
    error: String,
}

impl SessionData {
    fn new(definition: Definition, seed: u64) -> Self {
        let synchronized_cursor =
            PlaylistCursor::new(definition.items.clone(), definition.mode, seed);
        Self {
            definition,
            seed,
            synchronized_cursor,
            targets: BTreeMap::new(),
            error: String::new(),
        }
    }

    fn synchronized(&self) -> bool {
        self.definition.mode == Mode::Sequential || self.definition.synchronized_selection
    }

    fn target_seed(&self, target_id: &TargetId) -> u64 {
        let mut hash = self.seed ^ 0xcbf2_9ce4_8422_2325;
        let mut feed = |byte: u8| {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        };
        match target_id {
            TargetId::Display(display_id) => {
                feed(0);
                for byte in display_id.to_le_bytes() {
                    feed(byte);
                }
            }
            TargetId::Canvas(canvas_id) => {
                feed(1);
                for byte in canvas_id.as_bytes() {
                    feed(*byte);
                }
            }
        }
        hash.max(1)
    }

    fn resume_for(
        target: &Target,
        resume_by_display: &HashMap<DisplayId, String>,
    ) -> Option<String> {
        target
            .display_ids
            .iter()
            .find_map(|display_id| resume_by_display.get(display_id).cloned())
    }

    fn upsert_target(
        &mut self,
        mut target: Target,
        resume_by_display: &HashMap<DisplayId, String>,
    ) -> (Option<(String, TargetId)>, Vec<DisplayId>) {
        target.display_ids.sort_unstable();
        target.display_ids.dedup();
        if let Some(existing) = self.targets.get_mut(&target.id) {
            let stale = existing
                .display_ids
                .iter()
                .copied()
                .filter(|display_id| !target.display_ids.contains(display_id))
                .collect();
            existing.display_ids = target.display_ids;
            return (
                existing
                    .current
                    .clone()
                    .map(|entry_id| (entry_id, target.id)),
                stale,
            );
        }

        let mut cursor = PlaylistCursor::new(
            self.definition.items.clone(),
            self.definition.mode,
            self.target_seed(&target.id),
        );
        let resume_id = Self::resume_for(&target, resume_by_display);
        let current = if self.synchronized() {
            if self.synchronized_cursor.current.is_none() {
                match resume_id {
                    Some(id) if self.synchronized_cursor.set_current(&id) => {}
                    _ => {
                        self.synchronized_cursor.first();
                    }
                }
            }
            let current = self.synchronized_cursor.current.clone();
            if let Some(entry_id) = &current {
                cursor.set_current(entry_id);
            }
            current
        } else {
            let current = match resume_id {
                Some(id) if cursor.set_current(&id) => Some(id),
                _ => cursor.first(),
            };
            if self.synchronized_cursor.current.is_none() {
                if let Some(entry_id) = &current {
                    self.synchronized_cursor.set_current(entry_id);
                }
            }
            current
        };
        let assignment = current
            .clone()
            .map(|entry_id| (entry_id, target.id.clone()));
        self.targets.insert(
            target.id,
            SessionTarget {
                display_ids: target.display_ids,
                cursor,
                current,
            },
        );
        (assignment, Vec::new())
    }

    fn remove_display(&mut self, target_id: &TargetId, display_id: DisplayId) {
        let remove_target = if let Some(target) = self.targets.get_mut(target_id) {
            target.display_ids.retain(|id| *id != display_id);
            target.display_ids.is_empty()
        } else {
            false
        };
        if remove_target {
            self.targets.remove(target_id);
        }
    }

    fn step_assignments(&mut self, delta: i32) -> Vec<ApplyAssignment> {
        if self.targets.is_empty() {
            return Vec::new();
        }
        let mut selections = Vec::new();
        if self.synchronized() {
            let Some(entry_id) = self.synchronized_cursor.next(delta) else {
                return Vec::new();
            };
            for (target_id, target) in &mut self.targets {
                target.current = Some(entry_id.clone());
                target.cursor.set_current(&entry_id);
                selections.push((entry_id.clone(), target_id.clone()));
            }
        } else {
            for (target_id, target) in &mut self.targets {
                if let Some(entry_id) = target.cursor.next(delta) {
                    target.current = Some(entry_id.clone());
                    selections.push((entry_id, target_id.clone()));
                }
            }
        }
        group_assignments(selections)
    }

    fn jump_to(&mut self, entry_id: &str) -> Vec<ApplyAssignment> {
        if !self.synchronized_cursor.set_current(entry_id) {
            return Vec::new();
        }
        let mut selections = Vec::new();
        for (target_id, target) in &mut self.targets {
            target.cursor.set_current(entry_id);
            target.current = Some(entry_id.to_owned());
            selections.push((entry_id.to_owned(), target_id.clone()));
        }
        group_assignments(selections)
    }

    fn reconfigure(&mut self, definition: Definition) -> Vec<ApplyAssignment> {
        let previous_shared = self.synchronized_cursor.current.clone().or_else(|| {
            self.targets
                .values()
                .find_map(|target| target.current.clone())
        });
        let mut synchronized_cursor = self.synchronized_cursor.clone();
        synchronized_cursor.replace_items(definition.items.clone(), definition.mode);
        let shared_valid = previous_shared
            .as_deref()
            .is_some_and(|entry_id| synchronized_cursor.set_current(entry_id));
        if !shared_valid {
            synchronized_cursor.first();
        }

        let mut invalid_targets = Vec::new();
        for (target_id, target) in &mut self.targets {
            let mut cursor = target.cursor.clone();
            cursor.replace_items(definition.items.clone(), definition.mode);
            let current_valid = target
                .current
                .as_deref()
                .is_some_and(|entry_id| cursor.set_current(entry_id));
            if !current_valid {
                target.current = cursor.first();
                invalid_targets.push(target_id.clone());
            }
            target.cursor = cursor;
        }
        self.definition = definition;
        self.synchronized_cursor = synchronized_cursor;
        let mut selections = Vec::new();
        if self.synchronized() && (!shared_valid || !invalid_targets.is_empty()) {
            if let Some(entry_id) = self.synchronized_cursor.current.clone() {
                for (target_id, target) in &mut self.targets {
                    target.current = Some(entry_id.clone());
                    target.cursor.set_current(&entry_id);
                    selections.push((entry_id.clone(), target_id.clone()));
                }
            }
        } else if !self.synchronized() {
            for target_id in invalid_targets {
                if let Some(target) = self.targets.get(&target_id) {
                    if let Some(entry_id) = &target.current {
                        selections.push((entry_id.clone(), target_id));
                    }
                }
            }
        }
        group_assignments(selections)
    }

    fn display_ids(&self) -> Vec<DisplayId> {
        self.targets
            .values()
            .flat_map(|target| target.display_ids.iter().copied())
            .collect()
    }
}

fn group_assignments(
    selections: impl IntoIterator<Item = (String, TargetId)>,
) -> Vec<ApplyAssignment> {
    let mut grouped: BTreeMap<String, Vec<TargetId>> = BTreeMap::new();
    for (entry_id, target_id) in selections {
        let targets = grouped.entry(entry_id).or_default();
        if !targets.contains(&target_id) {
            targets.push(target_id);
        }
    }
    grouped
        .into_iter()
        .map(|(entry_id, targets)| ApplyAssignment { entry_id, targets })
        .collect()
}

fn merge_assignments(assignments: Vec<ApplyAssignment>) -> Vec<ApplyAssignment> {
    group_assignments(assignments.into_iter().flat_map(|assignment| {
        assignment
            .targets
            .into_iter()
            .map(move |target| (assignment.entry_id.clone(), target))
    }))
}

struct PlaylistSession {
    data: Arc<Mutex<SessionData>>,
    handle: RotationHandle,
    deadline: Arc<std::sync::Mutex<Option<Instant>>>,
    task: JoinHandle<()>,
}

impl PlaylistSession {
    fn new(definition: Definition, apply: ApplyPort, shutdown: watch::Receiver<bool>) -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(1)
            .max(1);
        let interval_secs = if definition.items.is_empty() {
            0
        } else {
            definition.interval_secs
        };
        let data = Arc::new(Mutex::new(SessionData::new(definition, seed)));
        let (handle, receiver) = make_handle();
        handle.set_interval(interval_secs);
        let deadline = Arc::new(std::sync::Mutex::new(None));
        let task = tokio::spawn(run_playlist_rotator(
            data.clone(),
            deadline.clone(),
            apply,
            receiver,
            shutdown,
        ));
        Self {
            data,
            handle,
            deadline,
            task,
        }
    }
}

impl Drop for PlaylistSession {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Default)]
struct EngineState {
    by_playlist: HashMap<i64, Arc<PlaylistSession>>,
    by_display: HashMap<DisplayId, (i64, TargetId)>,
}

#[derive(Default)]
pub struct Engine {
    state: Mutex<EngineState>,
    operations: Mutex<()>,
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn owned_display_ids(&self) -> Vec<DisplayId> {
        self.state.lock().await.by_display.keys().copied().collect()
    }

    pub async fn is_owned(&self, display_id: DisplayId) -> bool {
        self.state.lock().await.by_display.contains_key(&display_id)
    }

    pub async fn owner_playlist(&self, display_id: DisplayId) -> Option<i64> {
        self.state
            .lock()
            .await
            .by_display
            .get(&display_id)
            .map(|(playlist_id, _)| *playlist_id)
    }

    async fn remove_display_inner(&self, display_id: DisplayId) {
        let mapping = {
            let mut state = self.state.lock().await;
            let Some((playlist_id, target_id)) = state.by_display.remove(&display_id) else {
                return;
            };
            let session = state.by_playlist.get(&playlist_id).cloned();
            (playlist_id, target_id, session)
        };
        let (playlist_id, target_id, Some(session)) = mapping else {
            return;
        };
        let empty = {
            let mut data = session.data.lock().await;
            data.remove_display(&target_id, display_id);
            data.targets.is_empty()
        };
        if empty {
            let mut state = self.state.lock().await;
            if state
                .by_playlist
                .get(&playlist_id)
                .is_some_and(|current| Arc::ptr_eq(current, &session))
            {
                state.by_playlist.remove(&playlist_id);
            }
        }
    }

    pub async fn activate(
        &self,
        activation: Activation,
        apply: ApplyPort,
        shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let Activation {
            definition,
            mut targets,
            resume_by_display,
            first_frame_timeout,
        } = activation;
        if definition.items.is_empty() && !definition.keep_empty {
            return Err(Error::PlaylistInvalid("playlist has no wallpapers".into()));
        }
        targets.retain(|target| !target.display_ids.is_empty());
        if targets.is_empty() {
            return Ok(());
        }

        let operation = self.operations.lock().await;
        let mut desired = HashMap::new();
        for target in &targets {
            for display_id in &target.display_ids {
                if desired.insert(*display_id, target.id.clone()).is_some() {
                    return Err(Error::PlaylistInvalid(format!(
                        "display {display_id} belongs to multiple playlist targets"
                    )));
                }
            }
        }
        let conflicts = {
            let state = self.state.lock().await;
            desired
                .iter()
                .filter_map(|(display_id, target_id)| {
                    state
                        .by_display
                        .get(display_id)
                        .filter(|(playlist_id, current_target)| {
                            *playlist_id != definition.id || current_target != target_id
                        })
                        .map(|_| *display_id)
                })
                .collect::<Vec<_>>()
        };
        for display_id in conflicts {
            self.remove_display_inner(display_id).await;
        }

        let (session, created) = {
            let mut state = self.state.lock().await;
            if let Some(session) = state.by_playlist.get(&definition.id).cloned() {
                (session, false)
            } else {
                let session = Arc::new(PlaylistSession::new(
                    definition.clone(),
                    apply.clone(),
                    shutdown,
                ));
                state.by_playlist.insert(definition.id, session.clone());
                (session, true)
            }
        };

        let (mut assignments, stale_displays, definition_changed, interval_changed) = {
            let mut data = session.data.lock().await;
            let old_definition = data.definition.clone();
            let mut assignments = if created || old_definition == definition {
                Vec::new()
            } else {
                data.reconfigure(definition.clone())
            };
            let mut stale_displays = Vec::new();
            let mut selections = Vec::new();
            for target in targets.clone() {
                let (selection, stale) = data.upsert_target(target, &resume_by_display);
                selections.extend(selection);
                stale_displays.extend(stale);
            }
            assignments.extend(group_assignments(selections));
            (
                assignments,
                stale_displays,
                old_definition != definition,
                old_definition.interval_secs != definition.interval_secs,
            )
        };
        assignments = merge_assignments(assignments);

        {
            let mut state = self.state.lock().await;
            for display_id in stale_displays {
                if state
                    .by_display
                    .get(&display_id)
                    .is_some_and(|(playlist_id, _)| *playlist_id == definition.id)
                {
                    state.by_display.remove(&display_id);
                }
            }
            for target in &targets {
                for display_id in &target.display_ids {
                    state
                        .by_display
                        .insert(*display_id, (definition.id, target.id.clone()));
                }
            }
        }
        if !created {
            session.handle.set_interval(if definition.items.is_empty() {
                0
            } else {
                definition.interval_secs
            });
            if definition_changed && !interval_changed {
                session.handle.kick();
            }
        }
        let incoming_displays = desired.keys().copied().collect::<Vec<_>>();
        drop(operation);

        let result = apply
            .apply(ApplyRequest {
                source: ApplySource::Activation,
                assignments,
                first_frame_timeout,
            })
            .await;
        if first_frame_timeout.is_some() && result.is_err() {
            self.deactivate(&incoming_displays).await;
        }
        result
    }

    pub async fn attach(
        &self,
        mut target: Target,
        playlist_id: i64,
        resume_by_display: HashMap<DisplayId, String>,
        first_frame_timeout: Duration,
        apply: ApplyPort,
    ) -> Result<bool> {
        target.display_ids.sort_unstable();
        target.display_ids.dedup();
        if target.display_ids.is_empty() {
            return Ok(true);
        }
        let operation = self.operations.lock().await;
        let (session, relocated, already_mapped) = {
            let state = self.state.lock().await;
            let Some(session) = state.by_playlist.get(&playlist_id).cloned() else {
                return Ok(false);
            };
            if target.display_ids.iter().any(|display_id| {
                state
                    .by_display
                    .get(display_id)
                    .is_some_and(|(owner, _)| *owner != playlist_id)
            }) {
                return Ok(false);
            }
            let relocated = target
                .display_ids
                .iter()
                .copied()
                .filter(|display_id| {
                    state
                        .by_display
                        .get(display_id)
                        .is_some_and(|(_, target_id)| target_id != &target.id)
                })
                .collect::<Vec<_>>();
            let already_mapped = target.display_ids.iter().all(|display_id| {
                state
                    .by_display
                    .get(display_id)
                    .is_some_and(|(owner, target_id)| {
                        *owner == playlist_id && target_id == &target.id
                    })
            });
            (session, relocated, already_mapped)
        };
        if already_mapped {
            let data = session.data.lock().await;
            if data
                .targets
                .get(&target.id)
                .is_some_and(|current| current.display_ids == target.display_ids)
            {
                return Ok(true);
            }
        }
        for display_id in relocated {
            self.remove_display_inner(display_id).await;
        }
        let (selection, stale) = session
            .data
            .lock()
            .await
            .upsert_target(target.clone(), &resume_by_display);
        {
            let mut state = self.state.lock().await;
            state
                .by_playlist
                .entry(playlist_id)
                .or_insert_with(|| session.clone());
            for display_id in stale {
                state.by_display.remove(&display_id);
            }
            for display_id in &target.display_ids {
                state
                    .by_display
                    .insert(*display_id, (playlist_id, target.id.clone()));
            }
        }
        drop(operation);
        apply
            .apply(ApplyRequest {
                source: ApplySource::Attach,
                assignments: group_assignments(selection),
                first_frame_timeout: Some(first_frame_timeout),
            })
            .await?;
        Ok(true)
    }

    pub async fn deactivate(&self, display_ids: &[DisplayId]) {
        let _operation = self.operations.lock().await;
        for display_id in display_ids {
            self.remove_display_inner(*display_id).await;
        }
    }

    pub async fn jump_to(&self, playlist_id: i64, entry_id: &str, apply: ApplyPort) -> Result<()> {
        let operation = self.operations.lock().await;
        let session = self
            .state
            .lock()
            .await
            .by_playlist
            .get(&playlist_id)
            .cloned();
        let Some(session) = session else {
            return Ok(());
        };
        let assignments = session.data.lock().await.jump_to(entry_id);
        if assignments.is_empty() {
            return Ok(());
        }
        session.handle.kick();
        drop(operation);
        apply
            .apply(ApplyRequest {
                source: ApplySource::Jump,
                assignments,
                first_frame_timeout: None,
            })
            .await
    }

    pub async fn step(&self, delta: i32, apply: ApplyPort) -> Result<bool> {
        let operation = self.operations.lock().await;
        let sessions = self
            .state
            .lock()
            .await
            .by_playlist
            .values()
            .cloned()
            .collect::<Vec<_>>();
        if sessions.is_empty() {
            return Ok(false);
        }

        let mut assignments = Vec::new();
        for session in &sessions {
            assignments.extend(session.data.lock().await.step_assignments(delta));
            session.handle.kick();
        }
        let assignments = merge_assignments(assignments);
        drop(operation);

        if !assignments.is_empty() {
            apply
                .apply(ApplyRequest {
                    source: ApplySource::Step,
                    assignments,
                    first_frame_timeout: None,
                })
                .await?;
        }
        Ok(true)
    }

    pub async fn status(&self) -> Vec<DisplayStatus> {
        let sessions = self
            .state
            .lock()
            .await
            .by_playlist
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let now = Instant::now();
        let mut statuses = Vec::new();
        for session in sessions {
            let remaining_secs = session
                .deadline
                .lock()
                .unwrap()
                .map(|deadline| deadline.saturating_duration_since(now).as_secs() as u32)
                .unwrap_or(0);
            let data = session.data.lock().await;
            let interval_secs = data.definition.interval_secs;
            let synchronized = data.synchronized();
            for target in data.targets.values() {
                let position = if synchronized {
                    data.synchronized_cursor.pos as u32
                } else {
                    target.cursor.pos as u32
                };
                for display_id in &target.display_ids {
                    statuses.push(DisplayStatus {
                        display_id: *display_id,
                        active_id: data.definition.id,
                        mode: data.definition.mode,
                        interval_secs,
                        current_id: target.current.clone(),
                        position,
                        count: data.definition.items.len() as u32,
                        remaining_secs,
                        waiting: data.definition.keep_empty && data.definition.items.is_empty(),
                        error: data.error.clone(),
                    });
                }
            }
        }
        statuses
    }

    pub async fn drop_display(&self, display_id: DisplayId) {
        let _operation = self.operations.lock().await;
        self.remove_display_inner(display_id).await;
    }

    pub async fn shutdown(&self) {
        let _operation = self.operations.lock().await;
        let mut state = self.state.lock().await;
        state.by_display.clear();
        state.by_playlist.clear();
    }

    pub async fn deactivate_playlist(&self, playlist_id: i64) -> Vec<DisplayId> {
        let _operation = self.operations.lock().await;
        let session = self.state.lock().await.by_playlist.remove(&playlist_id);
        let Some(session) = session else {
            return Vec::new();
        };
        let display_ids = session.data.lock().await.display_ids();
        self.state
            .lock()
            .await
            .by_display
            .retain(|_, (owner, _)| *owner != playlist_id);
        display_ids
    }

    pub async fn rebuild(
        &self,
        definition: Definition,
        apply: ApplyPort,
    ) -> Result<Vec<DisplayId>> {
        self.rebuild_inner(definition, apply, None).await
    }

    pub async fn refresh_tickets(&self) -> Vec<RefreshTicket> {
        let sessions = self
            .state
            .lock()
            .await
            .by_playlist
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut tickets = Vec::new();
        for session in sessions {
            let definition = session.data.lock().await.definition.clone();
            if definition.keep_empty {
                tickets.push(RefreshTicket {
                    session,
                    definition,
                });
            }
        }
        tickets
    }

    pub async fn refresh(
        &self,
        ticket: RefreshTicket,
        definition: Definition,
        apply: ApplyPort,
    ) -> Result<()> {
        self.rebuild_inner(definition, apply, Some(ticket))
            .await
            .map(|_| ())
    }

    pub async fn refresh_error(&self, ticket: &RefreshTicket, error: String) -> bool {
        let _operation = self.operations.lock().await;
        let session = self
            .state
            .lock()
            .await
            .by_playlist
            .get(&ticket.playlist_id())
            .cloned();
        let Some(session) = session else {
            return false;
        };
        if !Arc::ptr_eq(&session, &ticket.session) {
            return false;
        }
        let mut data = session.data.lock().await;
        if data.definition != ticket.definition || data.error == error {
            return false;
        }
        data.error = error;
        true
    }

    async fn rebuild_inner(
        &self,
        definition: Definition,
        apply: ApplyPort,
        ticket: Option<RefreshTicket>,
    ) -> Result<Vec<DisplayId>> {
        let operation = self.operations.lock().await;
        let session = self
            .state
            .lock()
            .await
            .by_playlist
            .get(&definition.id)
            .cloned();
        let Some(session) = session else {
            return Ok(Vec::new());
        };
        if let Some(ticket) = ticket {
            if !Arc::ptr_eq(&session, &ticket.session)
                || session.data.lock().await.definition != ticket.definition
            {
                return Ok(Vec::new());
            }
        }
        if definition.items.is_empty() && !definition.keep_empty {
            let display_ids = session.data.lock().await.display_ids();
            {
                let mut state = self.state.lock().await;
                state.by_playlist.remove(&definition.id);
                state
                    .by_display
                    .retain(|_, (owner, _)| *owner != definition.id);
            }
            return Ok(display_ids);
        }
        let old_definition = session.data.lock().await.definition.clone();
        session.data.lock().await.error.clear();
        if old_definition == definition {
            return Ok(Vec::new());
        }
        let assignments = session.data.lock().await.reconfigure(definition.clone());
        session.handle.set_interval(if definition.items.is_empty() {
            0
        } else {
            definition.interval_secs
        });
        if !assignments.is_empty() && old_definition.interval_secs == definition.interval_secs {
            session.handle.kick();
        }
        drop(operation);
        apply
            .apply(ApplyRequest {
                source: ApplySource::Rebuild,
                assignments,
                first_frame_timeout: None,
            })
            .await?;
        Ok(Vec::new())
    }

    pub async fn set_interval(&self, playlist_id: i64, interval_secs: u32) {
        if let Some(session) = self
            .state
            .lock()
            .await
            .by_playlist
            .get(&playlist_id)
            .cloned()
        {
            let mut data = session.data.lock().await;
            data.definition.interval_secs = interval_secs;
            session
                .handle
                .set_interval(if data.definition.items.is_empty() {
                    0
                } else {
                    interval_secs
                });
        }
    }
}

async fn run_playlist_rotator(
    data: Arc<Mutex<SessionData>>,
    deadline: Arc<std::sync::Mutex<Option<Instant>>>,
    apply: ApplyPort,
    mut config: watch::Receiver<RotationConfig>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut scheduled_deadline = None;
    loop {
        let current = *config.borrow();
        if current.interval_secs == 0 {
            scheduled_deadline = None;
            *deadline.lock().unwrap() = None;
            tokio::select! {
                _ = config.changed() => continue,
                changed = shutdown.changed() => if changed.is_err() || *shutdown.borrow() { break },
            }
        } else {
            let duration = Duration::from_secs(current.interval_secs as u64);
            let due = scheduled_deadline.unwrap_or_else(|| Instant::now() + duration);
            *deadline.lock().unwrap() = Some(due);
            tokio::select! {
                _ = tokio::time::sleep(due.saturating_duration_since(Instant::now())) => {
                    if config.borrow().interval_secs == 0 { continue; }
                    scheduled_deadline = due.checked_add(duration);
                    *deadline.lock().unwrap() = scheduled_deadline;
                    let assignments = data.lock().await.step_assignments(1);
                    if assignments.is_empty() { continue; }
                    if let Err(error) = apply.apply(ApplyRequest {
                        source: ApplySource::Rotation,
                        assignments,
                        first_frame_timeout: None,
                    }).await {
                        log::warn!("playlist rotator apply failed: {error:#}");
                    }
                    if let Some(mut next) = scheduled_deadline {
                        while next <= Instant::now() {
                            next = next.checked_add(duration).unwrap_or_else(|| Instant::now() + duration);
                        }
                        scheduled_deadline = Some(next);
                        *deadline.lock().unwrap() = scheduled_deadline;
                    }
                }
                _ = config.changed() => {
                    scheduled_deadline = None;
                    continue;
                },
                changed = shutdown.changed() => if changed.is_err() || *shutdown.borrow() { break },
            }
        }
    }
    *deadline.lock().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(id: i64, synchronized_selection: bool) -> Definition {
        Definition {
            id,
            mode: Mode::Shuffle,
            interval_secs: 0,
            synchronized_selection,
            items: vec!["a".into(), "b".into(), "c".into()],
            keep_empty: false,
        }
    }

    fn target(display_id: DisplayId) -> Target {
        Target {
            id: TargetId::Display(display_id),
            display_ids: vec![display_id],
        }
    }

    fn capture_port() -> (ApplyPort, Arc<Mutex<Vec<ApplyRequest>>>) {
        let applied = Arc::new(Mutex::new(Vec::new()));
        let capture = applied.clone();
        let port = ApplyPort::new(move |request| {
            let capture = capture.clone();
            async move {
                capture.lock().await.push(request);
                Ok(())
            }
        });
        (port, applied)
    }

    #[test]
    fn synchronized_selection_groups_targets() {
        let mut data = SessionData::new(definition(1, true), 7);
        data.upsert_target(target(10), &HashMap::new());
        data.upsert_target(target(20), &HashMap::new());
        let assignments = data.step_assignments(1);
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].targets.len(), 2);
    }

    #[test]
    fn independent_selection_keeps_per_target_cursor() {
        let mut data = SessionData::new(definition(1, false), 7);
        data.upsert_target(target(10), &HashMap::new());
        data.upsert_target(target(20), &HashMap::new());
        assert_ne!(
            data.targets[&TargetId::Display(10)].cursor.rng,
            data.targets[&TargetId::Display(20)].cursor.rng
        );
        assert_eq!(
            data.step_assignments(1)
                .iter()
                .map(|assignment| assignment.targets.len())
                .sum::<usize>(),
            2
        );
        assert_eq!(data.targets.len(), 2);
    }

    #[test]
    fn sequential_selection_is_always_synchronized() {
        let mut definition = definition(1, false);
        definition.mode = Mode::Sequential;
        let mut data = SessionData::new(definition, 7);
        data.upsert_target(target(10), &HashMap::new());
        data.upsert_target(target(20), &HashMap::new());

        let assignments = data.step_assignments(1);

        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].targets.len(), 2);
    }

    #[test]
    fn canvas_members_share_one_selection_unit() {
        let mut data = SessionData::new(definition(1, false), 7);
        data.upsert_target(
            Target {
                id: TargetId::Canvas("main".into()),
                display_ids: vec![10, 11],
            },
            &HashMap::new(),
        );
        data.upsert_target(target(20), &HashMap::new());

        let assignments = data.step_assignments(1);
        let targets = assignments
            .iter()
            .flat_map(|assignment| assignment.targets.iter())
            .collect::<Vec<_>>();

        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets
                .iter()
                .filter(|target| matches!(target, TargetId::Canvas(_)))
                .count(),
            1
        );
    }

    #[test]
    fn enabling_synchronized_selection_realigns_on_next_tick() {
        let mut data = SessionData::new(definition(1, false), 7);
        data.upsert_target(target(10), &HashMap::new());
        data.upsert_target(target(20), &HashMap::new());
        data.targets
            .get_mut(&TargetId::Display(20))
            .unwrap()
            .current = Some("c".into());

        let assignments = data.reconfigure(definition(1, true));

        assert!(assignments.is_empty());
        let assignments = data.step_assignments(1);
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].targets.len(), 2);
        assert!(data
            .targets
            .values()
            .all(|target| target.current == data.synchronized_cursor.current));
    }

    #[tokio::test]
    async fn separate_activation_uses_one_playlist_session() {
        let engine = Engine::new();
        let (port, _) = capture_port();
        let (_shutdown_tx, shutdown) = watch::channel(false);
        for display_id in [10, 20] {
            engine
                .activate(
                    Activation {
                        definition: definition(4, false),
                        targets: vec![target(display_id)],
                        resume_by_display: HashMap::new(),
                        first_frame_timeout: None,
                    },
                    port.clone(),
                    shutdown.clone(),
                )
                .await
                .unwrap();
        }
        let state = engine.state.lock().await;
        assert_eq!(state.by_playlist.len(), 1);
        assert_eq!(state.by_display.len(), 2);
    }

    #[tokio::test]
    async fn session_lives_until_its_last_display_leaves() {
        let engine = Engine::new();
        let (port, _) = capture_port();
        let (_shutdown_tx, shutdown) = watch::channel(false);
        engine
            .activate(
                Activation {
                    definition: definition(4, false),
                    targets: vec![target(10), target(20)],
                    resume_by_display: HashMap::new(),
                    first_frame_timeout: None,
                },
                port,
                shutdown,
            )
            .await
            .unwrap();

        engine.deactivate(&[10]).await;
        {
            let state = engine.state.lock().await;
            assert_eq!(state.by_playlist.len(), 1);
            assert_eq!(state.by_display.len(), 1);
        }

        engine.deactivate(&[20]).await;
        let state = engine.state.lock().await;
        assert!(state.by_playlist.is_empty());
        assert!(state.by_display.is_empty());
    }

    #[tokio::test]
    async fn attach_replaces_display_targets_with_canvas() {
        let engine = Engine::new();
        let (port, applied) = capture_port();
        let (_shutdown_tx, shutdown) = watch::channel(false);
        engine
            .activate(
                Activation {
                    definition: definition(4, false),
                    targets: vec![target(10), target(20)],
                    resume_by_display: HashMap::new(),
                    first_frame_timeout: None,
                },
                port.clone(),
                shutdown,
            )
            .await
            .unwrap();
        applied.lock().await.clear();

        assert!(engine
            .attach(
                Target {
                    id: TargetId::Canvas("main".into()),
                    display_ids: vec![10, 20],
                },
                4,
                HashMap::new(),
                Duration::from_secs(1),
                port,
            )
            .await
            .unwrap());

        let state = engine.state.lock().await;
        assert_eq!(state.by_playlist.len(), 1);
        assert_eq!(state.by_display.len(), 2);
        assert!(state
            .by_display
            .values()
            .all(|(_, target)| target == &TargetId::Canvas("main".into())));
        let session = state.by_playlist[&4].clone();
        drop(state);
        let data = session.data.lock().await;
        assert_eq!(data.targets.len(), 1);
        assert!(data.targets.contains_key(&TargetId::Canvas("main".into())));
        assert_eq!(applied.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn attach_unchanged_target_does_not_apply_again() {
        let engine = Engine::new();
        let (port, applied) = capture_port();
        let (_shutdown_tx, shutdown) = watch::channel(false);
        engine
            .activate(
                Activation {
                    definition: definition(4, false),
                    targets: vec![target(10)],
                    resume_by_display: HashMap::new(),
                    first_frame_timeout: None,
                },
                port.clone(),
                shutdown,
            )
            .await
            .unwrap();
        applied.lock().await.clear();

        assert!(engine
            .attach(target(10), 4, HashMap::new(), Duration::from_secs(1), port,)
            .await
            .unwrap());

        assert!(applied.lock().await.is_empty());
    }

    #[tokio::test]
    async fn activation_applies_without_application_context() {
        let engine = Engine::new();
        let (port, applied) = capture_port();
        let (_shutdown_tx, shutdown) = watch::channel(false);
        engine
            .activate(
                Activation {
                    definition: Definition {
                        id: 1,
                        mode: Mode::Sequential,
                        interval_secs: 0,
                        synchronized_selection: true,
                        items: vec!["42".into()],
                        keep_empty: false,
                    },
                    targets: vec![target(7)],
                    resume_by_display: HashMap::new(),
                    first_frame_timeout: Some(Duration::from_secs(1)),
                },
                port,
                shutdown,
            )
            .await
            .unwrap();
        let requests = applied.lock().await;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].assignments[0].entry_id, "42");
        assert_eq!(
            requests[0].assignments[0].targets,
            vec![TargetId::Display(7)]
        );
        assert_eq!(requests[0].source, ApplySource::Activation);
    }

    #[tokio::test]
    async fn manual_step_uses_playlist_direction_and_targets() {
        let engine = Engine::new();
        let (port, applied) = capture_port();
        let (_shutdown_tx, shutdown) = watch::channel(false);
        let mut definition = definition(1, false);
        definition.mode = Mode::Sequential;
        engine
            .activate(
                Activation {
                    definition,
                    targets: vec![target(10), target(20)],
                    resume_by_display: HashMap::new(),
                    first_frame_timeout: None,
                },
                port.clone(),
                shutdown,
            )
            .await
            .unwrap();
        applied.lock().await.clear();

        assert!(engine.step(-1, port).await.unwrap());

        let requests = applied.lock().await;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].source, ApplySource::Step);
        assert_eq!(requests[0].assignments.len(), 1);
        assert_eq!(requests[0].assignments[0].entry_id, "c");
        assert_eq!(requests[0].assignments[0].targets.len(), 2);
    }

    #[tokio::test]
    async fn manual_step_without_playlist_is_not_handled() {
        let engine = Engine::new();
        let (port, applied) = capture_port();

        assert!(!engine.step(1, port).await.unwrap());
        assert!(applied.lock().await.is_empty());
    }

    #[tokio::test]
    async fn smart_playlist_waits_and_rejects_stale_session_refresh() {
        let engine = Engine::new();
        let (port, applied) = capture_port();
        let (_shutdown_tx, shutdown) = watch::channel(false);
        let mut def = definition(1, true);
        def.keep_empty = true;
        def.items.clear();
        let activation = Activation {
            definition: def.clone(),
            targets: vec![target(10)],
            resume_by_display: HashMap::new(),
            first_frame_timeout: None,
        };
        engine
            .activate(activation.clone(), port.clone(), shutdown.clone())
            .await
            .unwrap();
        let status = engine.status().await;
        assert_eq!(status[0].count, 0);
        assert_eq!(status[0].current_id, None);
        assert_eq!(engine.owned_display_ids().await, vec![10]);
        let stale = engine.refresh_tickets().await.pop().unwrap();
        engine.deactivate(&[10]).await;
        engine
            .activate(activation, port.clone(), shutdown)
            .await
            .unwrap();
        let mut populated = def.clone();
        populated.items = vec!["new".into()];
        engine
            .refresh(stale, populated.clone(), port.clone())
            .await
            .unwrap();
        assert_eq!(engine.status().await[0].count, 0);
        let ticket = engine.refresh_tickets().await.pop().unwrap();
        engine
            .refresh(ticket, populated.clone(), port.clone())
            .await
            .unwrap();
        assert_eq!(engine.status().await[0].current_id.as_deref(), Some("new"));
        let ticket = engine.refresh_tickets().await.pop().unwrap();
        assert!(engine.refresh_error(&ticket, "query failed".into()).await);
        assert_eq!(engine.status().await[0].count, 1);
        assert_eq!(engine.status().await[0].error, "query failed");
        applied.lock().await.clear();
        engine.rebuild(populated, port.clone()).await.unwrap();
        assert!(engine.status().await[0].error.is_empty());
        assert!(applied.lock().await.is_empty());
        assert!(engine.rebuild(def, port).await.unwrap().is_empty());
        assert_eq!(engine.status().await[0].current_id, None);
        assert_eq!(engine.owned_display_ids().await, vec![10]);
    }
}
