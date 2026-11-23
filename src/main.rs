use anyhow::{Context, Result};
use archive_search::{APP_ID, Hit, NEW, OFFLINE, ONLINE, PAGE_SIZE, PARTIAL};
use gtk::gio::{self, prelude::*};
use gtk::glib::{self, ControlFlow};
use gtk::prelude::*;
use rusqlite::Connection;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime};
use tempfile::{Builder, TempDir};

const TITLE: &str = "Archive Scout";
const PORTAL: &str = "org.freedesktop.portal.Desktop";
const SCAN_INTERVAL: Duration = Duration::from_secs(120);
const SCAN_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_SCANS: usize = 3;
type SearchResult = Result<(Vec<Hit>, bool)>;
type Request = (u64, String, i64);

fn main() {
    let arguments: Vec<_> = std::env::args_os().collect();
    if let Some(mode) = arguments.get(1).and_then(|mode| mode.to_str()) {
        let prefix = archive_search::filesystem_root();
        let result = match (mode, arguments.len()) {
            ("--scan", 5) => (|| {
                let mut db = archive_search::database(Path::new(&arguments[2]))?;
                let id = archive_search::text_path(Path::new(&arguments[3]))?.parse()?;
                archive_search::scan(&mut db, id, prefix.as_deref(), Path::new(&arguments[4]))
            })(),
            ("--extract", 7) => (|| {
                archive_search::extract(
                    Path::new(&arguments[2]),
                    archive_search::text_path(Path::new(&arguments[3]))?.parse()?,
                    archive_search::text_path(Path::new(&arguments[4]))?,
                    archive_search::text_path(Path::new(&arguments[5]))?,
                    prefix.as_deref(),
                    Path::new(&arguments[6]),
                )
            })(),
            _ => Err(anyhow::anyhow!("Неизвестные аргументы")),
        };
        if let Err(error) = result {
            eprintln!("{error:#}");
            std::process::exit(1);
        }
        return;
    }

    let application = gtk::Application::builder().application_id(APP_ID).build();
    application.connect_activate(|application| {
        if let Some(window) = application.active_window() {
            window.present();
            return;
        }
        if let Err(error) = create_window(application) {
            eprintln!("{error:#}");
            let window = gtk::ApplicationWindow::builder()
                .application(application)
                .title(TITLE)
                .build();
            window.present();
            alert(
                window.upcast_ref(),
                "Не удалось запустить приложение",
                &format!("{error:#}"),
            );
        }
    });
    application.run();
}

fn status_text(state: i64) -> &'static str {
    match state {
        ONLINE => "Доступен",
        OFFLINE => "Недоступен · сохранённый индекс",
        PARTIAL => "Проверка неполная",
        _ => "Ожидает сканирования",
    }
}

fn alert(parent: &gtk::Window, title: &str, detail: &str) {
    let dialog = gtk::MessageDialog::builder()
        .transient_for(parent)
        .modal(true)
        .buttons(gtk::ButtonsType::Close)
        .text(title)
        .secondary_text(detail)
        .build();
    dialog.connect_response(|dialog, _| dialog.close());
    dialog.present();
}

fn button(text: &str, class: Option<&str>) -> gtk::Button {
    let button = gtk::Button::with_label(text);
    if let Some(class) = class {
        button.add_css_class(class);
    }
    button
}

fn label(text: &str, class: Option<&str>) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    if let Some(class) = class {
        label.add_css_class(class);
    }
    label
}

struct ScanJob {
    child: Child,
    files: TempDir,
    started: Instant,
}

impl ScanJob {
    fn start(database: &Path, id: i64, directory: &Path) -> Result<Self> {
        let files = Builder::new().prefix("scan-").tempdir_in(directory)?;
        let child = Command::new(std::env::current_exe()?)
            .arg("--scan")
            .arg(database)
            .arg(id.to_string())
            .arg(files.path().join("heartbeat"))
            .stdout(Stdio::null())
            .stderr(File::create(files.path().join("error"))?)
            .spawn()?;
        Ok(Self {
            child,
            files,
            started: Instant::now(),
        })
    }

    fn error(&self) -> String {
        fs::read_to_string(self.files.path().join("error"))
            .unwrap_or_default()
            .trim()
            .to_owned()
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.try_wait();
    }
}

struct App {
    window: gtk::ApplicationWindow,
    db: Connection,
    database: PathBuf,
    scratch: TempDir,
    open_cache: PathBuf,
    source_list: gtk::ListBox,
    source_rows: RefCell<Vec<(i64, gtk::Label, gtk::Label)>>,
    scan_button: gtk::Button,
    scan_status: gtk::Label,
    remove: gtk::Button,
    queue: RefCell<VecDeque<i64>>,
    scans: RefCell<HashMap<i64, ScanJob>>,
    next_scan: Cell<Instant>,
    next_mount_check: Cell<Instant>,
    data_version: Cell<i64>,
    search: gtk::SearchEntry,
    hits: gio::ListStore,
    selection: gtk::SingleSelection,
    results_panel: gtk::Stack,
    result_actions: gtk::Box,
    empty_title: gtk::Label,
    empty_detail: gtk::Label,
    empty_add: gtk::Button,
    search_error: RefCell<Option<String>>,
    page: gtk::Label,
    previous: gtk::Button,
    next: gtk::Button,
    reveal: gtk::Button,
    open: gtk::Button,
    extract: gtk::Button,
    action_running: Cell<bool>,
    offset: Cell<i64>,
    generation: Cell<u64>,
    rendered: Cell<u64>,
    more: Cell<bool>,
    search_due: Cell<Option<Instant>>,
    requests: Sender<Request>,
    results: Receiver<(u64, SearchResult)>,
    shutdown: Cell<bool>,
}

