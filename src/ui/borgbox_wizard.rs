//! "New on BorgBox" quick setup (BorgMac's `BorgBoxQuickSetupSheet`): a
//! stand-alone singleton window that first registers a BorgBox server and
//! then creates a repo on it in one step — dedicated SSH key, `POST /repos`
//! on the daemon, `borg init` over SSH, save locally. Existing repos on the
//! server can be imported from the same window.
use super::borgbox_alerts::sheet_header;
use super::borgbox_import;
use super::kit::{self, icons};
use super::state::{AppState, connect_while_alive};
use crate::core::borgbox::{self, BorgBoxRemoteRepo};
use crate::core::models::{BorgBoxServer, Repository};
use crate::core::{borg, format, sshkeys};
use adw::prelude::*;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use uuid::Uuid;

/// Marks this wizard among the other `borg-wizard` windows.
const WINDOW_CLASS: &str = "borgbox-wizard";
const CONTENT_WIDTH: i32 = 620;
const CONTENT_HEIGHT: i32 = 720;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Idle,
    ValidatingServer,
    GeneratingKey,
    RegisteringRepo,
    InitRepo,
    SavingLocally,
    Done,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Step::Idle => "",
            Step::ValidatingServer => "Validating server…",
            Step::GeneratingKey => "Generating a dedicated SSH key…",
            Step::RegisteringRepo => "Registering repo on the server (POST /repos)…",
            Step::InitRepo => "Initializing the Borg repository (borg init)…",
            Step::SavingLocally => "Saving to Luna…",
            Step::Done => "Done.",
        }
    }

    fn is_running(self) -> bool {
        !matches!(self, Step::Idle | Step::Done)
    }
}

struct Wizard {
    app: Rc<AppState>,
    window: adw::Window,
    subtitle: gtk::Label,
    forms: gtk::Stack,

    // Server form
    server_name: adw::EntryRow,
    daemon_url: adw::EntryRow,
    token: adw::PasswordEntryRow,
    ssh_host_override: adw::EntryRow,
    ssh_user: adw::EntryRow,
    ssh_port: adw::EntryRow,

    // Repo form
    server_row: adw::ActionRow,
    remote_group: adw::PreferencesGroup,
    remote_rows: RefCell<Vec<gtk::Widget>>,
    repo_name: adw::EntryRow,
    passphrase: adw::PasswordEntryRow,
    passphrase_confirm: adw::PasswordEntryRow,

    progress: gtk::Box,
    progress_spinner: gtk::Spinner,
    progress_done: gtk::Image,
    progress_label: gtk::Label,
    forget: gtk::Button,
    cancel: gtk::Button,
    primary: gtk::Button,

    /// Existing repos on the daemon, fetched once a server is configured.
    remote_repos: RefCell<Vec<BorgBoxRemoteRepo>>,
    loading_remote_repos: Cell<bool>,
    /// The server the remote repo list was last requested for.
    listed_server: Cell<Option<Uuid>>,
    step: Cell<Step>,
}

/// Opens (or focuses) the wizard window.
pub fn open(app: &Rc<AppState>) {
    let existing = app.application.windows().into_iter().find(|w| w.has_css_class(WINDOW_CLASS));
    match existing {
        Some(window) => window.present(),
        None => build(app).present(),
    }
}

fn lights() -> gtk::WindowControls {
    let controls = gtk::WindowControls::new(gtk::PackType::Start);
    controls.set_decoration_layout(Some("close,minimize,maximize:"));
    controls.add_css_class("borg-lights");
    controls.set_valign(gtk::Align::Center);
    controls
}

fn section(title: &str) -> adw::PreferencesGroup {
    let group = kit::form_group();
    if !title.is_empty() {
        group.set_title(title);
    }
    group
}

