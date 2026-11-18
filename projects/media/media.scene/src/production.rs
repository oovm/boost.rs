//! Production book: one project plus the things a finished work needs around it.
//!
//! [`Project`] stays the render truth: frames, scenes and items are what the
//! renderer consumes. A [`Production`] adds the relations the rest of the chain
//! points at — shots, storyboard panels, subtitle cues, assets, tasks and
//! delivery targets — and keeps every one of them on a stable identity.
//!
//! Three rules make this book usable as the single source of truth:
//!
//! - Identity is validated on construction *and* on deserialization. A blank,
//!   padded or punctuation-ridden handle is an error instead of a silent alias.
//! - Every cross-reference is resolved by [`Production::validate`]: a cue that
//!   points at a missing shot, or a shot that points at a missing scene, fails
//!   loudly instead of producing an empty storyboard page.
//! - A [`Task`] carries the fingerprint of its inputs and configuration, so
//!   cache freshness is a value comparison. The output files existing is not
//!   evidence that they were produced from the current project.
//!
//! Time inside the book is frame based, like the scene IR; seconds are derived
//! through [`Project::time_base`] and never stored.

use std::collections::{HashMap, HashSet};

use media_types::Error;
use serde::{Deserialize, Serialize};

use crate::scene::Project;

/// The stable handle of one production entity.
///
/// Handles are compared as written, so they may not be padded and may only use
/// characters that survive a JSON key, a file name and a URL fragment. Anything
/// else is rejected by [`entity_id!`] instead of being quietly normalised.
fn check_id(kind: &str, raw: &str) -> Result<(), Error> {
    if raw.is_empty() {
        return Err(Error::invalid_input(format!("{kind} id must not be empty")));
    }
    if raw.trim() != raw {
        return Err(Error::invalid_input(format!("{kind} id `{raw}` must not be padded with whitespace")));
    }
    if !raw.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':')) {
        return Err(Error::invalid_input(format!(
            "{kind} id `{raw}` may only use ASCII letters, digits, `-`, `_`, `.` and `:`"
        )));
    }
    Ok(())
}

macro_rules! entity_id {
    ($name:ident, $kind:literal) => {
        #[doc = concat!("Stable identity of one production ", $kind, ".")]
        #[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            #[doc = concat!("Validates and wraps a ", $kind, " handle.")]
            pub fn new(raw: impl Into<String>) -> Result<Self, Error> {
                let raw = raw.into();
                check_id($kind, &raw)?;
                Ok(Self(raw))
            }

            /// The handle as authored.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = Error;

            fn try_from(raw: String) -> Result<Self, Self::Error> {
                Self::new(raw)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

entity_id!(ShotId, "shot");
entity_id!(PanelId, "panel");
entity_id!(CueId, "cue");
entity_id!(AssetId, "asset");
entity_id!(TaskId, "task");

/// What a referenced file actually is.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AssetKind {
    /// A still image.
    Image,
    /// A video clip.
    Video,
    /// An audio recording or render.
    Audio,
    /// A font file.
    Font,
    /// A 3D or voxel asset.
    Model,
    /// Any other document the work depends on.
    Document,
}

/// Where an asset came from, so a rebuild can tell a source file from a
/// generated or remote artefact.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum AssetSource {
    /// A file the author owns; the path names it directly.
    File,
    /// A recording made for this work.
    Recorded,
    /// Produced by a named engine. The parameters live in the task that made it.
    Generated {
        /// Engine or model identity, e.g. a local engine id or a service name.
        engine: String,
    },
    /// Downloaded from a remote service.
    Remote {
        /// Service identity, e.g. an `atlas.cloud` provider.
        service: String,
    },
}

/// One referenced file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    /// Stable handle other entities reference.
    pub id: AssetId,
    /// Path relative to the production root. Absolute paths and `..` escapes
    /// are rejected: a relocatable work must not depend on one machine layout.
    pub path: String,
    /// What the file is.
    pub kind: AssetKind,
    /// Content digest recorded when the asset entered the work.
    pub digest: String,
    /// How the work obtained it.
    pub source: AssetSource,
}