fn search_worker(database: PathBuf) -> (Sender<Request>, Receiver<(u64, SearchResult)>) {
    let (input, requests) = mpsc::channel::<Request>();
    let (results, output) = mpsc::channel();
    std::thread::spawn(move || {
        let db = archive_search::database(&database);
        while let Ok(mut request) = requests.recv() {
            for newer in requests.try_iter() {
                request = newer;
            }
            let (generation, query, offset) = request;
            let result = match &db {
                Ok(db) => archive_search::search(db, &query, offset),
                Err(error) => Err(anyhow::anyhow!("{error:#}")),
            };
            if results.send((generation, result)).is_err() {
                break;
            }
        }
    });
    (input, output)
}

impl App {
    fn parent(&self) -> &gtk::Window {
        self.window.upcast_ref()
    }

    fn failure(&self, title: &str, error: impl std::fmt::Display) {
        alert(self.parent(), title, &format!("{error}"));
    }

    fn source_model(&self) -> Result<()> {
        let selected = self.selected_source();
        while let Some(child) = self.source_list.first_child() {
            self.source_list.remove(&child);
        }
        let mut rows = self.source_rows.borrow_mut();
        rows.clear();
        for source in archive_search::sources(&self.db)? {
            let row = gtk::ListBoxRow::new();
            let box_ = gtk::Box::new(gtk::Orientation::Vertical, 4);
            box_.set_margin_top(9);
            box_.set_margin_bottom(9);
            box_.set_margin_start(10);
            box_.set_margin_end(10);
            let path = label(&source.path, None);
            path.set_ellipsize(gtk::pango::EllipsizeMode::Start);
            path.set_tooltip_text(Some(&source.path));
            let state = label("", Some("dim-label"));
            state.set_ellipsize(gtk::pango::EllipsizeMode::End);
            box_.append(&path);
            box_.append(&state);
            row.set_child(Some(&box_));
            self.source_list.append(&row);
            if selected == Some(source.id) {
                self.source_list.select_row(Some(&row));
            }
            rows.push((source.id, state, path));
        }
        drop(rows);
        self.source_status()?;
        Ok(())
    }

    fn source_status(&self) -> Result<()> {
        let scans = self.scans.borrow();
        let queue = self.queue.borrow();
        let sources: HashMap<_, _> = archive_search::sources(&self.db)?
            .into_iter()
            .map(|source| (source.id, source))
            .collect();
        for (id, label, _) in self.source_rows.borrow().iter() {
            if let Some(source) = sources.get(id) {
                let text = if scans.contains_key(id) {
                    if source.state == OFFLINE {
                        "Недоступен · повторная проверка…"
                    } else {
                        "Сканирование…"
                    }
                } else if queue.contains(id) && source.state == NEW {
                    "В очереди"
                } else if source.state == ONLINE && !source.detail.is_empty() {
                    &source.detail
                } else {
                    status_text(source.state)
                };
                label.set_text(text);
                label.set_tooltip_text((!source.detail.is_empty()).then_some(&source.detail));
            }
        }
        let active = !scans.is_empty() || !queue.is_empty();
        self.scan_button.set_label(if active {
            "Остановить"
        } else {
            "Обновить индекс"
        });
        self.scan_button.set_sensitive(!sources.is_empty());
        self.scan_status.set_text(if active {
            "Сканирование источников…"
        } else {
            ""
        });
        self.update_results_panel();
        Ok(())
    }

    fn selected_source(&self) -> Option<i64> {
        let row = self.source_list.selected_row()?;
        self.source_rows
            .try_borrow()
            .ok()?
            .get(row.index() as usize)
            .map(|item| item.0)
    }

    fn add_path(&self, path: &str) -> Result<()> {
        let path = path.trim_end_matches('/');
        let path = if path.is_empty() { "/" } else { path };
        let id = archive_search::add_source(&self.db, path)?;
        if !self.queue.borrow().contains(&id) && !self.scans.borrow().contains_key(&id) {
            self.queue.borrow_mut().push_back(id);
        }
        self.source_model()?;
        self.remove.set_sensitive(self.selected_source().is_some());
        Ok(())
    }

    fn all_sources(&self) -> Result<()> {
        let mut queue = self.queue.borrow_mut();
        let scans = self.scans.borrow();
        for source in archive_search::sources(&self.db)? {
            if !queue.contains(&source.id) && !scans.contains_key(&source.id) {
                queue.push_back(source.id);
            }
        }
        self.next_scan.set(Instant::now() + SCAN_INTERVAL);
        Ok(())
    }

    fn stop_scans(&self) {
        self.queue.borrow_mut().clear();
        for (_, mut scan) in self.scans.borrow_mut().drain() {
            scan.stop();
        }
        self.next_scan.set(Instant::now() + SCAN_INTERVAL);
    }

