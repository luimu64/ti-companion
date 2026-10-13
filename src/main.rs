//! TI-Nspire Companion — a small cross-platform file manager for TI-Nspire
//! calculators over USB.
//!
//! The USB link protocol is a clean-room Rust reimplementation derived from the
//! TI-Nspire CX CAS Student Software's link module (see `protocol.rs`,
//! `transport.rs`, `device.rs`).
//!
//! All USB work happens on a worker thread so the UI never blocks: the GUI
//! sends `Job`s and receives `Done`s over channels.

mod device;
mod mcp;
mod protocol;
mod transport;

use device::{Calc, DirItem};
use eframe::egui;
use std::sync::mpsc::{channel, Receiver, Sender};

fn main() -> eframe::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("--probe") => {
            probe();
            return Ok(());
        }
        Some("--mcp") => {
            // Headless MCP server (stdio): drive the calculator with no GUI.
            let (session, _worker) = start_worker();
            if let Err(e) = mcp::serve_stdio(session) {
                eprintln!("mcp server error: {e}");
                std::process::exit(1);
            }
            return Ok(());
        }
        _ => {}
    }

    // GUI mode: start the calculator worker, share it with the MCP server when
    // TI_NSPIRE_MCP is set, and drive it from the UI.
    let (session, _worker) = start_worker();
    if std::env::var_os("TI_NSPIRE_MCP").is_some() {
        let s = session.clone();
        std::thread::spawn(move || {
            if let Err(e) = mcp::serve_stdio(s) {
                eprintln!("mcp server stopped: {e}");
            }
        });
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    let app_session = session;
    eframe::run_native(
        "TI-Nspire Companion",
        options,
        Box::new(move |_cc| Ok(Box::new(App::new(app_session)))),
    )
}

/// Start the single calculator thread and return a handle to it.
fn start_worker() -> (Session, std::thread::JoinHandle<()>) {
    let (tx, rx) = channel::<Request>();
    let handle = std::thread::spawn(move || worker(rx));
    (Session { tx }, handle)
}

/// Headless diagnostic: shows exactly which USB step blocks or fails.
fn probe() {
    use std::time::Instant;
    let t = Instant::now();
    let log = |s: &str| println!("[{:>7.3}s] {s}", t.elapsed().as_secs_f64());
    log("connecting...");
    match device::Calc::connect() {
        Ok(mut c) => {
            log("connected + handshake ok");
            match c.device_info() {
                Ok(i) => log(&format!("device: {} type={:#x}", i.device_name, i.device_type)),
                Err(e) => log(&format!("device info failed: {e}")),
            }
            match c.dirlist("/") {
                Ok(items) => log(&format!("dirlist ok: {} entries", items.len())),
                Err(e) => log(&format!("dirlist failed: {e}")),
            }
        }
        Err(e) => log(&format!("connect failed: {e}")),
    }
}

/// A unit of work handed to the calculator thread.
#[derive(Clone)]
pub enum Job {
    Connect,
    List(String),
    Mkdir(String),
    Rename(String, String),
    Copy(String, String),
    /// Delete a batch of paths; the flag marks whether each entry is a folder.
    DeleteMany(Vec<(String, bool)>),
    Download(String),
    Upload(String, Vec<u8>),
    Info(String),
    DeviceInfo,
}

/// The result of a job, delivered back to the requester.
#[derive(Clone)]
pub enum Done {
    Connected { name: String, cx2: bool },
    Listed { path: String, items: Vec<DirItem> },
    /// A mutating action succeeded; the UI refreshes the listing.
    Ok(String),
    Downloaded { name: String, data: Vec<u8> },
    Info(String),
    DeviceInfo(serde_json::Value),
    Err(String),
}

/// One request to the calculator thread, with a private reply channel so a
/// caller can block on exactly its own result.
pub struct Request {
    pub job: Job,
    pub reply: Sender<Done>,
}

/// A handle that can drive the calculator. Shared by the GUI and the MCP server.
#[derive(Clone)]
pub struct Session {
    tx: Sender<Request>,
}

impl Session {
    /// Queue a job and wait for its result.
    pub fn call(&self, job: Job) -> Done {
        let (tx, rx) = channel();
        if self.tx.send(Request { job, reply: tx }).is_err() {
            return Done::Err("calculator worker is gone".into());
        }
        rx.recv().unwrap_or_else(|_| Done::Err("worker dropped the request".into()))
    }

