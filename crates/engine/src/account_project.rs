//! The native project in a WebOS account window. Storage and its revision are owned by the host.

use std::sync::Arc;

use figcraft_doc::{Document, NodeId};
use serde_json::{Value, json};

use crate::{DocState, EngineError, Result, Session, branches, cmd};

pub const MAX_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectState {
    pub document: u64,
    pub revision: u64,
    pub dirty: bool,
    pub capturable: bool,
    pub autosave_eligible: bool,
    pub operation_pending: bool,
    pub unfinished_edit: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveKey {
    pub ticket: u64,
    pub document: u64,
    pub revision: u64,
    pub path: String,
}

pub struct PreparedSave {
    pub key: SaveKey,
    pub bytes: Vec<u8>,
}

struct CapturedTab {
    uid: u64,
    revision: u64,
    doc: Arc<Document>,
}

struct PendingSave {
    key: SaveKey,
    tabs: Vec<CapturedTab>,
}

#[derive(Default)]
pub struct AccountProject {
    document: Option<u64>,
    path: Option<String>,
    revision: u64,
    stamp: Vec<(u64, u64)>,
    last_ticket: u64,
    pending: Option<PendingSave>,
    repaired: bool,
}

fn rejected(message: impl Into<String>) -> EngineError {
    EngineError::Other(message.into())
}

pub fn valid_path(path: &str) -> bool {
    path.len() <= 2048
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path.split('/').all(|part| !part.is_empty() && !matches!(part, "." | ".."))
        && path.rsplit_once('.').is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("figcraft"))
}

fn bounded(bytes: Vec<u8>) -> Result<Vec<u8>> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return Err(rejected("The FigCraft project exceeds the account document limit."));
    }
    Ok(bytes)
}

impl AccountProject {
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// Admit one project and any native branch tabs it subsequently creates. A failed open leaves
    /// the session empty and retryable by the host.
    pub fn start(
        &mut self,
        session: &mut Session,
        path: Option<String>,
        bytes: Option<&[u8]>,
        recover: bool,
        settings: Option<&Value>,
    ) -> Result<()> {
        if self.document.is_some() || !session.docs.is_empty() {
            return Err(rejected("This FigCraft window already has a project."));
        }
        if path.as_deref().is_some_and(|p| !valid_path(p))
            || path.is_some() && bytes.is_none()
            || recover && bytes.is_none()
            || bytes.is_some() && settings.is_some()
        {
            return Err(rejected("Invalid FigCraft account project."));
        }
        if let Some(bytes) = bytes {
            if bytes.is_empty() || bytes.len() > MAX_BYTES {
                return Err(rejected("The FigCraft project exceeds the account document limit."));
            }
            let loaded = figcraft_format::load_bytes_with_versions(bytes).map_err(|error| rejected(error.to_string()))?;
            self.repaired = loaded.images_past_budget != 0 || !loaded.issues.is_empty();
            if self.repaired {
                session.message = Some("FigCraft repaired or omitted parts of this file while opening it. Save a copy to keep the original.".into());
            }
            // Preserve the saved page colour even when the host opens before the first UI frame.
            session.ui_dark.get_or_insert(false);
            let tab = DocState::new(loaded.doc, None).with_versions(loaded.versions).with_branches(loaded.branches);
            session.docs.push(if recover { tab.unsaved() } else { tab });
        } else {
            let settings = settings.cloned().unwrap_or_else(|| json!({}));
            let settings = settings.as_object().ok_or_else(|| rejected("Invalid FigCraft project settings."))?;
            let kind = settings.get("kind").and_then(Value::as_str).unwrap_or("design");
            let command = match kind {
                "design" => "file.new",
                "board" => "file.newBoard",
                "template" => "file.newFromTemplate",
                _ => return Err(rejected("Unsupported new FigCraft project type.")),
            };
            let mut params = match settings.get("name").and_then(Value::as_str) {
                Some(name) if valid_name(name) => json!({"name": name}),
                Some(_) => return Err(rejected("Invalid FigCraft project name.")),
                None => json!({}),
            };
            if kind == "template" {
                let id = settings
                    .get("template")
                    .and_then(Value::as_str)
                    .filter(|id| valid_name(id))
                    .ok_or_else(|| rejected("Choose a FigCraft template."))?;
                params["template"] = json!(id);
            }
            session.execute(command, &params)?;
            if kind == "template" {
                let tab = session.ds_mut()?;
                tab.saved_doc = Arc::new(Document::new(&tab.doc.name));
            }
        }
        self.path = path;
        self.document = Some(session.ds()?.uid);
        self.state(session, false).ok_or_else(|| rejected("The FigCraft project is unavailable."))?;
        Ok(())
    }