fn build(app: &Rc<AppState>) -> adw::Window {
    let window = adw::Window::builder()
        .application(&app.application)
        .title("New on BorgBox")
        .resizable(false)
        .build();
    window.add_css_class("borg-wizard");
    window.add_css_class(WINDOW_CLASS);

    let titlebar = adw::HeaderBar::new();
    titlebar.add_css_class("bb2-wizard-titlebar");
    titlebar.set_show_start_title_buttons(false);
    titlebar.set_show_end_title_buttons(false);
    titlebar.pack_start(&lights());
    titlebar.set_title_widget(Some(&kit::label("New on BorgBox", &["borg-title"])));

    let content = kit::vbox(16);
    content.set_margin_top(20);
    content.set_margin_bottom(20);
    content.set_margin_start(20);
    content.set_margin_end(20);
    content.set_size_request(CONTENT_WIDTH - 40, CONTENT_HEIGHT - 40);

    let (header, subtitle) = sheet_header(icons::SERVER, "New repository on BorgBox", "", false);
    content.append(&header);
    content.append(&kit::divider());

    // ----------------------------------------------------------- server form
    let server_form = kit::vbox(18);
    let server_group = section("Server");
    let server_name = adw::EntryRow::builder().title("Name (e.g. Home NAS)").text("My BorgBox server").build();
    let daemon_url =
        adw::EntryRow::builder().title("Daemon URL (http://host:9999)").text("http://hive.local:9999").build();
    daemon_url.set_input_hints(gtk::InputHints::NO_SPELLCHECK);
    let token = adw::PasswordEntryRow::builder().title("API token (Bearer)").build();
    server_group.add(&server_name);
    server_group.add(&daemon_url);
    server_group.add(&token);
    server_form.append(&server_group);

    let advanced_group = section("");
    let advanced = adw::ExpanderRow::builder().title("SSH (advanced)").build();
    let ssh_host_override = adw::EntryRow::new();
    ssh_host_override.set_input_hints(gtk::InputHints::NO_SPELLCHECK);
    let ssh_user = adw::EntryRow::builder().title("SSH user").text("borg").build();
    let ssh_port = adw::EntryRow::builder().title("SSH port").text("22").build();
    advanced.add_row(&ssh_host_override);
    advanced.add_row(&ssh_user);
    advanced.add_row(&ssh_port);
    advanced_group.add(&advanced);
    server_form.append(&advanced_group);
    let server_help = kit::caption_markup(
        "The token is stored in the keyring. The app checks <tt>/health</tt> and <tt>/info</tt> before saving to confirm the server is reachable and the token is valid.",
    );
    server_help.set_margin_start(4);
    server_form.append(&server_help);

    // ------------------------------------------------------------- repo form
    let repo_form = kit::vbox(18);
    let current_group = section("Server");
    let server_row = adw::ActionRow::new();
    server_row.add_css_class("bb2-server-row");
    server_row.set_subtitle_lines(1);
    server_row.add_prefix(&kit::icon_styled(icons::SERVER, &["tint"]));
    current_group.add(&server_row);
    repo_form.append(&current_group);

    let remote_group = section("Existing repos on the server");
    repo_form.append(&remote_group);

    let create_group = section("Create a new repo");
    let repo_name = adw::EntryRow::builder().title("Name — e.g. mac (lowercase, digits, hyphens)").build();
    repo_name.set_input_hints(gtk::InputHints::NO_SPELLCHECK | gtk::InputHints::LOWERCASE);
    let passphrase = adw::PasswordEntryRow::builder().title("Passphrase").build();
    let passphrase_confirm = adw::PasswordEntryRow::builder().title("Repeat passphrase").build();
    create_group.add(&repo_name);
    create_group.add(&passphrase);
    create_group.add(&passphrase_confirm);
    let create_section = kit::vbox(6);
    create_section.append(&create_group);
    let repo_help =
        kit::caption("The daemon validates the name: lowercase, digits and hyphens, 63 characters max.");
    repo_help.set_margin_start(4);
    create_section.append(&repo_help);
    repo_form.append(&create_section);

    let forms = gtk::Stack::new();
    forms.set_vhomogeneous(false);
    for (name, form) in [("server", &server_form), ("repo", &repo_form)] {
        form.set_margin_end(4);
        form.set_margin_bottom(4);
        forms.add_named(&kit::scrolled(form), Some(name));
    }
    forms.set_vexpand(true);
    content.append(&forms);

    // ------------------------------------------------------ progress / footer
    let progress = kit::vbox(16);
    progress.append(&kit::divider());
    let progress_row = kit::hbox(10);
    let progress_spinner = kit::spinner();
    let progress_done = kit::icon_styled(icons::CHECK_CIRCLE, &["c-green"]);
    let progress_label = kit::label("", &["secondary"]);
    progress_row.append(&progress_spinner);
    progress_row.append(&progress_done);
    progress_row.append(&progress_label);
    progress.append(&progress_row);
    content.append(&progress);

    let forget = kit::button("Forget server");
    forget.add_css_class("flat");
    forget.set_tooltip_text(Some("Remove the server and its token from this computer."));
    let cancel = kit::button("Cancel");
    let primary = kit::primary_button("");
    let footer = kit::hbox(8);
    footer.append(&forget);
    footer.append(&kit::spacer());
    footer.append(&cancel);
    footer.append(&primary);
    content.append(&footer);

    let view = adw::ToolbarView::new();
    view.add_top_bar(&titlebar);
    view.set_content(Some(&content));
    window.set_content(Some(&view));
    window.set_default_widget(Some(&primary));

    let wizard = Rc::new(Wizard {
        app: app.clone(),
        window: window.clone(),
        subtitle,
        forms,
        server_name,
        daemon_url,
        token,
        ssh_host_override,
        ssh_user,
        ssh_port,
        server_row,
        remote_group,
        remote_rows: RefCell::new(vec![]),
        repo_name,
        passphrase,
        passphrase_confirm,
        progress,
        progress_spinner,
        progress_done,
        progress_label,
        forget,
        cancel,
        primary,
        remote_repos: RefCell::new(vec![]),
        loading_remote_repos: Cell::new(false),
        listed_server: Cell::new(None),
        step: Cell::new(Step::Idle),
    });

    // ------------------------------------------------------------ behaviour
    let on_input = glib::clone!(
        #[weak]
        wizard,
        move || update_controls(&wizard)
    );
    for row in [&wizard.server_name, &wizard.daemon_url, &wizard.repo_name] {
        row.connect_changed(glib::clone!(
            #[strong]
            on_input,
            move |_| on_input()
        ));
    }
    for row in [&wizard.token, &wizard.passphrase, &wizard.passphrase_confirm] {
        row.connect_changed(glib::clone!(
            #[strong]
            on_input,
            move |_| on_input()
        ));
    }

    wizard.forget.connect_clicked(glib::clone!(
        #[weak]
        wizard,
        move |_| {
            if let Some(server) = wizard.app.borgbox.first() {
                wizard.app.borgbox.remove(&server);
            }
        }
    ));
    wizard.cancel.connect_clicked(glib::clone!(
        #[weak]
        window,
        move |_| window.close()
    ));
    wizard.primary.connect_clicked(glib::clone!(
        #[weak]
        wizard,
        move |_| match wizard.app.borgbox.first() {
            None => add_server(&wizard),
            Some(server) => run_repo_wizard(&wizard, server),
        }
    ));

    connect_while_alive(
        app,
        |a| &a.borgbox.changed,
        &window,
        glib::clone!(
            #[weak]
            wizard,
            move || sync_server(&wizard)
        ),
    );
    // "Already imported" depends on the local repositories.
    connect_while_alive(
        app,
        |a| &a.repos.changed,
        &window,
        glib::clone!(
            #[weak]
            wizard,
            move || rebuild_remote_repos(&wizard)
        ),
    );

    // Handlers only hold weak references; the wizard state lives until the
    // window goes away.
    let keep = RefCell::new(Some(wizard.clone()));
    window.connect_destroy(move |_| {
        keep.borrow_mut().take();
    });

    sync_server(&wizard);
    window
}