    /// Queue a job whose result is delivered to `reply` (used by the GUI so it
    /// can stay non-blocking).
    pub fn tx_send(&self, job: Job, reply: Sender<Done>) -> Result<(), ()> {
        self.tx.send(Request { job, reply }).map_err(|_| ())
    }
}

struct App {
    session: Session,
    reply_tx: Sender<Done>,
    done: Receiver<Done>,
    calc_ready: bool,
    status: String,
    cwd: String,
    entries: Vec<DirItem>,
    /// Indices of the selected rows.
    selected: std::collections::BTreeSet<usize>,
    /// Anchor row for shift-range selection.
    anchor: Option<usize>,
    new_name: String,
    new_folder: String,
    device_name: String,
    busy: bool,
}

impl App {
    fn new(session: Session) -> Self {
        let (reply_tx, done) = channel::<Done>();
        App {
            session,
            reply_tx,
            done,
            calc_ready: false,
            status: "Not connected. Press Connect.".into(),
            cwd: "/".into(),
            entries: Vec::new(),
            selected: std::collections::BTreeSet::new(),
            anchor: None,
            new_name: String::new(),
            new_folder: String::new(),
            device_name: String::new(),
            busy: false,
        }
    }

    fn send(&mut self, job: Job) {
        self.busy = true;
        let reply = self.reply_tx.clone();
        let _ = self.session.tx_send(job, reply);
    }

    fn pump(&mut self) {
        while let Ok(msg) = self.done.try_recv() {
            self.busy = false;
            match msg {
                Done::Connected { name, cx2 } => {
                    self.calc_ready = true;
                    self.device_name = name;
                    self.status = format!(
                        "Connected ({} transport)",
                        if cx2 { "CX II" } else { "classic/CX" }
                    );
                }
                Done::Listed { path, items } => {
                    self.entries = items;
                    self.cwd = path;
                    self.selected.clear();
                    self.anchor = None;
                    self.status = format!("{} entries", self.entries.len());
                }
                Done::Ok(msg) => {
                    self.status = msg;
                    // Any successful mutating action changes the listing.
                    self.refresh();
                }
                Done::Downloaded { name, data } => {
                    if let Some(dest) = rfd::FileDialog::new().set_file_name(&name).save_file() {
                        match std::fs::write(&dest, &data) {
                            Ok(_) => {
                                self.status =
                                    format!("Saved {} bytes to {}", data.len(), dest.display())
                            }
                            Err(e) => self.status = format!("Write failed: {e}"),
                        }
                    }
                }
                Done::Info(msg) => self.status = msg,
                Done::DeviceInfo(_) => {}
                Done::Err(e) => self.status = format!("Error: {e}"),
            }
        }
    }

    fn path_of(&self, name: &str) -> String {
        if self.cwd.ends_with('/') {
            format!("{}{}", self.cwd, name)
        } else {
            format!("{}/{}", self.cwd, name)
        }
    }