    fn tick_scans(&self, now: Instant) -> Result<()> {
        if now >= self.next_mount_check.get() {
            self.next_mount_check.set(now + Duration::from_secs(5));
            let version = self
                .db
                .query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0))?;
            let changed = self.data_version.replace(version) != version;
            let unmounted = archive_search::mark_unmounted(&self.db)?;
            self.source_status()?;
            if changed || unmounted {
                self.refresh_search();
            }
        }
        if now >= self.next_scan.get() {
            self.all_sources()?;
        }

        let mut completed = Vec::new();
        let mut scans = self.scans.borrow_mut();
        for (id, job) in scans.iter_mut() {
            match job.child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        archive_search::status(&self.db, *id, PARTIAL, &job.error())?;
                    }
                    completed.push(*id);
                }
                Ok(None) => {
                    let last = fs::metadata(job.files.path().join("heartbeat"))
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|time| time.elapsed().ok())
                        .unwrap_or_else(|| job.started.elapsed());
                    if last >= SCAN_TIMEOUT {
                        job.stop();
                        archive_search::status(
                            &self.db,
                            *id,
                            OFFLINE,
                            "Источник не ответил за 60 секунд",
                        )?;
                        completed.push(*id);
                    }
                }
                Err(error) => {
                    job.stop();
                    let detail = format!("Ошибка сканирования: {error}");
                    archive_search::status(&self.db, *id, PARTIAL, &detail)?;
                    completed.push(*id);
                }
            }
        }
        for id in &completed {
            scans.remove(id);
        }
        let mut started = false;
        while scans.len() < MAX_SCANS {
            let Some(id) = self.queue.borrow_mut().pop_front() else {
                break;
            };
            match ScanJob::start(&self.database, id, self.scratch.path()) {
                Ok(job) => {
                    scans.insert(id, job);
                }
                Err(error) => archive_search::status(&self.db, id, PARTIAL, &error.to_string())?,
            }
            started = true;
        }
        drop(scans);
        if started || !completed.is_empty() {
            self.source_status()?;
        }
        if !completed.is_empty() {
            self.refresh_search();
        }
        Ok(())
    }

    fn refresh_search(&self) {
        if self.search_due.get().is_none() {
            self.schedule_search(false);
        }
    }

    fn schedule_search(&self, reset: bool) {
        if reset {
            self.offset.set(0);
            self.generation.set(self.generation.get() + 1);
            self.update_actions();
        }
        self.search_due.set(Some(
            Instant::now() + Duration::from_millis(if reset { 180 } else { 0 }),
        ));
        if reset {
            self.search_error.borrow_mut().take();
            self.update_results_panel();
        }
    }

    fn tick_search(&self, now: Instant) {
        if self.search_due.get().is_some_and(|due| now >= due) {
            self.search_due.set(None);
            let generation = self.generation.get() + 1;
            self.generation.set(generation);
            let _ = self.requests.send((
                generation,
                self.search.text().to_string(),
                self.offset.get(),
            ));
        }
        for (generation, result) in self.results.try_iter() {
            if generation == self.generation.get() {
                match result {
                    Ok((hits, more)) => self.render_hits(generation, hits, more),
                    Err(error) => {
                        self.search_error.replace(Some(error.to_string()));
                        self.update_results_panel();
                        self.update_actions();
                    }
                }
            }
        }
    }

    fn render_hits(&self, generation: u64, hits: Vec<Hit>, more: bool) {
        if hits.is_empty() && self.offset.get() > 0 {
            self.offset.set((self.offset.get() - PAGE_SIZE).max(0));
            self.schedule_search(false);
            return;
        }
        let previous = self.selected_hit();
        let keep = previous.and_then(|old| {
            hits.iter().position(|new| {
                old.archive == new.archive && old.member == new.member && old.ordinal == new.ordinal
            })
        });
        let count = hits.len();
        let rows: Vec<_> = hits.into_iter().map(glib::BoxedAnyObject::new).collect();
        self.hits.splice(0, self.hits.n_items(), &rows);
        self.selection
            .set_selected(keep.map_or(gtk::INVALID_LIST_POSITION, |index| index as u32));
        self.rendered.set(generation);
        self.more.set(more);
        self.previous.set_sensitive(self.offset.get() > 0);
        self.next.set_sensitive(more);
        self.page.set_text(&format!(
            "{}–{}{}",
            self.offset.get() + 1,
            self.offset.get() + count as i64,
            if more { " и далее" } else { "" }
        ));
        self.search_error.borrow_mut().take();
        self.update_results_panel();
        self.update_actions();
    }

    fn update_results_panel(&self) {
        let has_sources = !self.source_rows.borrow().is_empty();
        let query = self.search.text();
        let query_empty = query.trim().is_empty();
        let error = self.search_error.borrow();
        let has_results = has_sources && !query_empty && error.is_none() && self.hits.n_items() > 0;
        self.results_panel
            .set_visible_child_name(if has_results { "results" } else { "empty" });
        self.result_actions.set_visible(has_results);
        let has_pages = has_results && (self.offset.get() > 0 || self.more.get());
        self.page.set_visible(has_pages);
        self.previous.set_visible(has_pages);
        self.next.set_visible(has_pages);
        self.empty_add.set_visible(!has_sources);

        if has_results {
            return;
        }
        let (title, detail) = if !has_sources {
            (
                "Добавьте папку или диск",
                "Archive Scout найдёт файлы внутри ZIP-архивов в добавленных источниках.",
            )
        } else if let Some(error) = error.as_deref() {
            ("Не удалось выполнить поиск", error)
        } else if self.hits.n_items() == 0
            && (!self.scans.borrow().is_empty() || !self.queue.borrow().is_empty())
        {
            (
                "Индексируем архивы",
                "Результаты будут появляться по мере сканирования.",
            )
        } else if query_empty {
            (
                "Найдите файл внутри ZIP",
                "Введите часть имени или пути. Например, отчёт.pdf или документы/2025.",
            )
        } else if self.search_due.get().is_some() || self.generation.get() != self.rendered.get() {
            ("Ищем в ZIP-архивах…", "")
        } else {
            (
                "Ничего не найдено",
                "Попробуйте поискать по короткому фрагменту имени или пути.",
            )
        };
        self.empty_title.set_text(title);
        self.empty_detail.set_text(detail);
        self.empty_detail.set_visible(!detail.is_empty());
    }

    fn selected_hit(&self) -> Option<Hit> {
        let selected = self
            .selection
            .selected_item()?
            .downcast::<glib::BoxedAnyObject>()
            .ok()?;
        let hit = selected.borrow::<Hit>().clone();
        Some(hit)
    }

    fn update_actions(&self) {
        let hit = self.selected_hit();
        let ready = hit.as_ref().is_some_and(|hit| hit.state != OFFLINE)
            && self.search_error.borrow().is_none()
            && self.rendered.get() == self.generation.get()
            && !self.action_running.get();
        self.reveal.set_sensitive(ready);
        self.open.set_sensitive(ready);
        self.extract.set_sensitive(ready);
    }

    fn action(self: &Rc<Self>, hit: Hit, save: Option<gio::File>) {
        if hit.state == OFFLINE || self.action_running.replace(true) {
            return;
        }
        self.update_actions();
        let app = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            let result = app.extract_and_use(hit, save).await;
            app.action_running.set(false);
            app.update_actions();
            if let Err(error) = result {
                app.failure("Действие с файлом не выполнено", format!("{error:#}"));
            }
        });
    }

    async fn extract_and_use(&self, hit: Hit, save: Option<gio::File>) -> Result<()> {
        let directory = Builder::new()
            .prefix("open-")
            .tempdir_in(&self.open_cache)?;
        let payload = directory.path().join("payload");
        fs::create_dir(&payload)?;
        let path = payload.join(archive_search::member_basename(&hit.member));
        let message = directory.path().join("error");
        let mut child = Command::new(std::env::current_exe()?)
            .arg("--extract")
            .arg(&hit.archive)
            .arg(hit.ordinal.to_string())
            .arg(&hit.member)
            .arg(&hit.stamp)
            .arg(&path)
            .stderr(File::create(&message)?)
            .spawn()?;
        loop {
            if self.shutdown.get() {
                let _ = child.kill();
                let _ = child.try_wait();
                return Ok(());
            }
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    let detail = fs::read_to_string(&message).unwrap_or_default();
                    anyhow::bail!("{}", detail.trim());
                }
                break;
            }
            glib::timeout_future(Duration::from_millis(150)).await;
        }
        if let Some(destination) = save {
            let source = gio::File::for_path(&path);
            let input = source.read_future(glib::Priority::DEFAULT).await?;
            let output = destination
                .replace_future(
                    None,
                    false,
                    gio::FileCreateFlags::NONE,
                    glib::Priority::DEFAULT,
                )
                .await?;
            output
                .splice_future(
                    &input,
                    gio::OutputStreamSpliceFlags::CLOSE_SOURCE
                        | gio::OutputStreamSpliceFlags::CLOSE_TARGET,
                    glib::Priority::DEFAULT,
                )
                .await?;
            self.scan_status.set_text("Файл извлечён");
        } else {
            if open_file(&path).await? {
                let _ = directory.keep();
            }
        }
        Ok(())
    }
}