/// One storyboard panel inside a shot.
///
/// The drawing side (poses, arrows, safe areas) is a later work package; what
/// I0 fixes is that a panel has an identity, belongs to exactly one shot and
/// covers a real frame range.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Panel {
    /// Stable handle.
    pub id: PanelId,
    /// Half-open frame range inside the shot, `[start, end)`.
    pub range: [u32; 2],
    /// Why the shot exists at this moment.
    pub purpose: String,
    /// What the frame is composed of.
    pub composition: String,
    /// Line spoken while the panel is on screen, if any.
    #[serde(default)]
    pub dialogue: Option<String>,
    /// Review notes attached to this panel.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// One shot: a scene plus its panels and lock state.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Shot {
    /// Stable handle that panels, cues and review notes point at.
    pub id: ShotId,
    /// `Scene.id` carrying the picture for this shot.
    pub scene: String,
    /// Panels in storyboard order.
    #[serde(default)]
    pub panels: Vec<Panel>,
    /// A locked shot keeps its content across local revisions; changing it is
    /// an explicit operation rather than an accidental edit.
    #[serde(default)]
    pub locked: bool,
}

/// How a cue's frame range was obtained.
///
/// Estimating from character counts is allowed, but it must be recorded as an
/// estimate: it cannot be signed off as exact alignment.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CueTiming {
    /// Derived from text length or a speaking-rate constant.
    Estimated,
    /// Measured from a real recording or an alignment result.
    Measured {
        /// Where the measurement came from.
        source: String,
    },
    /// Adjusted by hand after measuring.
    Manual {
        /// What it was adjusted from.
        basis: String,
    },
}

/// One subtitle cue.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Cue {
    /// Stable handle shared by the subtitle file, the burn-in and the review
    /// page.
    pub id: CueId,
    /// Shot the cue belongs to.
    pub shot: ShotId,
    /// Half-open frame range, `[start, end)`.
    pub range: [u32; 2],
    /// Text as displayed.
    pub text: String,
    /// Speaker identity, when the work distinguishes voices.
    #[serde(default)]
    pub speaker: Option<String>,
    /// How the range was produced.
    pub timing: CueTiming,
}

/// One production task: what it needs, what it makes and whether a previous
/// result is still current.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Task {
    /// Stable handle, also the dependency key.
    pub id: TaskId,
    /// Capability the task exercises, e.g. `media.render` or `media.voice`.
    pub capability: String,
    /// Fingerprint of the exact inputs (project revision, asset digests,
    /// script text). Empty is rejected: a task without inputs is not cacheable.
    pub input: String,
    /// Fingerprint of the processing configuration.
    pub config: String,
    /// Tasks that must finish first.
    #[serde(default)]
    pub deps: Vec<TaskId>,
    /// Output paths relative to the production root.
    #[serde(default)]
    pub outputs: Vec<String>,
}

impl Task {
    /// Whether a recorded result still matches the current inputs.
    ///
    /// Both fingerprints must match. Checking that `outputs` exist is not
    /// enough: a stale file from an older revision would pass that test.
    #[must_use]
    pub fn is_fresh(&self, input: &str, config: &str) -> bool {
        !self.input.is_empty() && self.input == input && self.config == config
    }

    /// Whether this task has to run again, given its latest recorded attempt.
    #[must_use]
    pub fn needs_run(&self, last: Option<&TaskRun>) -> bool {
        !last.is_some_and(|run| run.is_current(self))
    }
}

/// Lifecycle of one task attempt.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TaskStatus {
    /// Declared but not started.
    Pending,
    /// Started and still running; a run left in this state is an interrupted
    /// attempt, not evidence of success.
    Running,
    /// Finished and published its outputs.
    Succeeded,
    /// Failed and published nothing.
    Failed,
    /// Stopped by the host and published nothing.
    Cancelled,
}

/// A failure reported by a run, using the same stable codes as the core.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskFailure {
    /// Stable code, e.g. `cancelled` or `encode`.
    pub code: String,
    /// Human-readable detail.
    pub message: String,
}