    fn refresh(&mut self) {
        let cwd = self.cwd.clone();
        self.send(Job::List(cwd));
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();
        if self.busy {
            // Keep repainting while work is in flight.
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!self.busy && !self.calc_ready, egui::Button::new("Connect"))
                    .clicked()
                {
                    self.status = "Connecting…".into();
                    self.send(Job::Connect);
                }
                if ui
                    .add_enabled(!self.busy && self.calc_ready, egui::Button::new("Refresh"))
                    .clicked()
                {
                    self.refresh();
                }
                if ui.add_enabled(!self.busy && self.calc_ready, egui::Button::new("⬆ Up")).clicked()
                {
                    let parent = parent_path(&self.cwd);
                    self.send(Job::List(parent));
                }
                if ui
                    .add_enabled(
                        !self.busy && self.calc_ready && !self.selected.is_empty(),
                        egui::Button::new("Open"),
                    )
                    .clicked()
                {
                    if let Some(i) = self.single_selection() {
                        self.open_index(i);
                    }
                }
                ui.separator();
                ui.label(format!(
                    "Device: {}",
                    if self.device_name.is_empty() { "-" } else { &self.device_name }
                ));
                if self.busy {
                    ui.spinner();
                }
            });
            // Breadcrumb path: click any segment to jump to it. Each separator
            // and name is its own element with fixed spacing so the layout is
            // uniform regardless of name length.
            ui.horizontal(|ui| {
                let nav_enabled = !self.busy && self.calc_ready;
                ui.spacing_mut().item_spacing.x = 4.0;
                let segments = breadcrumbs(&self.cwd);
                for (idx, (label, path)) in segments.into_iter().enumerate() {
                    // idx 0 is the filesystem root and renders as its own "/".
                    // The first child follows it directly (that slash already
                    // separates them); later children get their own separator.
                    if idx == 0 {
                        if nav_enabled && ui.add(egui::Label::new("/").sense(egui::Sense::click())).clicked() {
                            self.send(Job::List(path));
                        }
                        continue;
                    }
                    if idx > 1 {
                        ui.label("/");
                    }
                    let link = egui::RichText::new(&label).color(if nav_enabled {
                        ui.visuals().hyperlink_color
                    } else {
                        ui.visuals().weak_text_color()
                    });
                    let resp = ui.add(egui::Label::new(link).sense(egui::Sense::click()));
                    if nav_enabled {
                        if resp.clicked() {
                            self.send(Job::List(path));
                        }
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut editable = self.cwd.clone();
                    if ui.add_enabled(nav_enabled, egui::TextEdit::singleline(&mut editable).desired_width(200.0)).lost_focus()
                        && editable != self.cwd
                    {
                        self.send(Job::List(editable));
                    }
                });
            });
        });

        egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
            ui.label(&self.status);
        });

        egui::SidePanel::right("actions").min_width(240.0).show(ctx, |ui| {
            let enabled = !self.busy && self.calc_ready;
            let n = self.selected.len();
            let single = self.single_selection();
            ui.heading("Actions");
            if n > 1 {
                ui.label(format!("{n} items selected"));
            }
            ui.add_space(4.0);

            if ui.add_enabled(enabled && single.is_some(), egui::Button::new("Open")).clicked() {
                if let Some(i) = single {
                    self.open_index(i);
                }
            }
            if ui.add_enabled(enabled && single.is_some(), egui::Button::new("Download…")).clicked() {
                let i = single.unwrap();
                let path = self.path_of(&self.entries[i].name);
                self.send(Job::Download(path));
            }
            if ui.add_enabled(enabled && single.is_some(), egui::Button::new("File info")).clicked() {
                let i = single.unwrap();
                let path = self.path_of(&self.entries[i].name);
                self.send(Job::Info(path));
            }

            ui.separator();
            if ui.add_enabled(enabled, egui::Button::new("Upload file…")).clicked() {
                if let Some(src) = rfd::FileDialog::new().pick_file() {
                    self.queue_upload(src);
                }
            }

            ui.separator();
            ui.label("Rename:");
            ui.text_edit_singleline(&mut self.new_name);
            if ui
                .add_enabled(enabled && single.is_some() && !self.new_name.is_empty(), egui::Button::new("Rename"))
                .clicked()
            {
                let i = single.unwrap();
                let old = self.entries[i].name.clone();
                let src = self.path_of(&old);
                let typed = self.new_name.trim().to_string();
                self.new_name.clear();
                if typed == old {
                    self.status = "Name unchanged".into();
                } else {
                    let dst = self.path_of(&rename_target(&old, &typed));
                    self.send(Job::Rename(src, dst));
                }
            }

            ui.separator();
            ui.label("New folder name:");
            ui.text_edit_singleline(&mut self.new_folder);
            if ui
                .add_enabled(enabled && !self.new_folder.is_empty(), egui::Button::new("Create folder"))
                .clicked()
            {
                let path = self.path_of(&self.new_folder.clone());
                self.new_folder.clear();
                self.send(Job::Mkdir(path));
            }

            ui.separator();
            if ui.add_enabled(enabled && single.is_some(), egui::Button::new("Duplicate (copy)")).clicked() {
                let i = single.unwrap();
                let name = self.entries[i].name.clone();
                let src = self.path_of(&name);
                let dst = self.path_of(&duplicate_name(&name, &self.entries));
                self.send(Job::Copy(src, dst));
            }
            let del_label = if n > 1 { format!("Delete {n} items") } else { "Delete".to_string() };
            if ui.add_enabled(enabled && n > 0, egui::Button::new(del_label)).clicked() {
                let targets: Vec<(String, bool)> = self
                    .selected
                    .iter()
                    .filter_map(|&i| self.entries.get(i))
                    .map(|e| (self.path_of(&e.name), e.is_dir))
                    .collect();
                self.send(Job::DeleteMany(targets));
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            // Keyboard: Enter opens, Backspace goes up, Ctrl+A selects all.
            if !self.busy && self.calc_ready {
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    if let Some(i) = self.single_selection() {
                        self.open_index(i);
                    }
                }
                if ui.input(|i| i.key_pressed(egui::Key::Backspace)) {
                    let parent = parent_path(&self.cwd);
                    self.send(Job::List(parent));
                }
                if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::A)) {
                    self.selected = (0..self.entries.len()).collect();
                    self.anchor = self.entries.len().checked_sub(1);
                }
            }

            let modifiers = ui.input(|i| i.modifiers);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let row_height = ui.text_style_height(&egui::TextStyle::Body) + 8.0;
                let full_width = ui.available_width();
                let cols = column_x(full_width);
                let font = egui::FontId::proportional(14.0);

                // Header row, painted with the same column layout as data rows.
                let (hrect, _) = ui.allocate_exact_size(
                    egui::vec2(full_width, row_height),
                    egui::Sense::hover(),
                );
                {
                    let painter = ui.painter();
                    let strong = ui.visuals().strong_text_color();
                    for (x, title) in cols.iter().zip(["Name", "Type", "Size", "Date"]) {
                        painter.text(
                            egui::pos2(hrect.left() + x, hrect.center().y),
                            egui::Align2::LEFT_CENTER,
                            title,
                            font.clone(),
                            strong,
                        );
                    }
                }

                let entries = self.entries.clone();
                for (i, e) in entries.iter().enumerate() {
                    let selected = self.selected.contains(&i);
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(full_width, row_height),
                        egui::Sense::hover(),
                    );
                    let resp = ui.interact(
                        rect,
                        egui::Id::new(("row", i)),
                        egui::Sense::click(),
                    );
                    // Striping like a standard list view.
                    let bg = if selected {
                        Some(ui.visuals().selection.bg_fill)
                    } else if i % 2 == 1 {
                        Some(ui.visuals().faint_bg_color)
                    } else {
                        None
                    };
                    if let Some(bg) = bg {
                        ui.painter().rect_filled(rect, 0.0, bg);
                    }
                    let text_color = if selected {
                        ui.visuals().strong_text_color()
                    } else {
                        ui.visuals().text_color()
                    };
                    let icon = if e.is_dir { "\u{1F4C1} " } else { "\u{1F4C4} " };
                    let cells = [
                        format!("{icon}{}", e.name),
                        if e.is_dir { "dir".to_string() } else { "file".to_string() },
                        if e.is_dir { String::new() } else { e.size.to_string() },
                        format_date(e.date),
                    ];
                    {
                        let painter = ui.painter();
                        for (x, s) in cols.iter().zip(cells.iter()) {
                            painter.text(
                                egui::pos2(rect.left() + x, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                s,
                                font.clone(),
                                text_color,
                            );
                        }
                    }
                    if resp.clicked() {
                        self.select_click(i, modifiers.shift, modifiers.command);
                    }
                    // Double-click opens folders and downloads files.
                    if resp.double_clicked() {
                        self.selected.clear();
                        self.selected.insert(i);
                        self.anchor = Some(i);
                        self.open_index(i);
                    }
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                }
            });
        });

    }
}