async fn open_file(path: &Path) -> Result<bool> {
    if archive_search::filesystem_root().is_none() {
        let uri = gio::File::for_path(path).uri();
        gio::AppInfo::launch_default_for_uri_future(&uri, gio::AppLaunchContext::NONE).await?;
        return Ok(true);
    }

    let connection = gio::bus_get_future(gio::BusType::Session).await?;
    let client = connection
        .unique_name()
        .context("Нет адреса приложения на D-Bus")?;
    let client = client.trim_start_matches(':').replace('.', "_");
    let token = format!("archive_{}", glib::uuid_string_random().replace('-', "_"));
    let request = format!("/org/freedesktop/portal/desktop/request/{client}/{token}");
    let file = File::open(path)?;
    let files = gio::UnixFDList::new();
    let handle = files.append(&file)?;
    let options = HashMap::from([("handle_token", token.to_variant())]);
    let response = Rc::new(Cell::new(None));
    let reply = Rc::clone(&response);
    let subscription = connection.subscribe_to_signal(
        Some(PORTAL),
        Some("org.freedesktop.portal.Request"),
        Some("Response"),
        Some(&request),
        None,
        gio::DBusSignalFlags::NONE,
        move |signal| {
            reply.set(Some(signal.parameters.child_get::<u32>(0)));
        },
    );
    let parameters = ("", glib::variant::Handle(handle), options).to_variant();
    let call = connection
        .call_with_unix_fd_list_future(
            Some(PORTAL),
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.OpenURI",
            "OpenFile",
            Some(&parameters),
            None,
            gio::DBusCallFlags::NONE,
            5000,
            Some(&files),
        )
        .await;
    if call.is_ok() {
        let deadline = Instant::now() + Duration::from_secs(300);
        while response.get().is_none() && Instant::now() < deadline {
            glib::timeout_future(Duration::from_millis(100)).await;
        }
    }
    drop(subscription);
    call?;
    match response.get() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => anyhow::bail!(
            "Система не смогла открыть файл. Его можно извлечь кнопкой «Извлечь файл…»."
        ),
    }
}