// MARK: - State → widgets

/// Shows the form matching the store (server form until a server exists)
/// and reloads the remote repo list when the current server changed.
fn sync_server(wizard: &Rc<Wizard>) {
    let server = wizard.app.borgbox.first();
    match &server {
        None => {
            wizard.subtitle.set_text("First, configure your BorgBox server.");
            wizard.forms.set_visible_child_name("server");
            wizard.listed_server.set(None);
        }
        Some(server) => {
            wizard
                .subtitle
                .set_text("Generate an SSH key, register with the daemon, and run borg init in one step.");
            wizard.server_row.set_title(&glib::markup_escape_text(&server.name));
            wizard.server_row.set_subtitle(&glib::markup_escape_text(&format!(
                "{} — ssh: {}@{}:{}",
                server.daemon_url, server.ssh_user, server.ssh_host, server.ssh_port
            )));
            wizard.forms.set_visible_child_name("repo");
            if wizard.listed_server.get() != Some(server.id) {
                wizard.listed_server.set(Some(server.id));
                load_remote_repos(wizard, server);
            }
        }
    }
    update_controls(wizard);
}

fn can_add_server(wizard: &Wizard) -> bool {
    wizard.step.get() == Step::Idle
        && !wizard.server_name.text().trim().is_empty()
        && !wizard.daemon_url.text().trim().is_empty()
        && !wizard.token.text().trim().is_empty()
}