impl App {
    /// The selected index only when exactly one row is selected.
    fn single_selection(&self) -> Option<usize> {
        if self.selected.len() == 1 {
            self.selected.iter().next().copied()
        } else {
            None
        }
    }

    /// Apply a click with its modifiers, like a standard list view.
    fn select_click(&mut self, i: usize, shift: bool, command: bool) {
        let (sel, anchor) = apply_click(&self.selected, self.anchor, i, shift, command);
        self.selected = sel;
        self.anchor = anchor;
        // Pre-fill the rename box with the selected name, the way a file
        // manager does, so the user edits rather than retypes it.
        self.new_name = if self.selected.len() == 1 {
            self.entries.get(i).map(|e| e.name.clone()).unwrap_or_default()
        } else {
            String::new()
        };
    }

    /// Read a local file and queue it for upload into the current directory.
    fn queue_upload(&mut self, src: std::path::PathBuf) {
        let name = src
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.is_empty() {
            return;
        }
        match std::fs::read(&src) {
            Ok(data) => {
                let dst = self.path_of(&name);
                self.send(Job::Upload(dst, data));
            }
            Err(e) => self.status = format!("Read failed: {e}"),
        }
    }

    /// Act on an entry the way a double-click should: folders open, files
    /// download.
    fn open_index(&mut self, i: usize) {
        if !self.calc_ready || self.busy {
            return;
        }
        let Some(item) = self.entries.get(i).cloned() else { return };
        let path = self.path_of(&item.name);
        if item.is_dir {
            self.send(Job::List(path));
        } else {
            self.send(Job::Download(path));
        }
    }
}