fn show_archive(app: &Rc<App>) {
    let Some(hit) = app.selected_hit() else {
        return;
    };
    let app = Rc::clone(app);
    glib::MainContext::default().spawn_local(async move {
        let attempt: Result<()> = async {
            let bus = gio::bus_get_future(gio::BusType::Session).await?;
            let uri = gio::File::for_path(&hit.archive).uri();
            let arguments = (vec![uri.as_str()], "").to_variant();
            bus.call_future(
                Some("org.freedesktop.FileManager1"),
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1",
                "ShowItems",
                Some(&arguments),
                None,
                gio::DBusCallFlags::NONE,
                5000,
            )
            .await?;
            Ok(())
        }
        .await;
        if let Err(error) = attempt {
            app.failure("Не удалось показать архив в Dolphin", error);
        }
    });
}

fn source_dialog(app: &Rc<App>) {
    let dialog = gtk::Dialog::builder()
        .title("Добавить источник")
        .transient_for(app.parent())
        .modal(true)
        .default_width(560)
        .build();
    dialog.add_button("Отмена", gtk::ResponseType::Cancel);
    let accept = dialog.add_button("Добавить", gtk::ResponseType::Accept);
    accept.add_css_class("suggested-action");
    dialog.set_default_response(gtk::ResponseType::Accept);
    let content = dialog.content_area();
    content.set_spacing(12);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    content.append(&label(
        "Выберите папку или корень смонтированного диска",
        None,
    ));

    let path_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let entry = gtk::Entry::builder()
        .placeholder_text("/mnt/архивы")
        .hexpand(true)
        .activates_default(true)
        .build();
    let browse = button("Выбрать…", None);
    path_row.append(&entry);
    path_row.append(&browse);
    content.append(&path_row);

    browse.connect_clicked({
        let dialog = dialog.clone();
        let entry = entry.clone();
        move |_| {
            let chooser = gtk::FileChooserDialog::new(
                Some("Выбрать папку"),
                Some(&dialog),
                gtk::FileChooserAction::SelectFolder,
                &[
                    ("Отмена", gtk::ResponseType::Cancel),
                    ("Выбрать", gtk::ResponseType::Accept),
                ],
            );
            let prefix = archive_search::filesystem_root();
            let home = glib::home_dir();
            let initial = match &prefix {
                Some(prefix) => prefix.join(home.strip_prefix("/").unwrap_or(&home)),
                None => home,
            };
            let _ = chooser.set_current_folder(Some(&gio::File::for_path(initial)));
            chooser.connect_response({
                let entry = entry.clone();
                move |chooser, response| {
                    if response == gtk::ResponseType::Accept {
                        if let Some(path) = chooser.file().and_then(|file| file.path()) {
                            let path = if let Some(prefix) = &prefix {
                                path.strip_prefix(prefix)
                                    .ok()
                                    .map(|path| Path::new("/").join(path))
                            } else {
                                Some(path)
                            };
                            if let Some(path) =
                                path.and_then(|path| path.into_os_string().into_string().ok())
                            {
                                entry.set_text(&path);
                            } else {
                                alert(
                                    &chooser.clone().upcast::<gtk::Window>(),
                                    "Выберите папку на диске",
                                    "Для смонтированной папки можно ввести абсолютный путь.",
                                );
                                return;
                            }
                        }
                    }
                    chooser.close();
                }
            });
            chooser.present();
        }
    });
    dialog.connect_response({
        let app = Rc::clone(app);
        let entry = entry.clone();
        move |dialog, response| {
            if response == gtk::ResponseType::Accept {
                let text = entry.text();
                if !text.starts_with('/') {
                    alert(
                        dialog.upcast_ref(),
                        "Укажите абсолютный путь",
                        "Например, /mnt/архивы",
                    );
                    return;
                }
                if let Err(error) = app.add_path(&text) {
                    alert(
                        dialog.upcast_ref(),
                        "Не удалось добавить источник",
                        &format!("{error:#}"),
                    );
                    return;
                }
            }
            dialog.close();
        }
    });
    GtkWindowExt::set_focus(&dialog, Some(&entry));
    dialog.present();
}