fn can_create_repo(wizard: &Wizard) -> bool {
    let passphrase = wizard.passphrase.text();
    wizard.step.get() == Step::Idle
        && !wizard.repo_name.text().trim().is_empty()
        && !passphrase.is_empty()
        && passphrase == wizard.passphrase_confirm.text()
}

fn update_controls(wizard: &Wizard) {
    let step = wizard.step.get();
    let has_server = wizard.app.borgbox.first().is_some();

    // The placeholder of the override field is the host derived from the URL.
    let auto = auto_ssh_host(&wizard.daemon_url.text());
    wizard.ssh_host_override.set_title(&glib::markup_escape_text(&if auto.is_empty() {
        "SSH host (empty = auto from URL)".to_string()
    } else {
        format!("SSH host (empty = auto from URL: {auto})")
    }));

    wizard.progress.set_visible(step != Step::Idle);
    wizard.progress_spinner.set_visible(step.is_running());
    wizard.progress_done.set_visible(step == Step::Done);
    wizard.progress_label.set_text(step.label());

    wizard.forget.set_visible(has_server);
    wizard.forget.set_sensitive(!step.is_running());
    wizard.cancel.set_sensitive(!step.is_running());
    if has_server {
        wizard.primary.set_label("Create repo");
        wizard.primary.set_sensitive(can_create_repo(wizard));
    } else {
        wizard.primary.set_label("Save and connect");
        wizard.primary.set_sensitive(can_add_server(wizard));
    }
}

fn set_step(wizard: &Wizard, step: Step) {
    wizard.step.set(step);
    update_controls(wizard);
}

/// Reports a failure in a modal alert and returns the wizard to idle.
fn fail(wizard: &Wizard, message: &str) {
    set_step(wizard, Step::Idle);
    if wizard.window.is_visible() {
        present_attached(&wizard.window, || kit::alert(&wizard.window, "Error", message));
    }
}

/// Runs `present` (which shows a sheet or alert over `window`) so that the
/// dialog is attached to the window. libadwaita only does that for
/// resizable windows — otherwise the dialog becomes a window of its own —
/// so the fixed-size wizard is resizable for as long as the dialog is up.
fn present_attached(window: &adw::Window, present: impl FnOnce()) {
    window.set_resizable(true);
    present();
    match window.visible_dialog() {
        Some(dialog) => {
            dialog.connect_closed(glib::clone!(
                #[weak]
                window,
                move |_| window.set_resizable(window.visible_dialog().is_some())
            ));
        }
        None => window.set_resizable(false),
    }
}

