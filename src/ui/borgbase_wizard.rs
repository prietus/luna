//! "New on BorgBase" wizard (BorgMac's `BorgBaseQuickSetupSheet`): generates a
//! dedicated SSH key, creates the repository on BorgBase and runs `borg init`
//! in one step. A stand-alone singleton window, not a sheet.
use super::borg_wizard::{ProgressRow, bind_escape, sheet_header, show_full_selection, wizard_window, wrapping_caption};
use super::kit::{self, icons};
use super::state::AppState;
use crate::core::models::Repository;
use crate::core::{borg, borgbase, sshkeys};
use adw::prelude::*;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const REGIONS: [(&str, &str); 2] = [("eu", "Europe (Frankfurt / Helsinki)"), ("us", "United States")];

#[derive(Clone, Copy, PartialEq)]
enum Step {
    Idle,
    GeneratingKey,
    UploadingKey,
    CreatingRepo,
    InitRepo,
    SavingLocally,
    Done,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Step::Idle => "",
            Step::GeneratingKey => "Generating a dedicated SSH key…",
            Step::UploadingKey => "Uploading SSH key to BorgBase…",
            Step::CreatingRepo => "Creating the repository on BorgBase…",
            Step::InitRepo => "Initializing the Borg repository (borg init)…",
            Step::SavingLocally => "Saving to Luna…",
            Step::Done => "Done.",
        }
    }

    fn is_running(self) -> bool {
        !matches!(self, Step::Idle | Step::Done)
    }
}

/// What the worker thread reports back to the window.
enum Event {
    Step(Step),
    Created { name: String, url: String, key_path: String },
    Failed(String),
}

struct Wizard {
    app: Rc<AppState>,
    window: adw::Window,
    form: gtk::Box,
    token_entry: gtk::Box,
    token: gtk::PasswordEntry,
    connect: gtk::Button,
    name: adw::EntryRow,
    passphrase: adw::PasswordEntryRow,
    confirm: adw::PasswordEntryRow,
    region: adw::ComboRow,
    progress: ProgressRow,
    forget: gtk::Button,
    cancel: gtk::Button,
    create: gtk::Button,
    has_token: Cell<bool>,
    step: Cell<Step>,
}

impl Wizard {
    fn update(&self) {
        let has_token = self.has_token.get();
        let step = self.step.get();
        let running = step.is_running();

        self.form.set_visible(has_token);
        self.token_entry.set_visible(!has_token);
        self.connect.set_sensitive(!self.token.text().trim().is_empty());

        self.progress.set(match step {
            Step::Idle => None,
            other => Some((other.label(), other.is_running())),
        });

        self.forget.set_visible(has_token);
        self.forget.set_sensitive(!running);
        self.cancel.set_sensitive(!running);
        self.create.set_visible(has_token);
        let pass = self.passphrase.text();
        let can_create =
            step == Step::Idle && !self.name.text().trim().is_empty() && !pass.is_empty() && pass == self.confirm.text();
        self.create.set_sensitive(can_create);
        self.window.set_default_widget(Some(if has_token { &self.create } else { &self.connect }));
    }

    fn fail(&self, message: &str) {
        self.step.set(Step::Idle);
        self.update();
        kit::alert(&self.window, "Error", message);
    }

    fn save_token(self: &Rc<Self>) {
        let trimmed = self.token.text().trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        self.connect.set_sensitive(false);
        let weak = Rc::downgrade(self);
        kit::bg(
            move || borgbase::save_token(&trimmed),
            move |result| {
                let Some(w) = weak.upgrade() else { return };
                match result {
                    Ok(()) => {
                        w.has_token.set(true);
                        w.update();
                    }
                    Err(e) => w.fail(&e),
                }
            },
        );
    }

    fn forget_token(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        kit::bg(borgbase::forget_token, move |()| {
            if let Some(w) = weak.upgrade() {
                w.has_token.set(false);
                w.update();
            }
        });
    }