/// The calculator thread: owns the connection and executes jobs serially.
fn worker(reqs: Receiver<Request>) {
    let mut calc: Option<Calc> = None;
    while let Ok(Request { job, reply }) = reqs.recv() {
        let out = match job {
            Job::Connect => match Calc::connect() {
                Ok(mut c) => {
                    let name = c.device_info().map(|i| i.device_name).unwrap_or_default();
                    let cx2 = c.is_cx2();
                    calc = Some(c);
                    // Report the connection, then list the root.
                    let _ = reply.send(Done::Connected { name, cx2 });
                    match calc.as_mut().map(|c| c.dirlist("/")) {
                        Some(Ok(items)) => Done::Listed { path: "/".into(), items },
                        Some(Err(e)) => Done::Err(format!("list failed: {e}")),
                        None => Done::Err("not connected".into()),
                    }
                }
                Err(e) => Done::Err(format!("connect failed: {e}")),
            },
            other => {
                let Some(c) = calc.as_mut() else {
                    let _ = reply.send(Done::Err("not connected".into()));
                    continue;
                };
                match other {
                    Job::List(path) => match c.dirlist(&path) {
                        Ok(items) => Done::Listed { path, items },
                        Err(e) => Done::Err(format!("list failed: {e}")),
                    },
                    Job::Mkdir(path) => report(c.mkdir(&path).map(|_| format!("Created {path}"))),
                    Job::Rename(a, b) => {
                        report(c.rename(&a, &b).map(|_| format!("Renamed to {b}")))
                    }
                    Job::Copy(a, b) => report(c.copy(&a, &b).map(|_| format!("Copied to {b}"))),
                    Job::DeleteMany(targets) => {
                        let mut deleted = 0usize;
                        let mut failed: Vec<String> = Vec::new();
                        for (path, is_dir) in &targets {
                            let r = if *is_dir { c.rmdir(path) } else { c.delete_file(path) };
                            match r {
                                Ok(_) => deleted += 1,
                                Err(e) => failed.push(format!("{path}: {e}")),
                            }
                        }
                        if failed.is_empty() {
                            Done::Ok(format!("Deleted {deleted} item(s)"))
                        } else if deleted > 0 {
                            // Some succeeded, so the listing changed: report as Ok so
                            // the UI refreshes, with the failures noted.
                            Done::Ok(format!(
                                "Deleted {deleted}/{}; failed: {}",
                                targets.len(),
                                failed.join("; ")
                            ))
                        } else {
                            Done::Err(format!("delete failed: {}", failed.join("; ")))
                        }
                    }
                    Job::Download(path) => match c.download(&path) {
                        Ok(data) => {
                            let name = path.rsplit('/').next().unwrap_or("file").to_string();
                            Done::Downloaded { name, data }
                        }
                        Err(e) => Done::Err(format!("download failed: {e}")),
                    },
                    Job::Upload(path, data) => match c.upload(&path, &data) {
                        Ok(_) => Done::Ok(format!("Uploaded {} bytes to {path}", data.len())),
                        Err(e) => Done::Err(format!("upload failed: {e}")),
                    },
                    Job::Info(path) => match c.attributes(&path) {
                        Ok(a) => Done::Info(format!(
                            "{}: {} | size {} | date {}",
                            a.name,
                            if a.is_dir { "folder" } else { "file" },
                            a.size,
                            format_date(a.date)
                        )),
                        Err(e) => Done::Err(format!("info failed: {e}")),
                    },
                    Job::DeviceInfo => match c.device_info() {
                        Ok(i) => Done::DeviceInfo(device_info_json(&i, c.is_cx2())),
                        Err(e) => Done::Err(format!("device info failed: {e}")),
                    },
                    Job::Connect => unreachable!(),
                }
            }
        };
        let _ = reply.send(out);
    }
}