/// One recorded task attempt.
///
/// Publishing is atomic: `outputs` is non-empty only for a succeeded run.
/// A cancelled or failed attempt lists no outputs, because nothing it wrote
/// was promoted over the previous good artifact.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskRun {
    /// Task this attempt belongs to.
    pub task: TaskId,
    /// How the attempt ended.
    pub status: TaskStatus,
    /// Input fingerprint the attempt ran against.
    pub input: String,
    /// Configuration fingerprint the attempt ran against.
    pub config: String,
    /// Published output paths, in the task's own order.
    #[serde(default)]
    pub outputs: Vec<String>,
    /// Failure detail; present only for `failed`.
    #[serde(default)]
    pub failure: Option<TaskFailure>,
}

impl TaskRun {
    /// Records a successful attempt that published `outputs`.
    #[must_use]
    pub fn succeeded(task: &Task, outputs: Vec<String>) -> Self {
        Self { task: task.id.clone(), status: TaskStatus::Succeeded, input: task.input.clone(), config: task.config.clone(), outputs, failure: None }
    }

    /// Records a host cancellation. Nothing is published.
    #[must_use]
    pub fn cancelled(task: &Task) -> Self {
        Self {
            task: task.id.clone(),
            status: TaskStatus::Cancelled,
            input: task.input.clone(),
            config: task.config.clone(),
            outputs: Vec::new(),
            failure: Some(TaskFailure { code: "cancelled".to_owned(), message: "cancelled by the host".to_owned() }),
        }
    }

    /// Records a failure with the core's stable code.
    #[must_use]
    pub fn failed(task: &Task, error: &Error) -> Self {
        Self {
            task: task.id.clone(),
            status: TaskStatus::Failed,
            input: task.input.clone(),
            config: task.config.clone(),
            outputs: Vec::new(),
            failure: Some(TaskFailure { code: error.code().to_owned(), message: error.message.clone() }),
        }
    }

    /// Whether this attempt is a usable result for `task` as it stands now.
    ///
    /// Succeeded, same fingerprints, same published outputs. A run that merely
    /// started (`running`) or ended without publishing is never current.
    #[must_use]
    pub fn is_current(&self, task: &Task) -> bool {
        self.status == TaskStatus::Succeeded && self.input == task.input && self.config == task.config && self.outputs == task.outputs
    }
}

/// One delivery target.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Delivery {
    /// Name used by the CLI and by the report, e.g. `master` or `web`.
    pub name: String,
    /// Container the work is packaged in, e.g. `mp4`.
    pub container: String,
    /// Video codec the target expects. An encoder that cannot produce it must
    /// fail instead of substituting another codec.
    #[serde(default)]
    pub video: Option<String>,
    /// Audio codec the target expects.
    #[serde(default)]
    pub audio: Option<String>,
    /// Subtitle format carried by the target, when subtitles are delivered.
    #[serde(default)]
    pub subtitle: Option<String>,
}

/// The single source of truth for one work.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Production {
    /// Render truth: scenes, items, camera and design.
    pub project: Project,
    /// Referenced files.
    #[serde(default)]
    pub assets: Vec<Asset>,
    /// Shots, in storyboard order.
    #[serde(default)]
    pub shots: Vec<Shot>,
    /// Subtitle cues, in time order.
    #[serde(default)]
    pub cues: Vec<Cue>,
    /// Tasks that can be resumed or reused.
    #[serde(default)]
    pub tasks: Vec<Task>,
    /// Recorded attempts, oldest first. The last entry for a task decides
    /// whether it still has to run.
    #[serde(default)]
    pub runs: Vec<TaskRun>,
    /// Delivery targets.
    #[serde(default)]
    pub delivery: Vec<Delivery>,
}

impl Production {
    /// Wraps a render project with no production metadata yet.
    #[must_use]
    pub fn new(project: Project) -> Self {
        Self { project, assets: Vec::new(), shots: Vec::new(), cues: Vec::new(), tasks: Vec::new(), runs: Vec::new(), delivery: Vec::new() }
    }