fn save_dialog(app: &Rc<App>) {
    let Some(hit) = app.selected_hit() else {
        return;
    };
    let dialog = gtk::FileChooserNative::new(
        Some("Извлечь файл"),
        Some(app.parent()),
        gtk::FileChooserAction::Save,
        Some("Извлечь"),
        Some("Отмена"),
    );
    dialog.set_current_name(archive_search::member_basename(&hit.member));
    dialog.connect_response({
        let app = Rc::clone(app);
        move |dialog, response| {
            let destination = (response == gtk::ResponseType::Accept)
                .then(|| dialog.file())
                .flatten();
            dialog.destroy();
            if destination.is_some() {
                app.action(hit.clone(), destination);
            }
        }
    });
    dialog.show();
}

fn add_column(
    view: &gtk::ColumnView,
    title: &str,
    width: i32,
    expand: bool,
    cell: fn(&Hit) -> String,
) {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let label = label("", None);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_single_line_mode(true);
        label.set_margin_top(5);
        label.set_margin_bottom(5);
        item.set_child(Some(&label));
    });
    let title_is_status = title == "Состояние";
    factory.connect_bind(move |_, item| {
        let boxed = item
            .item()
            .and_downcast::<glib::BoxedAnyObject>()
            .expect("hit");
        let hit = boxed.borrow::<Hit>();
        let label = item.child().and_downcast::<gtk::Label>().expect("cell");
        let text = cell(&hit);
        label.set_text(&text);
        let tooltip = if title_is_status && !hit.detail.is_empty() {
            &hit.detail
        } else {
            &text
        };
        label.set_tooltip_text((!tooltip.is_empty()).then_some(tooltip));
        if hit.state == OFFLINE {
            label.add_css_class("dim-label");
        } else {
            label.remove_css_class("dim-label");
        }
    });
    let column = gtk::ColumnViewColumn::new(Some(title), Some(factory));
    column.set_fixed_width(width);
    column.set_resizable(true);
    column.set_expand(expand);
    view.append_column(&column);
}