/// Serialise device information for the MCP tools.
fn device_info_json(i: &device::DevInfo, cx2: bool) -> serde_json::Value {
    serde_json::json!({
        "name": i.device_name,
        "cx2": cx2,
        "device_type": i.device_type,
        "os_version": version_string(i.os_version),
        "boot1_version": version_string(i.boot1_version),
        "boot2_version": version_string(i.boot2_version),
        "runlevel": i.runlevel,
        "battery": i.battery,
        "clock_speed": i.clock_speed,
        "lcd": { "width": i.lcd_width, "height": i.lcd_height },
        "storage": { "free": i.flash_free, "total": i.flash_total },
        "ram": { "free": i.ram_free, "total": i.ram_total },
    })
}

fn version_string((major, minor, build): (u8, u8, u16)) -> String {
    format!("{major}.{minor}.{build}")
}

fn report(r: Result<String, transport::TransportError>) -> Done {
    match r {
        Ok(msg) => Done::Ok(msg),
        Err(e) => Done::Err(e.to_string()),
    }
}

fn parent_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => trimmed[..i].to_string(),
    }
}

/// Split a path into (label, full-path) breadcrumb segments, root first.
fn breadcrumbs(path: &str) -> Vec<(String, String)> {
    let mut out = vec![("/".to_string(), "/".to_string())];
    let mut acc = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        acc.push('/');
        acc.push_str(seg);
        out.push((seg.to_string(), acc.clone()));
    }
    out
}

/// X offsets for the Name/Type/Size/Date columns.
fn column_x(total_width: f32) -> [f32; 4] {
    let pad = 6.0;
    let name = (total_width * 0.5).max(120.0);
    let type_w = 60.0;
    let size_w = 90.0;
    [pad, name, name + type_w, name + type_w + size_w]
}

/// Compute the new selection for a click, mirroring standard list-view rules.
fn apply_click(
    selected: &std::collections::BTreeSet<usize>,
    anchor: Option<usize>,
    i: usize,
    shift: bool,
    command: bool,
) -> (std::collections::BTreeSet<usize>, Option<usize>) {
    use std::collections::BTreeSet;
    if shift {
        let a = anchor.unwrap_or(i);
        let (lo, hi) = if a <= i { (a, i) } else { (i, a) };
        ((lo..=hi).collect(), anchor.or(Some(i)))
    } else if command {
        let mut s = selected.clone();
        if !s.insert(i) {
            s.remove(&i);
        }
        (s, Some(i))
    } else {
        let mut s = BTreeSet::new();
        s.insert(i);
        (s, Some(i))
    }
}