    /// Tasks whose latest attempt does not match the declared inputs.
    ///
    /// Order follows declaration, so a host can run them in the order the
    /// author wrote. Dependency satisfaction is not evaluated here: a task
    /// whose dependency is also pending will simply appear in the list too.
    #[must_use]
    pub fn pending_tasks(&self) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|task| {
                let last = self.runs.iter().rev().find(|run| run.task == task.id);
                task.needs_run(last)
            })
            .collect()
    }

    /// Tasks whose latest attempt is a current success.
    #[must_use]
    pub fn fresh_tasks(&self) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|task| {
                let last = self.runs.iter().rev().find(|run| run.task == task.id);
                last.is_some_and(|run| run.is_current(task))
            })
            .collect()
    }

    /// Checks identity, references, ranges and task dependencies.
    ///
    /// Every problem is reported, not just the first one: a broken production
    /// usually has several, and fixing them one error per run is how stale
    /// references survive.
    pub fn validate(&self) -> Result<(), Error> {
        let problems = self.problems();
        if problems.is_empty() {
            Ok(())
        }
        else {
            Err(Error::invalid_input(problems.join("\n")))
        }
    }

    /// The problems [`Production::validate`] would report, one per entry.
    ///
    /// Hosts expose this list directly so a review page can point at the exact
    /// broken reference instead of a joined message.
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let scenes = self.scene_index(&mut problems);
        self.check_ids(&mut problems);
        self.check_assets(&mut problems);
        self.check_shots(&scenes, &mut problems);
        self.check_cues(&scenes, &mut problems);
        self.check_tasks(&mut problems);
        self.check_delivery(&mut problems);
        problems
    }

    /// Scene identities keyed for shot lookup, reporting blanks and duplicates.
    ///
    /// A scene only needs an id when something can reference it. Films that do
    /// not declare shots yet keep working, but once a shot exists an unnamed
    /// scene is a problem instead of an unreachable block.
    fn scene_index(&self, problems: &mut Vec<String>) -> HashMap<&str, usize> {
        let mut index = HashMap::new();
        let referenced = !self.shots.is_empty();
        for (position, scene) in self.project.scenes.iter().enumerate() {
            let Some(id) = scene.id.as_deref() else {
                if referenced {
                    problems.push(format!("scenes[{position}] has no id, so no shot can reference it"));
                }
                continue;
            };
            if id.trim().is_empty() {
                problems.push(format!("scenes[{position}] id must not be empty"));
                continue;
            }
            if index.insert(id, position).is_some() {
                problems.push(format!("duplicate scene id `{id}`"));
            }
        }
        index
    }

    fn check_ids(&self, problems: &mut Vec<String>) {
        let mut shots = HashSet::new();
        for shot in &self.shots {
            if !shots.insert(shot.id.clone()) {
                problems.push(format!("duplicate shot id `{}`", shot.id));
            }
            let mut panels = HashSet::new();
            for panel in &shot.panels {
                if !panels.insert(panel.id.clone()) {
                    problems.push(format!("duplicate panel id `{}` in shot `{}`", panel.id, shot.id));
                }
                if panel.range[0] >= panel.range[1] {
                    problems.push(format!("panel `{}` has an empty frame range {:?}", panel.id, panel.range));
                }
            }
        }

        let mut cues = HashSet::new();
        for cue in &self.cues {
            if !cues.insert(cue.id.clone()) {
                problems.push(format!("duplicate cue id `{}`", cue.id));
            }
            if cue.range[0] >= cue.range[1] {
                problems.push(format!("cue `{}` has an empty frame range {:?}", cue.id, cue.range));
            }
            if cue.text.trim().is_empty() {
                problems.push(format!("cue `{}` has no text", cue.id));
            }
        }

        let mut assets = HashSet::new();
        for asset in &self.assets {
            if !assets.insert(asset.id.clone()) {
                problems.push(format!("duplicate asset id `{}`", asset.id));
            }
        }

        let mut tasks = HashSet::new();
        for task in &self.tasks {
            if !tasks.insert(task.id.clone()) {
                problems.push(format!("duplicate task id `{}`", task.id));
            }
        }
    }

    fn check_assets(&self, problems: &mut Vec<String>) {
        for asset in &self.assets {
            if asset.path.trim().is_empty() {
                problems.push(format!("asset `{}` has no path", asset.id));
            }
            if asset.path.starts_with('/') || asset.path.contains(":\\") || asset.path.starts_with("\\\\") {
                problems.push(format!("asset `{}` path `{}` must be relative to the production root", asset.id, asset.path));
            }
            if asset.path.split(['/', '\\']).any(|segment| segment == "..") {
                problems.push(format!("asset `{}` path `{}` escapes the production root", asset.id, asset.path));
            }
            if asset.digest.trim().is_empty() {
                problems.push(format!("asset `{}` has no digest", asset.id));
            }
        }
    }

    fn check_shots(&self, scenes: &HashMap<&str, usize>, problems: &mut Vec<String>) {
        for shot in &self.shots {
            let Some(&position) = scenes.get(shot.scene.as_str()) else {
                problems.push(format!("shot `{}` references unknown scene `{}`", shot.id, shot.scene));
                continue;
            };
            let duration = self.project.scenes[position].duration;
            for panel in &shot.panels {
                if panel.range[1] > duration {
                    problems.push(format!(
                        "panel `{}` ends at frame {} but scene `{}` is {duration} frames long",
                        panel.id, panel.range[1], shot.scene
                    ));
                }
            }
        }
    }

    fn check_cues(&self, scenes: &HashMap<&str, usize>, problems: &mut Vec<String>) {
        let shots: HashMap<&str, &Shot> = self.shots.iter().map(|shot| (shot.id.as_str(), shot)).collect();
        for cue in &self.cues {
            let Some(shot) = shots.get(cue.shot.as_str()) else {
                problems.push(format!("cue `{}` references unknown shot `{}`", cue.id, cue.shot));
                continue;
            };
            let Some(&position) = scenes.get(shot.scene.as_str()) else {
                // The shot itself already reports the missing scene.
                continue;
            };
            let duration = self.project.scenes[position].duration;
            if cue.range[1] > duration {
                problems.push(format!(
                    "cue `{}` ends at frame {} but shot `{}` covers {duration} frames",
                    cue.id, cue.range[1], shot.id
                ));
            }
        }
    }

    fn check_tasks(&self, problems: &mut Vec<String>) {
        let known: HashSet<&str> = self.tasks.iter().map(|task| task.id.as_str()).collect();
        let mut outputs: HashMap<&str, &TaskId> = HashMap::new();
        for task in &self.tasks {
            if task.capability.trim().is_empty() {
                problems.push(format!("task `{}` declares no capability", task.id));
            }
            if task.input.trim().is_empty() {
                problems.push(format!("task `{}` declares no input fingerprint", task.id));
            }
            for dep in &task.deps {
                if !known.contains(dep.as_str()) {
                    problems.push(format!("task `{}` depends on unknown task `{dep}`", task.id));
                }
            }
            for output in &task.outputs {
                if output.trim().is_empty() {
                    problems.push(format!("task `{}` declares an empty output path", task.id));
                }
                if let Some(owner) = outputs.insert(output, &task.id) {
                    problems.push(format!("tasks `{owner}` and `{}` both write `{output}`", task.id));
                }
            }
        }
        let by_id: HashMap<&str, &Task> = self.tasks.iter().map(|task| (task.id.as_str(), task)).collect();
        for (position, run) in self.runs.iter().enumerate() {
            let Some(task) = by_id.get(run.task.as_str()) else {
                problems.push(format!("runs[{position}] references unknown task `{}`", run.task));
                continue;
            };
            if !run.outputs.is_empty() && run.status != TaskStatus::Succeeded {
                problems.push(format!("run for `{}` is {:?} and must not publish outputs", run.task, run.status));
            }
            match run.status {
                TaskStatus::Succeeded => {
                    if run.outputs != task.outputs {
                        problems.push(format!(
                            "run for `{}` published {:?} but the task declares {:?}",
                            run.task, run.outputs, task.outputs
                        ));
                    }
                    if run.failure.is_some() {
                        problems.push(format!("run for `{}` succeeded but carries a failure", run.task));
                    }
                }
                TaskStatus::Failed => {
                    if run.failure.as_ref().is_none_or(|failure| failure.code.trim().is_empty()) {
                        problems.push(format!("run for `{}` failed without a stable code", run.task));
                    }
                }
                TaskStatus::Pending | TaskStatus::Running | TaskStatus::Cancelled => {}
            }
        }

        if let Some(cycle) = task_cycle(&self.tasks) {
            problems.push(format!("task dependency cycle: {}", cycle.join(" -> ")));
        }
    }

    fn check_delivery(&self, problems: &mut Vec<String>) {
        let mut names = HashSet::new();
        for delivery in &self.delivery {
            if delivery.name.trim().is_empty() {
                problems.push("a delivery target has no name".to_owned());
                continue;
            }
            if !names.insert(delivery.name.as_str()) {
                problems.push(format!("duplicate delivery target `{}`", delivery.name));
            }
            if delivery.container.trim().is_empty() {
                problems.push(format!("delivery `{}` declares no container", delivery.name));
            }
        }
    }
}