    fn run(self: &Rc<Self>) {
        let clean_name = self.name.text().trim().to_string();
        if clean_name.is_empty() {
            return;
        }
        let passphrase = self.passphrase.text().to_string();
        let region = REGIONS.get(self.region.selected() as usize).map(|r| r.0).unwrap_or("eu");

        self.step.set(Step::GeneratingKey);
        self.update();

        let (tx, rx) = async_channel::unbounded::<Event>();
        let pass = passphrase.clone();
        std::thread::spawn(move || {
            let step = |s: Step| {
                let _ = tx.send_blocking(Event::Step(s));
            };
            let result = (|| -> Result<Event, String> {
                // 1. Generate SSH key
                let (private_key_path, public_key) =
                    sshkeys::generate(&clean_name, &format!("luna-{clean_name}"))?;
                // 2. Upload to BorgBase
                step(Step::UploadingKey);
                let added = borgbase::add_ssh_key(&format!("Luna {clean_name}"), &public_key)?;
                // 3. Create repo on BorgBase
                step(Step::CreatingRepo);
                let created = borgbase::create_repo(&clean_name, &[added.id], false, region)?;
                // 4. borg init
                step(Step::InitRepo);
                borg::init_repo(&created.repo_path, &pass, "repokey-blake2", Some(&private_key_path))?;
                Ok(Event::Created { name: created.name, url: created.repo_path, key_path: private_key_path })
            })();
            let _ = tx.send_blocking(result.unwrap_or_else(Event::Failed));
        });

        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(event) = rx.recv().await {
                let Some(w) = weak.upgrade() else { break };
                match event {
                    Event::Step(step) => {
                        w.step.set(step);
                        w.update();
                    }
                    Event::Created { name, url, key_path } => w.finish(&name, &url, key_path, &passphrase),
                    Event::Failed(e) if borgbase::is_unauthenticated(&e) => {
                        // The stored token is stale — wipe it and bounce the user back
                        // to the token entry screen so they can paste a fresh one.
                        w.token.set_text("");
                        w.has_token.set(false);
                        w.forget_token();
                        w.fail("The BorgBase token is no longer valid. Paste a new one.");
                    }
                    Event::Failed(e) => w.fail(&e),
                }
            }
        });
    }

    /// 5. Save locally
    fn finish(self: &Rc<Self>, name: &str, url: &str, key_path: String, passphrase: &str) {
        self.step.set(Step::SavingLocally);
        self.update();
        let repo = Repository::new(name, url, Some(key_path));
        if let Err(e) = self.app.repos.add(repo, passphrase) {
            self.fail(&e);
            return;
        }
        self.step.set(Step::Done);
        self.update();
        // Auto-dismiss after a short pause
        let window = self.window.downgrade();
        kit::after(800, move || {
            if let Some(window) = window.upgrade() {
                window.close();
            }
        });
    }
}