fn is_already_imported(wizard: &Wizard, remote: &BorgBoxRemoteRepo) -> bool {
    let Some(server) = wizard.app.borgbox.first() else { return false };
    wizard
        .app
        .repos
        .repositories()
        .iter()
        .any(|local| local.url.contains(&server.ssh_host) && local.url.ends_with(&remote.path))
}

fn rebuild_remote_repos(wizard: &Rc<Wizard>) {
    for row in wizard.remote_rows.borrow_mut().drain(..) {
        wizard.remote_group.remove(&row);
    }
    let mut rows: Vec<gtk::Widget> = vec![];
    let remotes = wizard.remote_repos.borrow().clone();
    if wizard.loading_remote_repos.get() {
        let row = adw::ActionRow::new();
        let loading = kit::hbox(8);
        let spinner = kit::spinner();
        spinner.set_valign(gtk::Align::Center);
        loading.append(&spinner);
        loading.append(&kit::label("Loading…", &["caption", "secondary"]));
        row.add_prefix(&loading);
        rows.push(row.upcast());
    } else if remotes.is_empty() {
        let row = adw::ActionRow::new();
        row.add_prefix(&kit::label("None yet.", &["caption", "secondary"]));
        rows.push(row.upcast());
    } else {
        for remote in remotes {
            let row = adw::ActionRow::builder().title(glib::markup_escape_text(&remote.name)).build();
            row.add_css_class("bb2-remote-row");
            if let Some(size) = remote.size_bytes {
                row.set_subtitle(&format::bytes_binary(size));
            }
            row.add_prefix(&kit::icon_styled(icons::DRIVE, &["tint"]));
            if is_already_imported(wizard, &remote) {
                row.add_suffix(&kit::label("Already imported", &["caption", "secondary"]));
            } else {
                let import = kit::button("Import");
                import.add_css_class("bb2-small");
                import.set_valign(gtk::Align::Center);
                import.connect_clicked(glib::clone!(
                    #[weak]
                    wizard,
                    move |_| {
                        if let Some(server) = wizard.app.borgbox.first() {
                            let present = || borgbox_import::present(
                                &wizard.window,
                                &wizard.app,
                                &server,
                                &remote.name,
                                &remote.path,
                                None,
                                borgbox_import::Mode::ExistingKey,
                            );
                            present_attached(&wizard.window, present);
                        }
                    }
                ));
                row.add_suffix(&import);
            }
            rows.push(row.upcast());
        }
    }
    for row in &rows {
        wizard.remote_group.add(row);
    }
    *wizard.remote_rows.borrow_mut() = rows;
}

fn load_remote_repos(wizard: &Rc<Wizard>, server: &BorgBoxServer) {
    wizard.loading_remote_repos.set(true);
    rebuild_remote_repos(wizard);
    let server = server.clone();
    let server_id = server.id;
    let weak = Rc::downgrade(wizard);
    kit::bg(
        move || borgbox::list_repos(&server),
        move |result| {
            let Some(wizard) = weak.upgrade() else { return };
            if wizard.listed_server.get() != Some(server_id) {
                return; // the server was forgotten or replaced meanwhile
            }
            wizard.loading_remote_repos.set(false);
            // Non-fatal: the user can still create a new repo even if the
            // listing failed. Swallowed silently so a daemon hiccup doesn't
            // ruin the wizard flow.
            *wizard.remote_repos.borrow_mut() = result.unwrap_or_default();
            rebuild_remote_repos(&wizard);
        },
    );
}

// MARK: - Actions

/// Host component of the daemon URL ("" when it can't be extracted).
fn auto_ssh_host(daemon_url: &str) -> String {
    let url = daemon_url.trim();
    let Some((_, rest)) = url.split_once("://") else { return String::new() };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if let Some(bracketed) = host_port.strip_prefix('[') {
        return bracketed.split(']').next().unwrap_or("").to_string();
    }
    host_port.split(':').next().unwrap_or("").to_string()
}