fn create_window(application: &gtk::Application) -> Result<Rc<App>> {
    let database = glib::user_data_dir().join(APP_ID).join("index.sqlite3");
    let db = archive_search::initialize(&database)?;
    let cache = glib::user_cache_dir().join(APP_ID);
    fs::create_dir_all(&cache)?;
    let scratch = Builder::new().prefix("session-").tempdir_in(&cache)?;
    let open_cache = cache.join("opened");
    fs::create_dir_all(&open_cache)?;
    for entry in fs::read_dir(&open_cache)?.flatten() {
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|time: SystemTime| time.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(86400));
        if old && entry.file_name().to_string_lossy().starts_with("open-") {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    let (requests, results) = search_worker(database.clone());

    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .title(TITLE)
        .default_width(1060)
        .default_height(640)
        .build();
    let header = gtk::HeaderBar::new();
    header.set_title_widget(Some(&label(TITLE, Some("title"))));
    let scan_button = button("Обновить индекс", None);
    header.pack_end(&scan_button);
    window.set_titlebar(Some(&header));

    let outer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 12);
    sidebar.set_width_request(250);
    sidebar.set_margin_top(16);
    sidebar.set_margin_bottom(16);
    sidebar.set_margin_start(14);
    sidebar.set_margin_end(14);
    sidebar.append(&label("Источники", Some("heading")));

    let source_list = gtk::ListBox::new();
    source_list.add_css_class("boxed-list");
    let empty = label("Добавьте диск или папку", Some("dim-label"));
    empty.set_margin_top(12);
    empty.set_margin_start(4);
    source_list.set_placeholder(Some(&empty));
    let source_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&source_list)
        .build();
    sidebar.append(&source_scroll);
    let source_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let add = button("Добавить…", Some("suggested-action"));
    let remove = button("Удалить", None);
    remove.set_sensitive(false);
    source_actions.append(&add);
    source_actions.append(&remove);
    sidebar.append(&source_actions);

    let main = gtk::Box::new(gtk::Orientation::Vertical, 12);
    main.set_hexpand(true);
    main.set_margin_top(16);
    main.set_margin_bottom(12);
    main.set_margin_start(16);
    main.set_margin_end(16);
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Имя файла или путь внутри ZIP")
        .hexpand(true)
        .build();
    main.append(&search);
    let hits = gio::ListStore::new::<glib::BoxedAnyObject>();
    let selection = gtk::SingleSelection::new(Some(hits.clone()));
    selection.set_autoselect(false);
    selection.set_can_unselect(true);
    let view = gtk::ColumnView::new(Some(selection.clone()));
    view.set_show_row_separators(true);
    add_column(&view, "Архив", 150, false, |hit| {
        Path::new(&hit.archive)
            .file_name()
            .and_then(|part| part.to_str())
            .unwrap_or(&hit.archive)
            .to_owned()
    });
    add_column(
        &view,
        "Расположение архива",
        220,
        true,
        |hit| {
            Path::new(&hit.archive)
                .parent()
                .map_or_else(String::new, |path| path.display().to_string())
        },
    );
    add_column(&view, "Путь внутри ZIP", 240, true, |hit| {
        hit.member.clone()
    });
    add_column(&view, "Состояние", 105, false, |hit| {
        if hit.state == ONLINE {
            String::new()
        } else if hit.state == OFFLINE {
            "Недоступен".into()
        } else {
            "Из индекса".into()
        }
    });
    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .has_frame(true)
        .child(&view)
        .build();
    let empty = gtk::Box::new(gtk::Orientation::Vertical, 10);
    empty.set_halign(gtk::Align::Center);
    empty.set_valign(gtk::Align::Center);
    empty.set_margin_start(20);
    empty.set_margin_end(20);
    let empty_icon = gtk::Image::from_icon_name("system-search-symbolic");
    empty_icon.set_pixel_size(40);
    empty_icon.add_css_class("dim-label");
    empty.append(&empty_icon);
    let empty_title = label("", Some("title-2"));
    empty_title.set_xalign(0.5);
    empty.append(&empty_title);
    let empty_detail = label("", Some("dim-label"));
    empty_detail.set_xalign(0.5);
    empty_detail.set_justify(gtk::Justification::Center);
    empty_detail.set_wrap(true);
    empty_detail.set_max_width_chars(50);
    empty.append(&empty_detail);
    let empty_add = button("Добавить источник…", Some("suggested-action"));
    empty_add.set_halign(gtk::Align::Center);
    empty_add.set_margin_top(6);
    empty.append(&empty_add);

    let results_panel = gtk::Stack::new();
    results_panel.set_vexpand(true);
    results_panel.add_named(&empty, Some("empty"));
    results_panel.add_named(&scroll, Some("results"));
    main.append(&results_panel);

    let scan_status = label("", Some("dim-label"));
    let page = label("", Some("dim-label"));
    page.set_visible(false);
    let previous = button("←", None);
    previous.set_tooltip_text(Some("Предыдущие результаты"));
    previous.set_sensitive(false);
    let next = button("→", None);
    next.set_tooltip_text(Some("Следующие результаты"));
    next.set_sensitive(false);
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    scan_status.set_hexpand(true);
    scan_status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    footer.append(&scan_status);
    footer.append(&page);
    footer.append(&previous);
    footer.append(&next);
    main.append(&footer);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let reveal = button("Показать архив в Dolphin", None);
    let open = button("Открыть файл", Some("suggested-action"));
    let extract = button("Извлечь файл…", None);
    actions.append(&open);
    actions.append(&extract);
    actions.append(&reveal);
    main.append(&actions);
    outer.append(&sidebar);
    outer.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    outer.append(&main);
    window.set_child(Some(&outer));

    let app = Rc::new(App {
        window,
        db,
        database,
        scratch,
        open_cache,
        source_list,
        source_rows: Default::default(),
        scan_button,
        scan_status,
        remove,
        queue: Default::default(),
        scans: Default::default(),
        next_scan: Cell::new(Instant::now()),
        next_mount_check: Cell::new(Instant::now()),
        data_version: Cell::new(0),
        search,
        hits,
        selection,
        results_panel,
        result_actions: actions,
        empty_title,
        empty_detail,
        empty_add,
        search_error: RefCell::new(None),
        page,
        previous,
        next,
        reveal,
        open,
        extract,
        action_running: Cell::new(false),
        offset: Cell::new(0),
        generation: Cell::new(0),
        rendered: Cell::new(0),
        more: Cell::new(false),
        search_due: Cell::new(None),
        requests,
        results,
        shutdown: Cell::new(false),
    });

    app.source_model()?;
    app.update_actions();
    add.connect_clicked({
        let app = Rc::downgrade(&app);
        move |_| {
            if let Some(app) = app.upgrade() {
                source_dialog(&app);
            }
        }
    });
    app.empty_add.connect_clicked(move |_| add.emit_clicked());
    app.source_list.connect_row_selected({
        let app = Rc::downgrade(&app);
        move |_, row| {
            if let Some(app) = app.upgrade() {
                app.remove.set_sensitive(row.is_some());
            }
        }
    });
    app.remove.connect_clicked({
        let app = Rc::downgrade(&app);
        move |_| {
            let Some(app) = app.upgrade() else { return };
            let Some(id) = app.selected_source() else {
                return;
            };
            if let Some(mut job) = app.scans.borrow_mut().remove(&id) {
                job.stop();
            }
            app.queue.borrow_mut().retain(|queued| *queued != id);
            let result = app
                .db
                .execute("DELETE FROM sources WHERE id=?", [id])
                .context("Не удалось удалить источник")
                .and_then(|_| app.source_model());
            if let Err(error) = result {
                app.failure("Источник не удалён", error);
            }
            app.refresh_search();
        }
    });
    app.scan_button.connect_clicked({
        let app = Rc::downgrade(&app);
        move |_| {
            let Some(app) = app.upgrade() else { return };
            let result = if app.scans.borrow().is_empty() && app.queue.borrow().is_empty() {
                app.all_sources()
            } else {
                app.stop_scans();
                Ok(())
            };
            if let Err(error) = result.and_then(|_| app.source_status()) {
                app.failure("Не удалось обновить индекс", error);
            }
        }
    });
    app.search.connect_search_changed({
        let app = Rc::downgrade(&app);
        move |_| {
            if let Some(app) = app.upgrade() {
                app.schedule_search(true);
            }
        }
    });
    app.selection.connect_selected_notify({
        let app = Rc::downgrade(&app);
        move |_| {
            if let Some(app) = app.upgrade() {
                app.update_actions();
            }
        }
    });
    app.previous.connect_clicked({
        let app = Rc::downgrade(&app);
        move |_| {
            if let Some(app) = app.upgrade() {
                app.offset.set((app.offset.get() - PAGE_SIZE).max(0));
                app.schedule_search(false);
            }
        }
    });
    app.next.connect_clicked({
        let app = Rc::downgrade(&app);
        move |_| {
            if let Some(app) = app.upgrade().filter(|app| app.more.get()) {
                app.offset.set(app.offset.get() + PAGE_SIZE);
                app.schedule_search(false);
            }
        }
    });
    app.reveal.connect_clicked({
        let app = Rc::downgrade(&app);
        move |_| {
            if let Some(app) = app.upgrade() {
                show_archive(&app);
            }
        }
    });
    app.open.connect_clicked({
        let app = Rc::downgrade(&app);
        move |_| {
            if let Some(app) = app.upgrade() {
                if let Some(hit) = app.selected_hit() {
                    app.action(hit, None);
                }
            }
        }
    });
    app.extract.connect_clicked({
        let app = Rc::downgrade(&app);
        move |_| {
            if let Some(app) = app.upgrade() {
                save_dialog(&app);
            }
        }
    });
    view.connect_activate({
        let app = Rc::downgrade(&app);
        move |_, position| {
            if let Some(app) = app.upgrade() {
                app.selection.set_selected(position);
                if app.open.is_sensitive() {
                    if let Some(hit) = app.selected_hit() {
                        app.action(hit, None);
                    }
                }
            }
        }
    });
    app.window.connect_close_request({
        let app = Rc::downgrade(&app);
        move |_| {
            if let Some(app) = app.upgrade() {
                app.shutdown.set(true);
                app.stop_scans();
            }
            glib::Propagation::Proceed
        }
    });
    app.window.present();
    let _ = app.search.grab_focus();
    let state = Rc::clone(&app);
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let app = &state;
        if app.shutdown.get() {
            return ControlFlow::Break;
        }
        let now = Instant::now();
        if let Err(error) = app.tick_scans(now) {
            app.scan_status
                .set_text(&format!("Ошибка индекса: {error}"));
        }
        app.tick_search(now);
        ControlFlow::Continue
    });
    Ok(app)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn wait_until(predicate: impl Fn() -> bool) {
        let context = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            while context.pending() {
                context.iteration(false);
            }
            if predicate() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(predicate(), "GTK window did not reach the expected state");
    }

    #[test]
    #[ignore = "requires an isolated XDG data directory and Wayland display"]
    fn wayland_results_and_offline_actions() -> Result<()> {
        gtk::init()?;
        let display = gtk::gdk::Display::default().context("Missing Wayland display")?;
        assert_eq!(display.type_().name(), "GdkWaylandDisplay");
        let application = gtk::Application::builder()
            .application_id(APP_ID)
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(gio::Cancellable::NONE)?;
        let app = create_window(&application)?;
        app.next_scan.set(Instant::now() + Duration::from_secs(60));
        assert_eq!(
            app.results_panel.visible_child_name().as_deref(),
            Some("empty")
        );
        assert!(app.empty_add.get_visible());
        assert!(!app.result_actions.get_visible());

        let fixture = tempfile::tempdir()?;
        let folder = fixture.path().join("source");
        fs::create_dir(&folder)?;
        let mut zip = zip::ZipWriter::new(File::create(folder.join("documents.zip"))?);
        zip.start_file(
            "Документы/Тест.txt",
            zip::write::SimpleFileOptions::default(),
        )?;
        zip.write_all(b"wayland fixture")?;
        zip.finish()?;
        app.add_path(archive_search::text_path(&folder)?)?;
        assert_eq!(app.empty_title.text(), "Индексируем архивы");
        let id = app
            .queue
            .borrow_mut()
            .pop_front()
            .context("Missing source")?;
        let mut archive_db = archive_search::database(&app.database)?;
        let heartbeat = fixture.path().join("heartbeat");
        archive_search::scan(&mut archive_db, id, None, &heartbeat)?;
        app.source_status()?;
        assert_eq!(app.empty_title.text(), "Найдите файл внутри ZIP");
        app.search.set_text("нет такого файла");
        wait_until(|| app.empty_title.text() == "Ничего не найдено");
        assert!(!app.result_actions.get_visible());
        app.search.set_text("документы/тест");
        wait_until(|| app.hits.n_items() == 1);
        assert_eq!(
            app.results_panel.visible_child_name().as_deref(),
            Some("results")
        );
        assert!(app.result_actions.get_visible());
        app.selection.set_selected(0);
        assert!(app.open.is_sensitive());
        assert!(app.extract.is_sensitive());
        assert!(app.reveal.is_sensitive());
        assert_eq!(app.selected_hit().unwrap().member, "Документы/Тест.txt");

        fs::rename(folder, fixture.path().join("offline"))?;
        archive_search::scan(&mut archive_db, id, None, &heartbeat)?;
        app.refresh_search();
        wait_until(|| app.selected_hit().is_some_and(|hit| hit.state == OFFLINE));
        assert!(!app.open.is_sensitive());
        assert!(!app.extract.is_sensitive());
        assert_eq!(app.hits.n_items(), 1);
        app.window.close();
        Ok(())
    }
}