/// Finds one dependency cycle, if any, as a readable path.
fn task_cycle(tasks: &[Task]) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Open,
        Done,
    }

    fn walk(
        current: &str,
        edges: &HashMap<&str, &[TaskId]>,
        marks: &mut HashMap<String, Mark>,
        path: &mut Vec<String>,
    ) -> Option<Vec<String>> {
        match marks.get(current) {
            Some(Mark::Done) => return None,
            Some(Mark::Open) => {
                let at = path.iter().position(|id| id == current).unwrap_or(0);
                let mut cycle = path[at..].to_vec();
                cycle.push(current.to_owned());
                return Some(cycle);
            }
            None => {}
        }
        marks.insert(current.to_owned(), Mark::Open);
        path.push(current.to_owned());
        for dep in edges.get(current).copied().unwrap_or(&[]) {
            if let Some(cycle) = walk(dep.as_str(), edges, marks, path) {
                return Some(cycle);
            }
        }
        path.pop();
        marks.insert(current.to_owned(), Mark::Done);
        None
    }

    let edges: HashMap<&str, &[TaskId]> = tasks.iter().map(|task| (task.id.as_str(), task.deps.as_slice())).collect();
    let mut marks = HashMap::new();
    for task in tasks {
        if let Some(cycle) = walk(task.id.as_str(), &edges, &mut marks, &mut Vec::new()) {
            return Some(cycle);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn project_with(two_scenes: bool) -> Project {
        let mechanism = json!({
            "id": "mechanism",
            "start": 0,
            "duration": 90,
            "background": { "kind": "solid", "color": "#0b1220" },
            "items": []
        });
        let spatial = json!({
            "id": "spatial",
            "start": 90,
            "duration": 120,
            "background": { "kind": "solid", "color": "#101820" },
            "items": []
        });
        let scenes = if two_scenes { vec![mechanism, spatial] } else { vec![mechanism] };
        serde_json::from_value(json!({
            "width": 1920, "height": 1080, "fps": 30, "duration_frames": 210,
            "scenes": scenes,
            "audio": null
        }))
        .expect("project")
    }

    #[test]
    fn abstract_and_spatial_scenes_share_one_book() {
        let production = Production {
            project: project_with(true),
            assets: vec![Asset {
                id: AssetId::new("bg-deck").unwrap(),
                path: "assets/deck.png".to_owned(),
                kind: AssetKind::Image,
                digest: "sha256:aa".to_owned(),
                source: AssetSource::File,
            }],
            shots: vec![
                Shot {
                    id: ShotId::new("shot-mechanism").unwrap(),
                    scene: "mechanism".to_owned(),
                    locked: false,
                    panels: vec![Panel {
                        id: PanelId::new("panel-1").unwrap(),
                        range: [0, 30],
                        purpose: "show the request entering".to_owned(),
                        composition: "node graph, left to right".to_owned(),
                        dialogue: Some("先看请求怎么进来".to_owned()),
                        notes: Vec::new(),
                    }],
                },
                Shot {
                    id: ShotId::new("shot-spatial").unwrap(),
                    scene: "spatial".to_owned(),
                    locked: true,
                    panels: vec![Panel {
                        id: PanelId::new("panel-2").unwrap(),
                        range: [0, 60],
                        purpose: "establish the room".to_owned(),
                        composition: "wide, foreground props".to_owned(),
                        dialogue: None,
                        notes: vec!["keep the window light".to_owned()],
                    }],
                },
            ],
            cues: vec![Cue {
                id: CueId::new("cue-1").unwrap(),
                shot: ShotId::new("shot-mechanism").unwrap(),
                range: [0, 30],
                text: "先看请求怎么进来".to_owned(),
                speaker: Some("narration".to_owned()),
                timing: CueTiming::Estimated,
            }],
            tasks: vec![Task {
                id: TaskId::new("render").unwrap(),
                capability: "media.render".to_owned(),
                input: "project:1".to_owned(),
                config: "h264/aac".to_owned(),
                deps: Vec::new(),
                outputs: vec!["out/master.mp4".to_owned()],
            }],
            runs: Vec::new(),
            delivery: vec![Delivery {
                name: "master".to_owned(),
                container: "mp4".to_owned(),
                video: Some("h264".to_owned()),
                audio: Some("aac".to_owned()),
                subtitle: Some("srt".to_owned()),
            }],
        };
        production.validate().expect("both scenes are representable");

        let wire = serde_json::to_string(&production).expect("serialize");
        let back: Production = serde_json::from_str(&wire).expect("deserialize");
        assert_eq!(back.shots.len(), 2);
        assert_eq!(back.cues[0].shot.as_str(), "shot-mechanism");
        assert_eq!(back.assets[0].path, "assets/deck.png");
    }

    #[test]
    fn a_film_without_shots_needs_no_scene_ids() {
        let mut project = project_with(false);
        project.scenes[0].id = None;
        Production::new(project).validate().expect("a render-only film stays valid");
    }

    #[test]
    fn unknown_references_are_reported() {
        let mut production = Production::new(project_with(false));
        production.shots.push(Shot {
            id: ShotId::new("shot-a").unwrap(),
            scene: "missing".to_owned(),
            locked: false,
            panels: Vec::new(),
        });
        production.cues.push(Cue {
            id: CueId::new("cue-a").unwrap(),
            shot: ShotId::new("shot-nowhere").unwrap(),
            range: [0, 10],
            text: "hello".to_owned(),
            speaker: None,
            timing: CueTiming::Measured { source: "recording".to_owned() },
        });
        let error = production.validate().expect_err("broken references must fail");
        assert!(error.message.contains("unknown scene `missing`"), "{}", error.message);
        assert!(error.message.contains("unknown shot `shot-nowhere`"), "{}", error.message);
    }

    #[test]
    fn ids_are_validated_on_construction_and_deserialization() {
        assert!(ShotId::new(" shot-a").is_err(), "padded ids alias another handle");
        assert!(ShotId::new("shot a").is_err(), "spaces do not survive a path or key");
        let empty = serde_json::from_value::<ShotId>(json!("")).expect_err("deserialization validates too");
        assert!(empty.to_string().contains("must not be empty"), "{empty}");
        let padded = serde_json::from_value::<ShotId>(json!(" ")).expect_err("padded handles are rejected");
        assert!(padded.to_string().contains("must not be padded"), "{padded}");
    }

    #[test]
    fn asset_paths_stay_inside_the_production_root() {
        let mut production = Production::new(project_with(false));
        production.assets.push(Asset {
            id: AssetId::new("leak").unwrap(),
            path: "../elsewhere/secret.png".to_owned(),
            kind: AssetKind::Image,
            digest: "sha256:bb".to_owned(),
            source: AssetSource::File,
        });
        production.assets.push(Asset {
            id: AssetId::new("absolute").unwrap(),
            path: "E:\\media\\deck.png".to_owned(),
            kind: AssetKind::Image,
            digest: "sha256:cc".to_owned(),
            source: AssetSource::File,
        });
        let error = production.validate().expect_err("escaping paths must fail");
        assert!(error.message.contains("escapes the production root"), "{}", error.message);
        assert!(error.message.contains("must be relative"), "{}", error.message);
    }

    #[test]
    fn task_cycles_and_output_collisions_are_reported() {
        let mut production = Production::new(project_with(false));
        let task = |id: &str, deps: Vec<&str>, output: &str| Task {
            id: TaskId::new(id).unwrap(),
            capability: "media.render".to_owned(),
            input: "project:1".to_owned(),
            config: "default".to_owned(),
            deps: deps.into_iter().map(|dep| TaskId::new(dep).unwrap()).collect(),
            outputs: vec![output.to_owned()],
        };
        production.tasks.push(task("a", vec!["b"], "out/a.mp4"));
        production.tasks.push(task("b", vec!["a"], "out/a.mp4"));
        let error = production.validate().expect_err("cycles and collisions must fail");
        assert!(error.message.contains("dependency cycle"), "{}", error.message);
        assert!(error.message.contains("both write `out/a.mp4`"), "{}", error.message);
    }

    #[test]
    fn only_a_current_success_clears_a_task() {
        let mut production = Production::new(project_with(false));
        let task = Task {
            id: TaskId::new("render").unwrap(),
            capability: "media.render".to_owned(),
            input: "project:1".to_owned(),
            config: "h264/aac".to_owned(),
            deps: Vec::new(),
            outputs: vec!["out/master.mp4".to_owned()],
        };
        production.tasks.push(task.clone());
        assert_eq!(production.pending_tasks().len(), 1, "a declared task with no run is pending");

        production.runs.push(TaskRun::cancelled(&task));
        assert_eq!(production.pending_tasks().len(), 1, "a cancelled run publishes nothing");
        assert!(production.fresh_tasks().is_empty());

        production.runs.push(TaskRun::succeeded(&task, task.outputs.clone()));
        production.validate().expect("a complete run is a valid record");
        assert!(production.pending_tasks().is_empty(), "the latest attempt is current");
        assert_eq!(production.fresh_tasks().len(), 1);

        production.tasks[0].input = "project:2".to_owned();
        assert_eq!(production.pending_tasks().len(), 1, "a newer input re-pends the task");
    }

    #[test]
    fn a_run_cannot_claim_outputs_it_did_not_publish() {
        let mut production = Production::new(project_with(false));
        let task = Task {
            id: TaskId::new("render").unwrap(),
            capability: "media.render".to_owned(),
            input: "project:1".to_owned(),
            config: "h264/aac".to_owned(),
            deps: Vec::new(),
            outputs: vec!["out/master.mp4".to_owned()],
        };
        production.tasks.push(task);
        production.runs.push(TaskRun::succeeded(&production.tasks[0], vec!["out/other.mp4".to_owned()]));
        production.runs.push(TaskRun {
            task: TaskId::new("ghost").unwrap(),
            ..TaskRun::cancelled(&production.tasks[0])
        });
        production.runs.push(TaskRun { status: TaskStatus::Failed, failure: None, ..TaskRun::cancelled(&production.tasks[0]) });

        let problems = production.problems();
        assert!(problems.iter().any(|p| p.contains("but the task declares")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("unknown task `ghost`")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("failed without a stable code")), "{problems:?}");
    }

    #[test]
    fn cache_freshness_compares_fingerprints_not_files() {
        let task = Task {
            id: TaskId::new("render").unwrap(),
            capability: "media.render".to_owned(),
            input: "project:1".to_owned(),
            config: "h264/aac".to_owned(),
            deps: Vec::new(),
            outputs: vec!["out/master.mp4".to_owned()],
        };
        assert!(task.is_fresh("project:1", "h264/aac"));
        assert!(!task.is_fresh("project:2", "h264/aac"), "a newer project revision invalidates the cache");
        assert!(!task.is_fresh("project:1", "h264/flac"), "a changed delivery config invalidates the cache");
        let anonymous = Task { input: String::new(), ..task };
        assert!(!anonymous.is_fresh("", "h264/aac"), "a task without inputs is never fresh");
    }
}
