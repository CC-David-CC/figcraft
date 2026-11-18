//! Same-origin WebOS document transport. The account service lives in the parent document.

use std::{cell::RefCell, collections::VecDeque, rc::Rc};

use figcraft_engine::account_project::{AccountProject, MAX_BYTES, PreparedSave, ProjectState, SaveKey, SaveOutcome};
use figcraft_ui_egui::{AccountIntent, FigApp};
use js_sys::{Array, ArrayBuffer, Object, Reflect, Uint8Array};
use serde_json::Value;
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use web_sys::{MessageEvent, Window};

const PREFIX: &str = "figcraft:account-";

#[derive(Clone)]
pub struct Parent {
    window: Window,
    origin: String,
    generation: String,
}

impl Parent {
    pub fn connect() -> Result<Self, String> {
        let browser = web_sys::window().ok_or("No browser window.")?;
        let window = browser.parent().ok().flatten().ok_or("No parent desktop.")?;
        let origin = browser.location().origin().map_err(|_| "No browser origin.")?;
        if Object::is(browser.as_ref(), window.as_ref()) || window.location().origin().ok().as_ref() != Some(&origin) {
            return Err("FigCraft needs a same-origin WebOS account window.".into());
        }
        let mut entropy = [0u8; 16];
        browser
            .crypto()
            .and_then(|crypto| crypto.get_random_values_with_u8_array(&mut entropy))
            .map_err(|_| "Could not identify the FigCraft native editor generation.")?;
        let generation = entropy.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(Self { window, origin, generation })
    }

    fn send(&self, kind: &str, packet: Object, bytes: Option<Vec<u8>>) -> Result<(), String> {
        set(&packet, "type", format!("{PREFIX}{kind}"))?;
        set(&packet, "generation", self.generation.as_str())?;
        let transfer = Array::new();
        if let Some(bytes) = bytes {
            let buffer = Uint8Array::from(bytes.as_slice()).buffer();
            set(&packet, "bytes", buffer.clone())?;
            transfer.push(&buffer);
        }
        self.window.post_message_with_transfer(&packet, &self.origin, &transfer).map_err(|_| "Could not reach the FigCraft desktop.".into())
    }

    pub fn ready(&self, error: Option<&str>) -> Result<(), String> {
        let packet = Object::new();
        if let Some(error) = error {
            set(&packet, "message", error)?;
        }
        self.send(if error.is_some() { "failed" } else { "ready" }, packet, None)
    }
}

fn set(object: &Object, name: &str, value: impl Into<JsValue>) -> Result<(), String> {
    if Reflect::set(object, &name.into(), &value.into()).ok() == Some(true) { Ok(()) } else { Err("Could not encode the FigCraft message.".into()) }
}

fn get(object: &JsValue, name: &str) -> JsValue {
    Reflect::get(object, &name.into()).unwrap_or(JsValue::UNDEFINED)
}

fn string(object: &JsValue, name: &str, limit: usize) -> Option<String> {
    get(object, name).as_string().filter(|value| value.len() <= limit)
}

fn integer(object: &JsValue, name: &str) -> Option<u64> {
    string(object, name, 20)?.parse().ok()
}