/// Build a non-colliding copy name, keeping the suffix *before* the extension
/// (the calculator rejects unknown extensions, so `a.tns` must become
/// `a (copy).tns`, never `a.tns (copy)`).
fn duplicate_name(name: &str, existing: &[DirItem]) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let taken = |candidate: &str| existing.iter().any(|e| e.name == candidate);
    let first = format!("{stem} (copy){ext}");
    if !taken(&first) {
        return first;
    }
    for n in 2..1000 {
        let candidate = format!("{stem} (copy {n}){ext}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    format!("{stem} (copy){ext}")
}

/// Decide the new file name for a rename.
///
/// Users typically type a bare name, so if they drop the extension we keep the
/// original one (the calculator rejects unknown extensions). A typed name that
/// contains a separator is treated as a full path and used verbatim.
fn rename_target(old: &str, typed: &str) -> String {
    if typed.contains('/') || typed.contains('\\') {
        return typed.trim_start_matches('/').to_string();
    }
    let has_ext = typed.rfind('.').is_some_and(|i| i > 0 && i + 1 < typed.len());
    if has_ext {
        return typed.to_string();
    }
    match old.rfind('.') {
        Some(i) if i > 0 => format!("{typed}{}", &old[i..]),
        _ => typed.to_string(),
    }
}

/// The calculator reports a packed date; render it as YYYY-MM-DD when plausible.
fn format_date(d: u32) -> String {
    if d == 0 || d == 0xFFFF_FFFF {
        return "-".into();
    }
    let year = ((d >> 9) & 0x7F) + 1980;
    let month = (d >> 5) & 0x0F;
    let day = d & 0x1F;
    if (1..=12).contains(&month) && (1..=31).contains(&day) {
        format!("{year:04}-{month:02}-{day:02}")
    } else {
        format!("{d:#010x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_of_root_and_nested() {
        assert_eq!(parent_path("/"), "/");
        assert_eq!(parent_path("/docs"), "/");
        assert_eq!(parent_path("/docs/a"), "/docs");
        assert_eq!(parent_path("/docs/a/"), "/docs");
    }

    #[test]
    fn rename_keeps_extension_and_rejects_separators() {
        // Bare name: keep the original extension.
        assert_eq!(rename_target("a.tns", "homework"), "homework.tns");
        // Explicit extension wins.
        assert_eq!(rename_target("a.tns", "homework.tno"), "homework.tno");
        // Folder (no extension) stays as typed.
        assert_eq!(rename_target("docs", "stuff"), "stuff");
        // Typed full path is used verbatim (leading slash stripped).
        assert_eq!(rename_target("a.tns", "/docs/b.tns"), "docs/b.tns");
        // Dotfile-ish and trailing-dot inputs are not treated as extensions.
        assert_eq!(rename_target("a.tns", "x."), "x..tns");
    }

    #[test]
    fn duplicate_keeps_extension_last() {
        let item = |n: &str| DirItem { name: n.into(), size: 0, date: 0, is_dir: false };
        let none: Vec<DirItem> = vec![];
        assert_eq!(duplicate_name("a.tns", &none), "a (copy).tns");
        assert_eq!(duplicate_name("a.tno", &none), "a (copy).tno");
        // No extension, and dotfiles keep their name.
        assert_eq!(duplicate_name("plain", &none), "plain (copy)");
        assert_eq!(duplicate_name(".hidden", &none), ".hidden (copy)");
        // Collisions get a counter.
        let with_one = vec![item("a.tns"), item("a (copy).tns")];
        assert_eq!(duplicate_name("a.tns", &with_one), "a (copy 2).tns");
        let with_two = vec![item("a (copy).tns"), item("a (copy 2).tns")];
        assert_eq!(duplicate_name("a.tns", &with_two), "a (copy 3).tns");
    }

    #[test]
    fn breadcrumb_paths() {
        assert_eq!(breadcrumbs("/"), vec![("/".to_string(), "/".to_string())]);
        let b = breadcrumbs("/docs/sub");
        assert_eq!(b.len(), 3);
        assert_eq!(b[0], ("/".into(), "/".into()));
        assert_eq!(b[1], ("docs".into(), "/docs".into()));
        assert_eq!(b[2], ("sub".into(), "/docs/sub".into()));
    }

    /// The breadcrumb labels, joined the way the UI renders them, must rebuild
    /// the original path (catches stray doubled separators).
    #[test]
    fn breadcrumb_labels_join_to_path() {
        let render = |p: &str| {
            breadcrumbs(p)
                .into_iter()
                .enumerate()
                .map(|(i, (label, _))| match i {
                    0 => "/".to_string(),
                    1 => label,
                    _ => format!("/{label}"),
                })
                .collect::<String>()
        };        for p in ["/", "/docs", "/docs/sub", "/a/b/c"] {
            assert_eq!(render(p), p, "rendering {p}");
        }
    }

    fn set(v: &[usize]) -> std::collections::BTreeSet<usize> {
        v.iter().copied().collect()
    }

    #[test]
    fn click_selection_rules() {
        use std::collections::BTreeSet;
        let empty = BTreeSet::new();

        // Plain click replaces the selection and sets the anchor.
        let (s, a) = apply_click(&set(&[0, 1, 2]), Some(0), 3, false, false);
        assert_eq!(s, set(&[3]));
        assert_eq!(a, Some(3));

        // Ctrl toggles in and out.
        let (s, _) = apply_click(&set(&[3]), Some(3), 5, false, true);
        assert_eq!(s, set(&[3, 5]));
        let (s, _) = apply_click(&set(&[3, 5]), Some(5), 5, false, true);
        assert_eq!(s, set(&[3]));

        // Shift selects the inclusive range from the anchor, either direction.
        let (s, a) = apply_click(&empty, Some(4), 2, true, false);
        assert_eq!(s, set(&[2, 3, 4]));
        assert_eq!(a, Some(4));
        let (s, _) = apply_click(&empty, Some(2), 4, true, false);
        assert_eq!(s, set(&[2, 3, 4]));

        // Shift with no anchor falls back to the clicked row.
        let (s, _) = apply_click(&empty, None, 7, true, false);
        assert_eq!(s, set(&[7]));
    }
}