fn add_server(wizard: &Rc<Wizard>) {
    let clean_url = wizard.daemon_url.text().trim().to_string();
    let clean_token = wizard.token.text().trim().to_string();
    let clean_name = wizard.server_name.text().trim().to_string();

    let host_override = wizard.ssh_host_override.text().trim().to_string();
    let host = if host_override.is_empty() { auto_ssh_host(&clean_url) } else { host_override };
    let port = wizard.ssh_port.text().trim().parse::<i64>().unwrap_or(22);
    let user = match wizard.ssh_user.text().trim() {
        "" => "borg".to_string(),
        user => user.to_string(),
    };

    if host.is_empty() {
        fail(wizard, "Could not extract a host from the daemon URL. Set one under SSH (advanced).");
        return;
    }

    set_step(wizard, Step::ValidatingServer);
    let (url, token) = (clean_url.clone(), clean_token.clone());
    let wizard = wizard.clone();
    kit::bg(
        move || {
            borgbox::health(&url)?;
            borgbox::info(&url, &token).map(|_| ())
        },
        move |result| {
            if let Err(e) = result {
                fail(&wizard, &e);
                return;
            }
            let server = BorgBoxServer {
                id: Uuid::new_v4(),
                name: clean_name,
                daemon_url: clean_url,
                ssh_host: host,
                ssh_user: user,
                ssh_port: port,
            };
            // Back to idle before the store notifies, so the repo form comes
            // up with its controls enabled.
            wizard.step.set(Step::Idle);
            match wizard.app.borgbox.add(server, &clean_token) {
                Ok(()) => update_controls(&wizard),
                Err(e) => fail(&wizard, &e),
            }
        },
    );
}

fn run_repo_wizard(wizard: &Rc<Wizard>, server: BorgBoxServer) {
    let clean_name = wizard.repo_name.text().trim().to_string();
    let passphrase = wizard.passphrase.text().to_string();
    // The flow keeps the wizard state alive until it finishes, so a repo
    // registered on the daemon still ends up saved locally if the window is
    // closed halfway.
    let wizard = wizard.clone();

    // 1. Generate a fresh ed25519 key dedicated to this repo.
    set_step(&wizard, Step::GeneratingKey);
    let name = clean_name.clone();
    kit::bg(
        move || sshkeys::generate(&borgbox_import::key_slug(&name), &format!("luna-borgbox-{name}")),
        move |result| {
            let (private_key_path, public_key) = match result {
                Ok(pair) => pair,
                Err(e) => return fail(&wizard, &e),
            };

            // 2. Register the repo on the daemon (POST /repos with name + pubkey).
            set_step(&wizard, Step::RegisteringRepo);
            let server_bg = server.clone();
            kit::bg(
                move || borgbox::create_repo(&server_bg, &clean_name, &public_key),
                move |result| {
                    let created = match result {
                        Ok(created) => created,
                        Err(e) => return fail(&wizard, &e),
                    };

                    // 3. Compose the SSH URL for borg init using the daemon's
                    //    absolute path combined with our configured ssh
                    //    user/host/port.
                    let borg_url = format!(
                        "ssh://{}@{}:{}{}",
                        server.ssh_user, server.ssh_host, server.ssh_port, created.path
                    );

                    // 4. borg init over SSH with the dedicated key.
                    set_step(&wizard, Step::InitRepo);
                    let (url, pass, key) = (borg_url.clone(), passphrase.clone(), private_key_path.clone());
                    kit::bg(
                        move || borg::init_repo(&url, &pass, "repokey-blake2", Some(&key)),
                        move |result| {
                            if let Err(e) = result {
                                return fail(&wizard, &e);
                            }

                            // 5. Save locally with the key wired so every
                            //    borg call uses it.
                            set_step(&wizard, Step::SavingLocally);
                            let repo = Repository::new(&created.name, &borg_url, Some(private_key_path));
                            if let Err(e) = wizard.app.repos.add(repo, &passphrase) {
                                return fail(&wizard, &e);
                            }

                            set_step(&wizard, Step::Done);
                            kit::after(800, move || wizard.window.close());
                        },
                    );
                },
            );
        },
    );
}