/// Opens (or focuses) the wizard window.
pub fn open(app: &Rc<AppState>) {
    let Some((window, content)) = wizard_window(app, "wizard", "New on BorgBase", 560, 540) else { return };

    content.append(&sheet_header(
        icons::WAND,
        "New repository on BorgBase",
        "Generate an SSH key, create the repo on BorgBase, and run borg init in one step.",
    ));
    content.append(&kit::divider());

    // ---------------------------------------------------------- token entry
    let token_entry = kit::vbox(12);
    let token_title = kit::hbox(8);
    token_title.append(&kit::icon_styled(icons::KEY, &["tint"]));
    token_title.append(&kit::label("Connect your BorgBase account", &["heading"]));
    token_entry.append(&token_title);
    token_entry.append(&wrapping_caption(kit::caption_markup(
        "To create repos automatically I need an API token with write scope. Generate one at borgbase.com → Settings → Access Tokens, check the <b>Repository Add</b> and <b>SSH Key Add</b> scopes, and paste it here. It is stored in your local keyring.",
    )));
    let token = gtk::PasswordEntry::builder().placeholder_text("Paste your API token here").activates_default(true).build();
    token_entry.append(&token);
    let token_buttons = kit::hbox(8);
    let link = gtk::Button::with_label("Open BorgBase");
    link.add_css_class("link-like");
    link.add_css_class("caption");
    link.connect_clicked(|_| kit::open_uri("https://www.borgbase.com/settings/access_tokens"));
    let connect = kit::primary_button("Connect");
    token_buttons.append(&link);
    token_buttons.append(&kit::spacer());
    token_buttons.append(&connect);
    token_entry.append(&token_buttons);
    token_entry.set_visible(false);
    content.append(&token_entry);

    // ----------------------------------------------------------------- form
    let form = kit::vbox(14);
    form.add_css_class("bb-compact");
    let repo_group = kit::form_group();
    repo_group.set_title("Repository");
    let name = adw::EntryRow::builder().title("Name (e.g. laptop-personal)").activates_default(true).build();
    let passphrase = adw::PasswordEntryRow::builder().title("Passphrase").activates_default(true).build();
    let confirm = adw::PasswordEntryRow::builder().title("Repeat passphrase").activates_default(true).build();
    repo_group.add(&name);
    repo_group.add(&passphrase);
    repo_group.add(&confirm);
    form.append(&repo_group);
    let region_group = kit::form_group();
    region_group.set_title("Region");
    let region_labels: Vec<&str> = REGIONS.iter().map(|r| r.1).collect();
    let region = kit::combo_row("Data center", &region_labels, 0);
    show_full_selection(&region);
    region_group.add(&region);
    form.append(&region_group);
    form.set_visible(false);
    content.append(&form);

    let progress = ProgressRow::new();
    content.append(&progress.widget);

    // --------------------------------------------------------------- footer
    let filler = kit::vbox(0);
    filler.set_vexpand(true);
    content.append(&filler);
    let forget = gtk::Button::with_label("Forget BorgBase token");
    forget.add_css_class("flat");
    forget.add_css_class("mac");
    forget.set_tooltip_text(Some(
        "Remove the API token from this computer. You'll need to paste it again to create more repos.",
    ));
    let cancel = kit::button("Cancel");
    let create = kit::primary_button("Create");
    content.append(&kit::button_row(Some(forget.upcast_ref()), &[&cancel, &create]));
    bind_escape(&window, &cancel);

    let w = Rc::new(Wizard {
        app: app.clone(),
        window: window.clone(),
        form,
        token_entry,
        token,
        connect,
        name,
        passphrase,
        confirm,
        region,
        progress,
        forget,
        cancel,
        create,
        has_token: Cell::new(false),
        step: Cell::new(Step::Idle),
    });

    w.token.connect_changed(glib::clone!(
        #[weak]
        w,
        move |_| w.update()
    ));
    w.name.connect_changed(glib::clone!(
        #[weak]
        w,
        move |_| w.update()
    ));
    for row in [&w.passphrase, &w.confirm] {
        row.connect_changed(glib::clone!(
            #[weak]
            w,
            move |_| w.update()
        ));
    }
    w.connect.connect_clicked(glib::clone!(
        #[weak]
        w,
        move |_| w.save_token()
    ));
    w.forget.connect_clicked(glib::clone!(
        #[weak]
        w,
        move |_| w.forget_token()
    ));
    w.cancel.connect_clicked(glib::clone!(
        #[weak]
        window,
        move |_| window.close()
    ));
    w.create.connect_clicked(glib::clone!(
        #[weak]
        w,
        move |_| w.run()
    ));
    // Closing mid-run would leave a half-created repository behind.
    window.connect_close_request(glib::clone!(
        #[weak]
        w,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_| if w.step.get().is_running() { glib::Propagation::Stop } else { glib::Propagation::Proceed }
    ));

    w.update();
    // The keyring lookup may block on an unlock prompt: keep it off the main thread.
    kit::bg(
        borgbase::has_token,
        glib::clone!(
            #[weak]
            w,
            move |has_token| {
                w.has_token.set(has_token);
                w.update();
            }
        ),
    );

    // The window owns the wizard state; handlers only hold weak references.
    let keep = RefCell::new(Some(w));
    window.connect_destroy(move |_| {
        keep.borrow_mut().take();
    });
    window.present();
}