    fn root<'a>(&self, session: &'a Session) -> Option<&'a DocState> {
        let document = self.document?;
        session.docs.iter().find(|tab| tab.uid == document && tab.branch_of.is_none())
    }

    pub fn state(&mut self, session: &Session, ui_busy: bool) -> Option<ProjectState> {
        let root = self.root(session)?;
        let document = root.uid;
        // Each branch tab has its own revision. Include it even when the root revision did not
        // move, and retain a monotonic aggregate across branch opening, switching and closing.
        let tabs = || std::iter::once(root).chain(branches::tabs(session, document));
        let stamp: Vec<_> = tabs().map(|tab| (tab.uid, tab.revision)).collect();
        if stamp != self.stamp {
            self.revision = self.revision.checked_add(1)?;
            self.stamp = stamp;
        }
        let unfinished_edit = tabs()
            .any(|tab| tab.interaction.is_some() || tab.text_preview.is_some() || tab.text_edit.as_ref().is_some_and(|text| text.preedit.is_some()));
        let foreign_tab = tabs().count() != session.docs.len();
        let dirty = branches::needs_copy(session, root);
        let operation_pending = self.pending.is_some() || ui_busy || foreign_tab;
        Some(ProjectState {
            document,
            revision: self.revision,
            dirty,
            capturable: !unfinished_edit && !foreign_tab,
            autosave_eligible: self.path.is_some()
                && !self.repaired
                && !operation_pending
                && !unfinished_edit
                && tabs().all(|tab| tab.text_edit.is_none()),
            operation_pending,
            unfinished_edit,
        })
    }

    pub fn capture_recovery(&mut self, session: &Session, document: u64, revision: u64) -> Result<Vec<u8>> {
        self.state(session, false)
            .filter(|state| state.document == document && state.revision == revision && state.dirty && state.capturable)
            .ok_or_else(|| rejected("The FigCraft recovery revision changed or an edit is still active."))?;
        self.encode(session, false)
    }

    fn encode(&self, session: &Session, thumbnail: bool) -> Result<Vec<u8>> {
        let root = self.root(session).ok_or(EngineError::NoDocument)?;
        let (mut doc, versions, branches) = branches::recovery_copy(session, root);
        if root.preview.is_none() && doc.pages.contains(&root.page) {
            Arc::make_mut(&mut doc).current_page = Some(root.page);
        }
        let thumb = thumbnail.then(|| cmd::file::thumbnail(&doc)).flatten();
        bounded(figcraft_format::save_bytes_with_history(&doc, thumb.as_deref(), &versions, &branches).map_err(|error| rejected(error.to_string()))?)
    }

    pub fn prepare_save(&mut self, session: &Session, document: u64, revision: u64, path: String) -> Result<PreparedSave> {
        if !valid_path(&path) {
            return Err(rejected("Choose an account-relative .figcraft file."));
        }
        if self.repaired && self.path.as_deref() == Some(path.as_str()) {
            return Err(rejected("Save this repaired FigCraft project under a new name."));
        }
        let state = self.state(session, false).ok_or(EngineError::NoDocument)?;
        if state.document != document || state.revision != revision || state.operation_pending || state.unfinished_edit {
            return Err(rejected("The FigCraft project changed or another operation is still active."));
        }
        let bytes = self.encode(session, true)?;
        let ticket = self.last_ticket.checked_add(1).ok_or_else(|| rejected("The FigCraft save ticket limit was reached."))?;
        self.last_ticket = ticket;
        let key = SaveKey { ticket, document, revision, path };
        let root = self.root(session).ok_or(EngineError::NoDocument)?;
        let tabs = std::iter::once(root)
            .chain(branches::tabs(session, document))
            .map(|tab| CapturedTab { uid: tab.uid, revision: tab.revision, doc: tab.document_itself().clone() })
            .collect();
        self.pending = Some(PendingSave { key: key.clone(), tabs });
        Ok(PreparedSave { key, bytes })
    }

    /// Adopt only the bytes confirmed for this pending write. A failed write releases its ticket.
    /// An uncertain write retains its ticket until an exact outcome is known or the view reopens.
    pub fn settle_save(&mut self, session: &mut Session, key: &SaveKey, outcome: SaveOutcome) -> Result<bool> {
        if !self.pending.as_ref().is_some_and(|pending| &pending.key == key) || self.root(session).is_none() {
            return Err(rejected("The FigCraft save result does not match its pending write."));
        }
        if outcome == SaveOutcome::Unknown {
            return Ok(false);
        }
        let pending = self.pending.take().ok_or_else(|| rejected("The FigCraft save result is no longer pending."))?;
        if outcome == SaveOutcome::Failed {
            return Ok(false);
        }
        // A branch edit made since serialization must keep its unsaved baseline even when the
        // root revision did not change. Synchronize branch heads before resetting the baselines.
        let unchanged = self.state(session, false).is_some_and(|state| state.revision == key.revision);
        branches::sync(session);
        for saved in pending.tabs {
            if let Some(tab) = session.docs.iter_mut().find(|tab| tab.uid == saved.uid) {
                tab.saved_doc = saved.doc;
                if tab.revision == saved.revision && (tab.uid != key.document || unchanged) {
                    tab.versions_changed = false;
                }
            }
        }
        self.path = Some(key.path.clone());
        self.repaired = false;
        Ok(true)
    }

    /// A delegated rename runs the command used by the Layers panel and retains its undo step.
    pub fn operation(
        &mut self,
        session: &mut Session,
        document: u64,
        path: Option<&str>,
        command: &str,
        args: &Value,
        ui_busy: bool,
    ) -> Result<Value> {
        let state = self.state(session, ui_busy).ok_or(EngineError::NoDocument)?;
        if state.document != document || self.path() != path {
            return Err(rejected("The FigCraft account binding changed."));
        }
        let args = args.as_object().ok_or_else(|| rejected("Invalid FigCraft operation arguments."))?;
        match command {
            "document.inspect" if args.is_empty() => {
                let root = self.root(session).ok_or(EngineError::NoDocument)?;
                let doc = root.document_itself();
                let layers: Vec<_> = doc
                    .nodes()
                    .filter(|node| !node.is_page())
                    .take(257)
                    .map(|node| json!({"id": node.id.0.to_string(), "name": node.name}))
                    .collect();
                Ok(json!({"name": doc.name, "revision": state.revision.to_string(), "dirty": state.dirty,
                    "pages": doc.pages.len(), "versions": root.versions.len(), "branches": root.branches.len(),
                    "layers": &layers[..layers.len().min(256)], "truncated": layers.len() > 256}))
            }
            "layer.rename" if args.len() == 2 => {
                let root = session.ds()?;
                if state.operation_pending || state.unfinished_edit || root.uid != document || root.preview.is_some() || root.text_edit.is_some() {
                    return Err(rejected("Finish the current FigCraft edit and return to the account project first."));
                }
                let layer = args
                    .get("layerId")
                    .and_then(Value::as_str)
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(NodeId)
                    .filter(|id| root.doc.node(*id).is_some_and(|node| !node.is_page()))
                    .ok_or_else(|| rejected("Unknown FigCraft layer."))?;
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| valid_name(name))
                    .ok_or_else(|| rejected("Choose a valid FigCraft layer name."))?;
                session.execute("object.rename", &json!({"ids": [layer], "name": name}))?;
                let revision = self.state(session, false).ok_or(EngineError::NoDocument)?.revision;
                Ok(json!({"layerId": layer.0.to_string(), "name": name, "revision": revision.to_string()}))
            }
            _ => Err(rejected("Unsupported FigCraft document operation.")),
        }
    }
}

fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= 256 && !name.chars().any(char::is_control)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    Written,
    Failed,
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank() -> (Session, AccountProject) {
        let mut session = Session::empty();
        let mut project = AccountProject::default();
        project.start(&mut session, None, None, false, None).unwrap();
        (session, project)
    }

    fn rectangle(session: &mut Session) -> u64 {
        session.execute("shape.rectangle", &json!({"x": 1.0, "y": 2.0, "width": 16.0, "height": 16.0})).unwrap();
        session.selection()[0].0
    }

    fn prepare(session: &Session, project: &mut AccountProject, path: &str) -> PreparedSave {
        let state = project.state(session, false).unwrap();
        project.prepare_save(session, state.document, state.revision, path.into()).unwrap()
    }

    #[test]
    fn account_project_waits_for_exact_success_and_keeps_later_edits_dirty() {
        let (mut session, mut project) = blank();
        assert!(!project.state(&session, false).unwrap().dirty);
        let blank_save = prepare(&session, &mut project, "Projects/Blank.figcraft");
        assert!(figcraft_format::load_bytes_with_versions(&blank_save.bytes).is_ok());
        assert_eq!(project.path(), None);
        assert!(project.state(&session, false).unwrap().operation_pending);
        let mut foreign = blank_save.key.clone();
        foreign.path = "Other/Blank.figcraft".into();
        assert!(project.settle_save(&mut session, &foreign, SaveOutcome::Written).is_err());
        rectangle(&mut session);
        assert!(project.settle_save(&mut session, &blank_save.key, SaveOutcome::Written).unwrap());
        assert_eq!(project.path(), Some("Projects/Blank.figcraft"));
        assert!(project.state(&session, false).unwrap().dirty);
        assert!(project.settle_save(&mut session, &blank_save.key, SaveOutcome::Written).is_err());

        let copy = prepare(&session, &mut project, "Projects/Copy.figcraft");
        assert!(!project.settle_save(&mut session, &copy.key, SaveOutcome::Failed).unwrap());
        assert_eq!(project.path(), Some("Projects/Blank.figcraft"));
        assert!(project.state(&session, false).unwrap().dirty);
        let uncertain = prepare(&session, &mut project, "Projects/Copy.figcraft");
        assert!(!project.settle_save(&mut session, &uncertain.key, SaveOutcome::Unknown).unwrap());
        let state = project.state(&session, false).unwrap();
        assert!(state.operation_pending && state.dirty);
        assert!(project.prepare_save(&session, state.document, state.revision, "Projects/New.figcraft".into()).is_err());
    }

    #[test]
    fn account_project_keeps_the_loaded_blank_page_colour() {
        let (session, mut project) = blank();
        let saved = prepare(&session, &mut project, "Existing.figcraft");
        let mut session = Session::empty();
        let mut project = AccountProject::default();
        project.start(&mut session, Some(saved.key.path), Some(&saved.bytes), false, None).unwrap();
        let before = session.doc().unwrap().node(session.ds().unwrap().page).unwrap().kind.clone();
        session.set_ui_dark(true);
        assert_eq!(session.doc().unwrap().node(session.ds().unwrap().page).unwrap().kind, before);
    }

    #[test]
    fn account_project_recovery_keeps_current_branch_and_version_history() {
        use base64::Engine as _;

        let (mut session, mut project) = blank();
        rectangle(&mut session);
        session.execute("file.saveVersion", &json!({"name": "Main checkpoint"})).unwrap();
        let branch = session.execute("branch.create", &json!({"name": "Rough design"})).unwrap();
        let state = project.state(&session, false).unwrap();
        let layer = rectangle(&mut session);
        let mut image = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255])).write_to(&mut image, image::ImageFormat::Png).unwrap();
        let data = base64::engine::general_purpose::STANDARD.encode(image.get_ref());
        session.execute("image.place", &json!({"data": data, "x": 25, "y": 25})).unwrap();
        session.execute("file.saveVersion", &json!({"name": "Branch checkpoint"})).unwrap();
        assert!(project.capture_recovery(&session, state.document, state.revision).is_err());
        let state = project.state(&session, false).unwrap();
        let bytes = project.capture_recovery(&session, state.document, state.revision).unwrap();
        let reopened = figcraft_format::load_bytes_with_versions(&bytes).unwrap();
        assert_eq!(reopened.versions.len(), 1);
        assert_eq!(reopened.branches[0].id, branch["id"].as_str().unwrap());
        let branch_versions: Vec<_> = reopened.branches[0].versions.iter().map(|version| version.name.as_str()).collect();
        assert_eq!(branch_versions, ["Branch created", "Branch checkpoint"]);
        assert!(reopened.branches[0].head.node(NodeId(layer)).is_some());
        assert_eq!(reopened.branches[0].head.images.len(), 1);

        let save = prepare(&session, &mut project, "Branches.figcraft");
        session.execute("object.rename", &json!({"ids": [layer], "name": "Later"})).unwrap();
        project.settle_save(&mut session, &save.key, SaveOutcome::Written).unwrap();
        assert!(project.state(&session, false).unwrap().dirty);
        let save = prepare(&session, &mut project, "Branches.figcraft");
        project.settle_save(&mut session, &save.key, SaveOutcome::Written).unwrap();
        assert!(!project.state(&session, false).unwrap().dirty);

        let mut restored = Session::empty();
        let mut restored_project = AccountProject::default();
        restored_project.start(&mut restored, None, Some(&bytes), true, None).unwrap();
        assert!(restored_project.state(&restored, false).unwrap().dirty);
        restored.execute("branch.open", &json!({"id": branch["id"]})).unwrap();
        assert!(restored.doc().unwrap().node(NodeId(layer)).is_some());
    }

    #[test]
    fn account_project_delegated_rename_undo_uses_the_native_layer() {
        let (mut session, mut project) = blank();
        let layer = rectangle(&mut session);
        let document = project.state(&session, false).unwrap().document;
        let name = session.doc().unwrap().node(NodeId(layer)).unwrap().name.clone();
        let args = json!({"layerId": layer.to_string(), "name": "Delegated"});
        assert!(project.operation(&mut session, document + 1, None, "layer.rename", &args, false).is_err());
        assert!(project.operation(&mut session, document, None, "layer.rename", &args, true).is_err());
        project.operation(&mut session, document, None, "layer.rename", &args, false).unwrap();
        assert_eq!(session.doc().unwrap().node(NodeId(layer)).unwrap().name, "Delegated");
        session.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(session.doc().unwrap().node(NodeId(layer)).unwrap().name, name);
    }
}