fn request(object: &JsValue) -> Option<String> {
    string(object, "requestId", 36).filter(|id| id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
}

fn nullable(object: &JsValue, name: &str, limit: usize) -> Result<Option<String>, String> {
    if get(object, name).is_null() {
        return Ok(None);
    }
    string(object, name, limit).map(Some).ok_or_else(|| format!("Invalid FigCraft {name}."))
}

fn save_key(object: &JsValue) -> Option<SaveKey> {
    Some(SaveKey {
        ticket: integer(object, "ticket")?,
        document: integer(object, "documentId")?,
        revision: integer(object, "revision")?,
        path: string(object, "path", 2048)?,
    })
}

fn write_save_key(packet: &Object, key: &SaveKey) -> Result<(), String> {
    set(packet, "ticket", key.ticket.to_string())?;
    set(packet, "documentId", key.document.to_string())?;
    set(packet, "revision", key.revision.to_string())?;
    set(packet, "path", key.path.as_str())
}

fn write_state(packet: &Object, state: Option<&ProjectState>) -> Result<(), String> {
    set(packet, "available", state.is_some())?;
    if let Some(state) = state {
        set(packet, "documentId", state.document.to_string())?;
        set(packet, "revision", state.revision.to_string())?;
        set(packet, "recoveryRevision", state.revision.to_string())?;
        set(packet, "dirty", state.dirty)?;
        set(packet, "capturable", state.capturable)?;
        set(packet, "autosaveEligible", state.autosave_eligible)?;
        set(packet, "operationPending", state.operation_pending)?;
        set(packet, "unfinishedEdit", state.unfinished_edit)?;
    }
    Ok(())
}

#[derive(Clone)]
pub struct Commands(Rc<RefCell<VecDeque<AccountIntent>>>);

impl Commands {
    pub fn send(&self, intent: AccountIntent) -> Result<(), String> {
        let mut commands = self.0.try_borrow_mut().map_err(|_| "FigCraft is handling an account request.")?;
        if !commands.is_empty() {
            return Err("FigCraft is handling an account request.".into());
        }
        commands.push_back(intent);
        Ok(())
    }
}

struct Dialog {
    id: u64,
    save: Option<(u64, u64)>,
}

pub struct AccountHost {
    parent: Parent,
    browser: Window,
    inbox: Rc<RefCell<VecDeque<JsValue>>>,
    listener: Closure<dyn FnMut(MessageEvent)>,
    commands: Commands,
    project: AccountProject,
    last_state: Option<ProjectState>,
    dialog: Option<Dialog>,
    next_dialog: u64,
    lease: Option<String>,
    finalized: bool,
}

impl AccountHost {
    pub fn install(parent: Parent, ctx: &egui::Context) -> Result<Self, String> {
        let browser = web_sys::window().ok_or("No browser window.")?;
        let inbox = Rc::new(RefCell::new(VecDeque::new()));
        let pending = inbox.clone();
        let source = parent.clone();
        let ctx = ctx.clone();
        let listener = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let from_parent = event.source().is_some_and(|from| Object::is(from.as_ref(), source.window.as_ref()));
            if !from_parent || event.origin() != source.origin {
                return;
            }
            let packet = event.data();
            if string(&packet, "generation", 32).as_deref() != Some(&source.generation)
                || !string(&packet, "type", 64).is_some_and(|kind| kind.starts_with(PREFIX))
            {
                return;
            }
            if let Ok(mut pending) = pending.try_borrow_mut()
                && pending.len() < 8
            {
                pending.push_back(packet);
                ctx.request_repaint();
            }
        });
        browser
            .add_event_listener_with_callback("message", listener.as_ref().unchecked_ref())
            .map_err(|_| "Could not receive FigCraft account messages.")?;
        Ok(Self {
            parent,
            browser,
            inbox,
            listener,
            commands: Commands(Rc::default()),
            project: AccountProject::default(),
            last_state: None,
            dialog: None,
            next_dialog: 0,
            lease: None,
            finalized: false,
        })
    }

    pub fn commands(&self) -> Commands {
        self.commands.clone()
    }

    pub fn suspended(&self) -> bool {
        self.lease.is_some() || self.finalized
    }

    fn state(&mut self, app: &FigApp) -> Option<ProjectState> {
        self.project.state(&app.session, self.dialog.is_some() || app.account_ui_busy())
    }

    pub fn publish_state(&mut self, app: &FigApp) -> Result<(), String> {
        let state = self.state(app);
        if state != self.last_state {
            let packet = Object::new();
            write_state(&packet, state.as_ref())?;
            self.parent.send("project-state", packet, None)?;
            self.last_state = state;
        }
        Ok(())
    }

    pub fn poll(&mut self, app: &mut FigApp) {
        let packets = std::mem::take(&mut *self.inbox.borrow_mut());
        for packet in packets {
            if let Err(error) = self.receive(app, &packet) {
                app.show_error(error);
            }
        }
        let intent = self.commands.0.borrow_mut().pop_front();
        if !self.suspended()
            && let Some(intent) = intent
            && let Err(error) = self.intent(app, intent)
        {
            app.show_error(error);
        }
    }

    fn intent(&mut self, app: &mut FigApp, intent: AccountIntent) -> Result<(), String> {
        if matches!(intent, AccountIntent::Close) {
            return self.parent.send("close-intent", Object::new(), None);
        }
        let state = self.state(app).ok_or("The FigCraft project has not opened.")?;
        if state.operation_pending || state.unfinished_edit {
            return Err("Finish the current FigCraft operation first.".into());
        }
        let packet = Object::new();
        let save = match intent {
            AccountIntent::New(settings) => {
                set(&packet, "operation", "new")?;
                set(&packet, "settings", settings.to_string())?;
                None
            }
            AccountIntent::Open => {
                set(&packet, "operation", "open")?;
                set(&packet, "path", JsValue::NULL)?;
                None
            }
            AccountIntent::Save { save_as } => {
                if !save_as && let Some(path) = self.project.path().map(str::to_owned) {
                    let prepared =
                        self.project.prepare_save(&app.session, state.document, state.revision, path).map_err(|error| error.to_string())?;
                    return self.send_save("save", packet, prepared, app);
                }
                let name = app.session.docs.iter().find(|tab| tab.uid == state.document).map_or("Untitled", |tab| tab.doc.name.as_str());
                set(&packet, "operation", "save")?;
                set(&packet, "suggested", format!("{name}.figcraft"))?;
                Some((state.document, state.revision))
            }
            AccountIntent::Close => return Ok(()),
        };
        self.next_dialog = self.next_dialog.checked_add(1).ok_or("FigCraft's dialog request limit was reached.")?;
        set(&packet, "requestId", self.next_dialog.to_string())?;
        self.parent.send("dialog", packet, None)?;
        self.dialog = Some(Dialog { id: self.next_dialog, save });
        Ok(())
    }

    fn send_save(&mut self, kind: &str, packet: Object, prepared: PreparedSave, app: &mut FigApp) -> Result<(), String> {
        let key = prepared.key;
        let sent = write_save_key(&packet, &key).and_then(|()| self.parent.send(kind, packet, Some(prepared.bytes)));
        if sent.is_err() {
            let _ = self.project.settle_save(&mut app.session, &key, SaveOutcome::Failed);
        }
        sent
    }

    fn receive(&mut self, app: &mut FigApp, packet: &JsValue) -> Result<(), String> {
        let kind = string(packet, "type", 64).ok_or("Invalid FigCraft message.")?;
        let Some(kind) = kind.strip_prefix(PREFIX) else { return Ok(()) };
        if self.suspended() && kind != "session" {
            return Ok(());
        }
        match kind {
            "start" => self.start(app, packet),
            "dialog-result" => self.dialog_result(app, packet),
            "save-result" => self.save_result(app, packet),
            "recovery-capture" => self.recover(app, packet),
            "autosave" | "session-save" => self.capture_save(app, packet, kind),
            "operation" => self.operation(app, packet),
            "session" => self.session(app, packet),
            "close" => {
                let response = response(packet)?;
                let approved = self.state(app).is_some_and(|state| !state.dirty && !state.operation_pending && !state.unfinished_edit);
                set(&response, "approved", approved)?;
                self.parent.send("close-result", response, None)
            }
            _ => Ok(()),
        }
    }

    fn start(&mut self, app: &mut FigApp, packet: &JsValue) -> Result<(), String> {
        let response = response(packet)?;
        let result = (|| {
            let path = nullable(packet, "path", 2048)?;
            let payload = get(packet, "bytes");
            let bytes = if payload.is_null() {
                None
            } else {
                let buffer = payload.dyn_into::<ArrayBuffer>().map_err(|_| "Invalid FigCraft project bytes.")?;
                if buffer.byte_length() as usize > MAX_BYTES {
                    return Err("The FigCraft project exceeds 128 MiB.".into());
                }
                Some(Uint8Array::new(&buffer).to_vec())
            };
            let settings = nullable(packet, "newSettings", 4096)?
                .map(|value| serde_json::from_str::<Value>(&value))
                .transpose()
                .map_err(|_| "Invalid FigCraft project settings.")?;
            let recovery = get(packet, "recover").as_bool().ok_or("Invalid FigCraft recovery request.")?;
            self.project.start(&mut app.session, path, bytes.as_deref(), recovery, settings.as_ref()).map_err(|error| error.to_string())?;
            app.ui.home = false;
            if let Some(warning) = app.session.message.take() {
                app.toast(warning);
            }
            Ok::<_, String>(())
        })();
        success(&response, result)?;
        if let Some(state) = self.state(app) {
            write_state(&response, Some(&state))?;
            set(&response, "path", self.project.path().map_or(JsValue::NULL, JsValue::from_str))?;
        }
        self.parent.send("started", response, None)
    }

    fn dialog_result(&mut self, app: &mut FigApp, packet: &JsValue) -> Result<(), String> {
        if self.dialog.as_ref().map(|dialog| dialog.id) != integer(packet, "requestId") {
            return Ok(());
        }
        let Some(dialog) = self.dialog.take() else { return Ok(()) };
        let outcome = string(packet, "outcome", 16);
        if outcome.as_deref() == Some("failed") {
            return Err(string(packet, "message", 512).unwrap_or_else(|| "The FigCraft dialog did not complete.".into()));
        }
        if outcome.as_deref() == Some("destination")
            && let Some((document, revision)) = dialog.save
        {
            let path = string(packet, "path", 2048).ok_or("Invalid FigCraft destination.")?;
            let prepared = self.project.prepare_save(&app.session, document, revision, path).map_err(|error| error.to_string())?;
            self.send_save("save", Object::new(), prepared, app)?;
        }
        Ok(())
    }

    fn save_result(&mut self, app: &mut FigApp, packet: &JsValue) -> Result<(), String> {
        let key = save_key(packet).ok_or("Invalid FigCraft save receipt.")?;
        let outcome = match string(packet, "outcome", 16).as_deref() {
            Some("written") => SaveOutcome::Written,
            Some("failed") => SaveOutcome::Failed,
            Some("unknown") => SaveOutcome::Unknown,
            _ => return Err("Invalid FigCraft save outcome.".into()),
        };
        let adopted = self.project.settle_save(&mut app.session, &key, outcome).map_err(|error| error.to_string())?;
        if outcome != SaveOutcome::Unknown {
            let response = Object::new();
            write_save_key(&response, &key)?;
            set(&response, "adopted", adopted)?;
            let state = Object::new();
            write_state(&state, self.state(app).as_ref())?;
            set(&response, "projectState", state)?;
            self.parent.send("save-settled", response, None)?;
        }
        if adopted {
            app.toast("Saved to account");
        } else if let Some(message) = string(packet, "message", 512) {
            app.show_error(message);
        }
        Ok(())
    }

    fn recover(&mut self, app: &FigApp, packet: &JsValue) -> Result<(), String> {
        let response = response(packet)?;
        let (document, revision) = target(packet)?;
        write_target(&response, document, revision)?;
        let bytes = match self.project.capture_recovery(&app.session, document, revision) {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                set(&response, "message", error.to_string())?;
                None
            }
        };
        self.parent.send("recovery-captured", response, bytes)
    }

    fn capture_save(&mut self, app: &mut FigApp, packet: &JsValue, kind: &str) -> Result<(), String> {
        let response = response(packet)?;
        let (document, revision) = target(packet)?;
        let path = string(packet, "path", 2048).ok_or("Invalid FigCraft destination.")?;
        let automatic = kind == "autosave";
        let allowed = self.state(app).is_some_and(|state| {
            state.document == document
                && state.revision == revision
                && state.dirty
                && !state.operation_pending
                && !state.unfinished_edit
                && (!automatic || state.autosave_eligible && self.project.path() == Some(path.as_str()))
        });
        let reply = if automatic { "autosave-result" } else { "session-save-result" };
        set(&response, "reviewedRevision", revision.to_string())?;
        if allowed {
            match self.project.prepare_save(&app.session, document, revision, path.clone()) {
                Ok(prepared) => {
                    set(&response, "outcome", "captured")?;
                    return self.send_save(reply, response, prepared, app);
                }
                Err(error) => {
                    set(&response, "outcome", "failed")?;
                    set(&response, "message", error.to_string())?;
                }
            }
        } else {
            set(&response, "outcome", "busy")?;
        }
        write_target(&response, document, revision)?;
        set(&response, "path", path)?;
        self.parent.send(reply, response, None)
    }

    fn operation(&mut self, app: &mut FigApp, packet: &JsValue) -> Result<(), String> {
        let response = response(packet)?;
        let document = integer(packet, "documentId").ok_or("Invalid FigCraft operation target.")?;
        set(&response, "documentId", document.to_string())?;
        let command = string(packet, "command", 64).ok_or("Invalid FigCraft command.")?;
        let path = nullable(packet, "path", 2048)?;
        let args: Value = serde_json::from_str(&string(packet, "args", 4096).ok_or("Invalid FigCraft operation arguments.")?)
            .map_err(|_| "Invalid FigCraft operation arguments.")?;
        let busy = self.dialog.is_some() || app.account_ui_busy();
        match self.project.operation(&mut app.session, document, path.as_deref(), &command, &args, busy) {
            Ok(value) => set(&response, "result", value.to_string())?,
            Err(error) => set(&response, "message", error.to_string())?,
        }
        self.parent.send("operation-result", response, None)
    }

    fn session(&mut self, app: &FigApp, packet: &JsValue) -> Result<(), String> {
        let response = response(packet)?;
        let operation = string(packet, "operation", 16).ok_or("Invalid FigCraft session operation.")?;
        let lease = string(packet, "lease", 64).filter(|lease| !lease.is_empty()).ok_or("Invalid FigCraft session lease.")?;
        let result = match operation.as_str() {
            "pause" if !self.suspended() => {
                let (document, revision) = target(packet)?;
                let exact = self.state(app).is_some_and(|state| {
                    state.document == document
                        && state.revision == revision
                        && Some(state.dirty) == get(packet, "dirty").as_bool()
                        && Some(state.unfinished_edit) == get(packet, "unfinishedEdit").as_bool()
                        && !state.operation_pending
                });
                if exact {
                    self.lease = Some(lease.clone());
                    Ok(())
                } else {
                    Err("The FigCraft project changed after close review.".into())
                }
            }
            "release" | "finalize" if self.lease.as_deref() == Some(&lease) => {
                self.lease = None;
                self.finalized = operation == "finalize";
                Ok(())
            }
            _ => Err("The FigCraft session lease is unavailable.".into()),
        };
        set(&response, "operation", operation)?;
        set(&response, "lease", lease)?;
        success(&response, result)?;
        self.parent.send("session-result", response, None)
    }
}

impl Drop for AccountHost {
    fn drop(&mut self) {
        let _ = self.browser.remove_event_listener_with_callback("message", self.listener.as_ref().unchecked_ref());
    }
}

fn response(packet: &JsValue) -> Result<Object, String> {
    let response = Object::new();
    set(&response, "requestId", request(packet).ok_or("Invalid FigCraft request ID.")?)?;
    Ok(response)
}

fn target(packet: &JsValue) -> Result<(u64, u64), String> {
    integer(packet, "documentId").zip(integer(packet, "revision")).ok_or_else(|| "Invalid FigCraft document target.".into())
}

fn write_target(packet: &Object, document: u64, revision: u64) -> Result<(), String> {
    set(packet, "documentId", document.to_string())?;
    set(packet, "revision", revision.to_string())
}

fn success(response: &Object, result: Result<(), String>) -> Result<(), String> {
    set(response, "ok", result.is_ok())?;
    if let Err(error) = result {
        set(response, "message", error)?;
    }
    Ok(())
}
