//! Search and historical-context windows share the asynchronous archive.
#[cfg(not(test))]
use super::archive::ArchiveService;
use super::archive::{ArchiveRecord, ArchiveReply, SearchCursor, SearchQuery};
use super::identity::{ConversationKey, MessageRef};
use super::{App, ChatMessage, Session};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct SearchUi {
    pub open: bool,
    query: SearchQuery,
    from: String,
    until: String,
    generation: u64,
    context_generation: u64,
    results: Vec<ArchiveRecord>,
    /// Where the next page starts; `None` once every match is shown.
    next: Option<SearchCursor>,
    /// The request in flight continues the visible results instead of replacing them.
    appending: bool,
    pending: bool,
    context_pending: bool,
    changed: Option<Instant>,
    error: Option<String>,
    context_error: Option<String>,
    indexing: bool,
    refresh_after_index: bool,
    context: Option<(MessageRef, Vec<ArchiveRecord>)>,
    context_radius: usize,
}

impl SearchUi {
    pub(crate) fn visible(&self) -> bool {
        self.open || self.context.is_some()
    }
    pub(crate) fn has_pending_work(&self) -> bool {
        self.pending || self.context_pending || self.changed.is_some()
    }
}

impl Session {
    pub(crate) fn archive_conversation_key(&self, target: &str) -> Option<ConversationKey> {
        Some(ConversationKey {
            network: self.logical_network_key()?,
            target: self.network_support.canonicalize(target),
        })
    }
    /// Scrollback rebuilt from the irssi text logs. Lines the archive knows to
    /// be redacted stay on disk (the logs are never rewritten) but are not shown.
    pub(crate) fn load_text_history(
        &self,
        network: &str,
        legacy_network: &str,
        channel_canonical: &str,
        channel_raw: &str,
        max_lines: usize,
        allow_legacy_migration: bool,
    ) -> Vec<ChatMessage> {
        let mut history = self.log_manager.load_history_casemapped(
            network,
            legacy_network,
            channel_canonical,
            channel_raw,
            max_lines,
            allow_legacy_migration,
        );
        if let Some(client) = &self.archive_client {
            // Rows are keyed by their physical line; unmatched rows are kept.
            history.retain(|message| !client.is_line_redacted(&message.message_key));
        }
        history
    }
    /// Drop rows already shown from a text log once the archive reports their
    /// line redacted. Such rows carry no server msgid, so REDACT cannot find them.
    pub(crate) fn drop_redacted_text_history(&mut self) {
        let Some(client) = &self.archive_client else {
            return;
        };
        let version = client.redacted_lines_version();
        if version == self.redacted_history_version {
            return;
        }
        self.redacted_history_version = version;
        for channel in self.conversations.values_mut() {
            channel
                .messages
                .retain(|message| !client.is_line_redacted(&message.message_key));
        }
    }
    pub(crate) fn record_archive_message(
        &self,
        target: &str,
        message: &ChatMessage,
        historical: bool,
    ) {
        self.record_archive_message_with_span(target, message, historical, None);
    }
    pub(crate) fn record_archive_message_with_span(
        &self,
        target: &str,
        message: &ChatMessage,
        historical: bool,
        span: Option<super::logging::LogSpan>,
    ) {
        if !self.logging_enabled
            || (message.no_log && !historical)
            || super::commands::service_message_contains_credentials(target, &message.content)
        {
            return;
        }
        if let (Some(client), Some(key)) =
            (&self.archive_client, self.archive_conversation_key(target))
        {
            let mut record = ArchiveRecord::from_message(key, message);
            record.text_span = span;
            client.upsert(record);
        }
    }
}

impl App {
    #[cfg(not(test))]
    pub(crate) fn install_archive(&mut self) {
        if !self.settings_writable {
            return;
        }
        if let Some(root) = dirs::config_dir().map(|path| path.join("linefeed")) {
            self.archive = ArchiveService::open(&root);
            for session in &mut self.sessions {
                session.archive_client = Some(self.archive.client.clone());
            }
        }
    }
    pub(crate) fn open_search(&mut self, index: usize, text: String, all_networks: bool) {
        self.archive.client.cancel_context();
        self.claim_global_dialogs(Some(super::windows::WindowKey::new(
            self.sessions[index].id,
            self.sessions[index].current_conversation,
        )));
        let session = &self.sessions[index];
        let query = SearchQuery {
            text,
            network: if all_networks {
                None
            } else {
                session.logical_network_key()
            },
            target: if all_networks {
                None
            } else {
                session
                    .current_target_name()
                    .map(|target| session.network_support.canonicalize(target))
            },
            ..Default::default()
        };
        self.search = SearchUi {
            open: true,
            query,
            indexing: self.archive.client.status().indexing,
            ..Default::default()
        };
        self.submit_search();
    }
    pub(crate) fn open_archive_context(&mut self, reference: MessageRef) {
        self.search.context = Some((reference.clone(), Vec::new()));
        self.search.context_radius = 50;
        self.search.context_generation = 0;
        self.search.context_error = None;
        self.search.context_pending = false;
        if !self.archive.client.is_enabled() {
            self.search.context_error =
                Some("The saved-history archive is unavailable for this profile".into());
            return;
        }
        self.search.context_generation = self.archive.client.context(reference, 50);
        self.search.context_pending = true;
    }
    fn close_history_search(&mut self) {
        self.search.open = false;
        self.search.generation = 0;
        self.search.pending = false;
        self.search.changed = None;
        self.archive.client.cancel_query();
    }
    fn close_archive_context(&mut self) {
        self.search.context = None;
        self.search.context_generation = 0;
        self.search.context_pending = false;
        self.search.context_error = None;
        self.archive.client.cancel_context();
    }
    /// "More results" continues after the last row shown; every other request
    /// (Enter, the Search button, an edited field, the debounce) starts over.
    fn plan_search_page(&mut self, next: bool) {
        self.search.query.after = if next { self.search.next } else { None };
    }
    fn submit_search(&mut self) {
        self.search.generation = 0;
        self.search.pending = false;
        self.search.changed = None;
        self.archive.client.cancel_query();
        self.search.error = None;
        if !self.archive.client.is_enabled() {
            self.search.pending = false;
            self.search.error =
                Some("The saved-history archive is unavailable for this profile".into());
            return;
        }
        self.search.query.from = match parse_date(&self.search.from, false) {
            Ok(date) => date,
            Err(error) => {
                self.search.error = Some(error);
                return;
            }
        };
        self.search.query.until = match parse_date(&self.search.until, true) {
            Ok(date) => date,
            Err(error) => {
                self.search.error = Some(error);
                return;
            }
        };
        if self
            .search
            .query
            .from
            .zip(self.search.query.until)
            .is_some_and(|(from, until)| from >= until)
        {
            self.search.error = Some("The start date must not be later than the end date".into());
            return;
        }
        self.search.appending = self.search.query.after.is_some();
        // Stored targets are casemapped keys, so the typed filter must be too.
        let mut query = self.search.query.clone();
        query.target_also.clear();
        let typed = query
            .target
            .as_deref()
            .map(str::trim)
            .filter(|target| !target.is_empty())
            .map(str::to_owned);
        let session = query.network.as_ref().and_then(|network| {
            self.sessions
                .iter()
                .find(|session| session.logical_network_key().as_ref() == Some(network))
        });
        query.target = match (typed, session) {
            (Some(target), Some(session)) => Some(session.network_support.canonicalize(&target)),
            (Some(target), None) => {
                // Without one known network the stored casemapping is unknown:
                // accept the key every supported mapping would have stored.
                let mut forms: Vec<String> = Vec::new();
                for mapping in [
                    super::types::CaseMapping::Rfc1459,
                    super::types::CaseMapping::Ascii,
                    super::types::CaseMapping::StrictRfc1459,
                    super::types::CaseMapping::Utf8Rfc8265,
                ] {
                    let support = super::NetworkSupport {
                        case_mapping: mapping,
                        ..Default::default()
                    };
                    let form = support.canonicalize(&target);
                    if !forms.contains(&form) {
                        forms.push(form);
                    }
                }
                let mut forms = forms.into_iter();
                let primary = forms.next();
                query.target_also = forms.collect();
                primary
            }
            (None, _) => None,
        };
        self.search.generation = self.archive.client.search(query);
        self.search.pending = true;
        self.search.changed = None;
    }
    pub(crate) fn tick_history_services(&mut self) {
        let indexing = self.archive.client.status().indexing;
        if self.search.indexing && !indexing {
            self.search.refresh_after_index = true;
        }
        self.search.indexing = indexing;
        for reply in self.archive.poll() {
            match reply {
                ArchiveReply::Search {
                    generation,
                    records,
                    next,
                } if self.search.open && generation == self.search.generation => {
                    if !self.search.appending {
                        self.search.results.clear();
                    }
                    // Keyset paging cannot repeat rows; the id check is a second
                    // line of defence for any future change to that guarantee.
                    for record in records {
                        if !self
                            .search
                            .results
                            .iter()
                            .any(|shown| shown.id == record.id)
                        {
                            self.search.results.push(record);
                        }
                    }
                    if self.search.results.len() > 500 {
                        self.search.results.drain(..self.search.results.len() - 500);
                    }
                    self.search.next = next;
                    self.search.pending = false;
                }
                ArchiveReply::Context {
                    generation,
                    reference,
                    records,
                } if self.search.context.is_some()
                    && generation == self.search.context_generation =>
                {
                    self.search.context = Some((reference, records));
                    self.search.context_pending = false;
                }
                ArchiveReply::Restored {
                    token,
                    reference,
                    records,
                } => self.handle_workspace_archive_reply(token, reference, records),
                ArchiveReply::RestoredError { token, message } => {
                    self.handle_workspace_archive_error(token, message)
                }
                ArchiveReply::Error {
                    generation,
                    message,
                } if self.search.open && generation == self.search.generation => {
                    self.search.error = Some(message);
                    self.search.pending = false;
                }
                ArchiveReply::ContextError {
                    generation,
                    message,
                } if self.search.context.is_some()
                    && generation == self.search.context_generation =>
                {
                    self.search.context_error = Some(message);
                    self.search.context_pending = false;
                }
                _ => {}
            }
        }
        for session in &mut self.sessions {
            session.drop_redacted_text_history();
        }
        // Replies already queued before REDACT and rows retained by an open window
        // must observe the same tombstones as new database queries.
        for record in &mut self.search.results {
            self.archive.client.apply_redactions(record);
        }
        self.search.results.retain(|record| !record.redacted);
        if let Some((_, records)) = &mut self.search.context {
            for record in records {
                self.archive.client.apply_redactions(record);
            }
        }
        // An initial query can finish before the background legacy import does.
        // Search and context use independent cancellation lanes, so refreshing
        // the complete search cannot disturb an open historical lookup.
        if self.search.refresh_after_index && self.search.open {
            self.search.refresh_after_index = false;
            self.search.query.after = None;
            self.submit_search();
        }
    }
    pub(crate) fn show_history_windows(&mut self, ctx: &egui::Context) {
        if self.search.open {
            let mut open = true;
            let mut changed = false;
            let mut search = false;
            let mut reveal = None;
            let mut next = false;
            egui::Window::new("History search")
                .open(&mut open)
                .default_size(egui::vec2(680.0, 500.0))
                .max_size(
                    (ctx.content_rect().size() - egui::vec2(16.0, 32.0))
                        .max(egui::vec2(100.0, 100.0)),
                )
                .show(ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Text:");
                        let field = ui.text_edit_singleline(&mut self.search.query.text);
                        changed |= field.changed();
                        search |=
                            field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if ui.button("Search").clicked() {
                            search = true;
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Sender:");
                        changed |= ui
                            .add(
                                egui::TextEdit::singleline(&mut self.search.query.sender)
                                    .align(egui::Align2::LEFT_CENTER)
                                    .desired_width(110.0),
                            )
                            .changed();
                        ui.label("From (UTC date):");
                        changed |= ui
                            .add(
                                egui::TextEdit::singleline(&mut self.search.from)
                                    .align(egui::Align2::LEFT_CENTER)
                                    .desired_width(95.0)
                                    .hint_text("YYYY-MM-DD"),
                            )
                            .changed();
                        ui.label("Through:");
                        changed |= ui
                            .add(
                                egui::TextEdit::singleline(&mut self.search.until)
                                    .align(egui::Align2::LEFT_CENTER)
                                    .desired_width(95.0)
                                    .hint_text("YYYY-MM-DD"),
                            )
                            .changed();
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Network contains:");
                        changed |= ui
                            .add(
                                egui::TextEdit::singleline(&mut self.search.query.network_pattern)
                                    .align(egui::Align2::LEFT_CENTER)
                                    .desired_width(150.0)
                                    .hint_text("host, account or network ID"),
                            )
                            .changed();
                        ui.label("Conversation:");
                        changed |= ui
                            .add(
                                egui::TextEdit::singleline(
                                    self.search.query.target.get_or_insert_with(String::new),
                                )
                                .align(egui::Align2::LEFT_CENTER)
                                .desired_width(120.0)
                                .hint_text("#channel or nick"),
                            )
                            .changed();
                    });
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("All networks / conversations").clicked() {
                            self.search.query.network = None;
                            self.search.query.target = None;
                            self.search.query.network_pattern.clear();
                            changed = true;
                        }
                        if let Some(network) = &self.search.query.network {
                            ui.label(network.storage_key());
                        }
                        changed |= ui
                            .checkbox(
                                &mut self.search.query.include_undated,
                                "Include unknown dates",
                            )
                            .changed();
                    });
                    let status = self.archive.client.status();
                    if status.indexing {
                        ui.label(
                            "Indexing saved logs… Results are incomplete until indexing finishes.",
                        );
                        ctx.request_repaint_after(Duration::from_millis(100));
                    }
                    if let Some(error) = &status.error {
                        ui.colored_label(super::theme::palette(ui.ctx()).mention, error);
                    }
                    if let Some(error) = &self.search.error {
                        ui.colored_label(super::theme::palette(ui.ctx()).danger, error);
                    }
                    if self.search.pending {
                        ui.spinner();
                        ctx.request_repaint_after(Duration::from_millis(50));
                    }
                    egui::ScrollArea::vertical()
                        .id_salt("archive_results")
                        .max_height((ui.available_height() - 36.0).max(40.0))
                        .show(ui, |ui| {
                            for record in &self.search.results {
                                let date = record
                                    .server_time
                                    .as_deref()
                                    .map(str::to_owned)
                                    .or_else(|| {
                                        record.received_at.map(|at| {
                                            super::helpers::format_timestamp(at.max(0) as u64)
                                        })
                                    })
                                    .unwrap_or_else(|| "date unknown".into());
                                let label = format!(
                                    "{} {} {} <{}> {}",
                                    date,
                                    record.conversation.network.storage_key(),
                                    record.conversation.target,
                                    record.sender,
                                    record.body
                                );
                                if ui
                                    .add(egui::Button::selectable(false, label).wrap())
                                    .clicked()
                                {
                                    reveal = Some(record.reference());
                                }
                            }
                        });
                    ui.horizontal(|ui| {
                        ui.label(format!("{} results shown", self.search.results.len()));
                        next = ui
                            .add_enabled(
                                self.search.next.is_some()
                                    && !self.search.pending
                                    && self.search.changed.is_none(),
                                egui::Button::new("More results"),
                            )
                            .clicked();
                    });
                });
            self.search.open = open;
            if !open {
                self.close_history_search();
            }
            if open && changed {
                self.search.changed = Some(Instant::now());
            }
            if self
                .search
                .changed
                .is_some_and(|at| at.elapsed() >= Duration::from_millis(250))
            {
                search = true;
            }
            if self.search.changed.is_some() {
                ctx.request_repaint_after(Duration::from_millis(250));
            }
            if open && (search || next) {
                self.plan_search_page(next);
                self.submit_search();
            }
            if let Some(reference) = reveal {
                self.open_archive_context(reference);
            }
        }
        if let Some((reference, records)) = &self.search.context {
            let mut open = true;
            let mut bigger = false;
            let mut reply_to = None;
            let mut reveal_parent = None;
            let reference = reference.clone();
            egui::Window::new("Historical context")
                .open(&mut open)
                .default_size(egui::vec2(700.0, 460.0))
                .max_size(
                    (ctx.content_rect().size() - egui::vec2(16.0, 32.0))
                        .max(egui::vec2(100.0, 100.0)),
                )
                .show(ctx, |ui| {
                    ui.label(format!(
                        "{} — {}",
                        reference.conversation.network.storage_key(),
                        reference.conversation.target
                    ));
                    if let Some(error) = &self.search.context_error {
                        ui.colored_label(super::theme::palette(ui.ctx()).danger, error);
                    }
                    if self.search.context_pending {
                        ui.spinner();
                        ctx.request_repaint_after(Duration::from_millis(50));
                    } else if records.is_empty() && self.search.context_error.is_none() {
                        ui.label("This message is not available in the saved archive.");
                    }
                    egui::ScrollArea::vertical()
                        .id_salt("archive_context")
                        .max_height((ui.available_height() - 38.0).max(50.0))
                        .show(ui, |ui| {
                            for record in records {
                                let selected = reference.archive_id.as_ref() == Some(&record.id)
                                    || reference
                                        .server_msgid
                                        .as_ref()
                                        .is_some_and(|id| record.msgid.as_ref() == Some(id));
                                let body = if record.redacted {
                                    "<message redacted>"
                                } else {
                                    &record.body
                                };
                                if let Some(parent_id) = &record.reply_to {
                                    let parent = records
                                        .iter()
                                        .find(|parent| parent.msgid.as_ref() == Some(parent_id));
                                    let preview = parent
                                        .map(|parent| {
                                            if parent.redacted {
                                                "<message redacted>".to_string()
                                            } else {
                                                format!(
                                                    "{}: {}",
                                                    parent.sender,
                                                    parent
                                                        .body
                                                        .chars()
                                                        .take(100)
                                                        .collect::<String>()
                                                )
                                            }
                                        })
                                        .unwrap_or_else(|| "Original message not loaded".into());
                                    if ui.small_button(format!("↪ {preview}")).clicked() {
                                        reveal_parent = Some(MessageRef {
                                            conversation: record.conversation.clone(),
                                            archive_id: parent.map(|p| p.id.clone()),
                                            server_msgid: Some(parent_id.clone()),
                                            server_time: None,
                                        });
                                    }
                                }
                                let text =
                                    format!("{} <{}> {}", record.display_time, record.sender, body);
                                ui.add(
                                    egui::Label::new(if selected {
                                        egui::RichText::new(text)
                                            .strong()
                                            .color(super::theme::palette(ui.ctx()).accent)
                                    } else {
                                        egui::RichText::new(text)
                                    })
                                    .wrap(),
                                );
                                if !record.reactions.is_empty() {
                                    ui.horizontal_wrapped(|ui| {
                                        for group in &record.reactions {
                                            ui.label(format!(
                                                "{} {}",
                                                group.emoji,
                                                group.actors.len()
                                            ))
                                            .on_hover_text(group.actors.join(", "));
                                        }
                                    });
                                }
                                if record.msgid.is_some()
                                    && !record.redacted
                                    && ui.small_button("Reply").clicked()
                                {
                                    reply_to = Some(record.reference());
                                }
                            }
                        });
                    bigger = ui
                        .add_enabled(
                            self.search.context_radius < 250 && !self.search.context_pending,
                            egui::Button::new("Show more context"),
                        )
                        .clicked();
                });
            if let Some(target) = reply_to
                && let Err(error) = self.begin_reply(target)
            {
                self.search.context_error = Some(error);
            }
            if !open {
                self.close_archive_context();
            } else if let Some(target) = reveal_parent {
                self.open_archive_context(target);
            } else if bigger {
                self.search.context_radius = (self.search.context_radius + 50).min(250);
                self.search.context_error = None;
                self.search.context_generation = self
                    .archive
                    .client
                    .context(reference, self.search.context_radius);
                self.search.context_pending = true;
            }
        }
    }
}

fn parse_date(input: &str, end: bool) -> Result<Option<i64>, String> {
    if input.trim().is_empty() {
        return Ok(None);
    }
    let input = input.trim();
    if input.len() != 10 || input.as_bytes()[4] != b'-' || input.as_bytes()[7] != b'-' {
        return Err("Dates must use YYYY-MM-DD".into());
    }
    let message = crate::irc::IrcMessage::parse(&format!(
        "@time={input}T00:00:00.000Z :archive NOTICE * :date"
    ));
    let time = message
        .and_then(|m| m.get_server_time())
        .ok_or_else(|| "Invalid calendar date".to_string())?;
    Ok(Some(time as i64 + if end { 86400 } else { 0 }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with_archive() -> (App, std::path::PathBuf) {
        let mut app = app();
        let root = std::env::temp_dir().join(format!(
            "linefeed-search-ui-{}-{}",
            std::process::id(),
            super::super::new_profile_id()
        ));
        app.archive = super::super::archive::ArchiveService::open(&root);
        assert!(app.archive.client.flush_barrier(Duration::from_secs(30)));
        (app, root)
    }

    fn finish_query(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.search.pending || app.search.context_pending {
            app.tick_history_services();
            assert!(Instant::now() < deadline, "Archive query did not complete");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn changing_search_does_not_cancel_the_open_historical_context() {
        let (mut app, root) = app_with_archive();
        let row = record();
        app.archive.client.upsert(row.clone());
        assert!(app.archive.client.flush_barrier(Duration::from_secs(30)));
        app.search.open = true;
        app.open_archive_context(row.reference());
        app.search.query.text = "old body".into();
        app.submit_search();
        finish_query(&mut app);
        assert_eq!(app.search.results.len(), 1);
        assert_eq!(
            app.search.context.as_ref().unwrap().1.len(),
            1,
            "The open context must finish independently of the search query"
        );
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opening_historical_context_does_not_cancel_the_pending_search() {
        let (mut app, root) = app_with_archive();
        let row = record();
        app.archive.client.upsert(row.clone());
        assert!(app.archive.client.flush_barrier(Duration::from_secs(30)));
        app.search.open = true;
        app.search.query.text = "old body".into();
        app.submit_search();
        app.open_archive_context(row.reference());
        finish_query(&mut app);
        assert_eq!(app.search.results.len(), 1);
        assert_eq!(app.search.context.as_ref().unwrap().1.len(), 1);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_late_context_reply_does_not_reopen_a_closed_context_window() {
        let (mut app, root) = app_with_archive();
        app.search.open = true;
        app.open_archive_context(record().reference());
        assert!(app.archive.client.flush_barrier(Duration::from_secs(30)));
        app.close_archive_context();
        app.tick_history_services();
        assert!(
            app.search.context.is_none(),
            "A closed context must stay closed"
        );
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn closing_search_keeps_the_pending_context_and_discards_late_search_results() {
        let (mut app, root) = app_with_archive();
        let row = record();
        app.archive.client.upsert(row.clone());
        assert!(app.archive.client.flush_barrier(Duration::from_secs(30)));
        app.search.open = true;
        app.submit_search();
        app.open_archive_context(row.reference());
        assert!(app.archive.client.flush_barrier(Duration::from_secs(30)));
        app.close_history_search();
        finish_query(&mut app);
        assert!(!app.search.open);
        assert!(app.search.results.is_empty());
        assert_eq!(app.search.context.as_ref().unwrap().1.len(), 1);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn app() -> App {
        App::with_settings(
            super::super::Settings {
                logging_enabled: false,
                logging_load_history: false,
                ..Default::default()
            },
            super::super::windows::LayoutFile::default(),
        )
    }

    fn record() -> ArchiveRecord {
        let conversation = ConversationKey {
            network: super::super::identity::LogicalNetworkKey::Direct {
                endpoint: super::super::identity::TransportEndpointKey::new(
                    "irc.example",
                    6697,
                    true,
                ),
            },
            target: "#test".into(),
        };
        let mut record = ArchiveRecord::from_message(
            conversation,
            &ChatMessage::new_fmt("alice", "old body", "short"),
        );
        record.msgid = Some("parent".into());
        record
    }

    #[test]
    fn cached_search_and_context_observe_live_redaction() {
        let mut app = app();
        let record = record();
        app.search.results.push(record.clone());
        app.search.context = Some((record.reference(), vec![record.clone()]));
        app.archive
            .client
            .redact(record.conversation, record.msgid.unwrap());
        app.tick_history_services();
        assert!(app.search.results.is_empty());
        let context = &app.search.context.as_ref().unwrap().1;
        assert!(context[0].redacted);
        assert!(context[0].body.is_empty());
    }

    #[test]
    fn unavailable_archive_context_finishes_with_an_error() {
        let mut app = app();
        app.open_archive_context(record().reference());
        assert!(!app.search.context_pending);
        assert!(app.search.context_error.is_some());
    }

    #[test]
    fn completing_log_import_refreshes_the_query_without_closing_context() {
        let mut app = app();
        app.search.open = true;
        app.search.indexing = true;
        app.search.query.after = Some((1, 1));
        app.search.context = Some((record().reference(), vec![record()]));
        app.tick_history_services();
        assert!(!app.search.refresh_after_index);
        assert_eq!(app.search.query.after, None);
        assert!(app.search.context.is_some());
    }

    #[test]
    fn a_fresh_search_after_more_results_replaces_instead_of_repeating_a_page() {
        let (mut app, root) = app_with_archive();
        for index in 0..150 {
            let mut row = record();
            row.id = format!("row{index}");
            row.msgid = Some(format!("m{index}"));
            row.received_at = Some(1_000 + index);
            app.archive.client.upsert(row);
        }
        assert!(app.archive.client.flush_barrier(Duration::from_secs(30)));
        app.search.open = true;
        app.search.query.text = "old body".into();
        app.plan_search_page(false);
        app.submit_search();
        finish_query(&mut app);
        assert_eq!(app.search.results.len(), 100);
        assert!(app.search.next.is_some());
        app.plan_search_page(true);
        app.submit_search();
        finish_query(&mut app);
        assert_eq!(app.search.results.len(), 150);
        assert!(app.search.next.is_none());
        // Enter or the Search button without paging starts again from the newest rows.
        app.plan_search_page(false);
        assert_eq!(app.search.query.after, None);
        app.submit_search();
        finish_query(&mut app);
        let mut ids: Vec<_> = app.search.results.iter().map(|r| r.id.clone()).collect();
        assert_eq!(ids.len(), 100);
        assert_eq!(ids[0], "row149");
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 100);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn typed_conversation_filter_is_trimmed_and_casemapped_like_the_stored_key() {
        let (mut app, root) = app_with_archive();
        let row = record();
        app.archive.client.upsert(row.clone());
        assert!(app.archive.client.flush_barrier(Duration::from_secs(30)));
        app.search.open = true;
        for typed in ["#TEST", " #test ", "#Test"] {
            app.search.query.target = Some(typed.into());
            app.submit_search();
            finish_query(&mut app);
            assert_eq!(app.search.results.len(), 1, "{typed:?} found nothing");
        }
        app.search.query.target = Some("#other".into());
        app.submit_search();
        finish_query(&mut app);
        assert!(app.search.results.is_empty());
        // A blank box means no conversation filter, not a literal blank target.
        app.search.query.target = Some("   ".into());
        app.submit_search();
        finish_query(&mut app);
        assert_eq!(app.search.results.len(), 1);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn redaction_session(app: &mut App, root: &std::path::Path) -> (String, ConversationKey) {
        let client = app.archive.client.clone();
        app.start(None);
        let session = &mut app.sessions[0];
        session.server_host = "workspace.example".into();
        session.nickname = "me".into();
        assert!(session.start_session_from_form());
        session.archive_client = Some(client);
        session.log_manager = super::super::logging::LogManager::with_test_dir(root.join("logs"));
        let network = session.logical_network_key().unwrap();
        let key = session.archive_conversation_key("alice").unwrap();
        (network.storage_key(), key)
    }

    fn log_and_archive(app: &App, network: &str, key: &ConversationKey, body: &str, msgid: &str) {
        let session = &app.sessions[0];
        let mut message = ChatMessage::new_fmt("alice", body, "short");
        message.msgid = Some(msgid.into());
        let span = session.log_manager.log_message(network, "alice", &message);
        session.log_manager.flush_all();
        let mut row = ArchiveRecord::from_message(key.clone(), &message);
        row.text_span = span;
        assert!(app.archive.client.upsert(row));
    }

    fn reload(app: &App, network: &str) -> Vec<String> {
        app.sessions[0]
            .load_text_history(network, "", "alice", "alice", 100, false)
            .into_iter()
            .map(|message| message.content)
            .collect()
    }

    #[test]
    fn redacted_messages_do_not_return_from_text_log_history_even_after_restart() {
        let (mut app, root) = app_with_archive();
        let (network, key) = redaction_session(&mut app, &root);
        log_and_archive(&app, &network, &key, "secret body", "m1");
        log_and_archive(&app, &network, &key, "kept body", "m2");
        assert!(app.archive.client.flush_barrier(Duration::from_secs(30)));
        assert_eq!(reload(&app, &network), ["secret body", "kept body"]);

        app.archive.client.redact(key.clone(), "m1".into());
        assert!(app.archive.client.flush_barrier(Duration::from_secs(30)));
        assert_eq!(reload(&app, &network), ["kept body"]);
        // The text log itself is deliberately not rewritten.
        let log = app.sessions[0].log_manager.test_log_path(&network, "alice");
        assert!(
            std::fs::read_to_string(log)
                .unwrap()
                .contains("secret body")
        );

        // A new process learns the redaction from the journal alone.
        app.archive = super::super::archive::ArchiveService::open(&root);
        assert!(app.archive.client.flush_barrier(Duration::from_secs(30)));
        app.sessions[0].archive_client = Some(app.archive.client.clone());
        assert_eq!(reload(&app, &network), ["kept body"]);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_redaction_removes_rows_already_loaded_from_the_text_log() {
        let (mut app, root) = app_with_archive();
        let (network, key) = redaction_session(&mut app, &root);
        log_and_archive(&app, &network, &key, "secret body", "m1");
        log_and_archive(&app, &network, &key, "kept body", "m2");
        assert!(app.archive.client.flush_barrier(Duration::from_secs(30)));
        let loaded = app.sessions[0].load_text_history(&network, "", "alice", "alice", 100, false);
        let id = app.sessions[0].ensure_target("alice");
        app.sessions[0]
            .conversation_channel_mut(id)
            .unwrap()
            .messages = loaded.into();
        // The reloaded rows carry no server msgid, so REDACT cannot find them
        // in the tab; the archive's knowledge must reach the open scrollback.
        app.archive.client.redact(key, "m1".into());
        assert!(app.archive.client.flush_barrier(Duration::from_secs(30)));
        app.tick_history_services();
        let shown: Vec<_> = app.sessions[0]
            .conversation_channel(id)
            .unwrap()
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(shown, ["kept body"]);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn conversation_filter_without_a_network_matches_every_casemapping() {
        let (mut app, root) = app_with_archive();
        // The same typed name is stored folded differently by networks that
        // advertise ascii, rfc1459, strict-rfc1459 or rfc8265 casemapping.
        for (index, target) in ["#a[b]\\^~", "#a{b}|~~", "#a{b}|^~"]
            .into_iter()
            .enumerate()
        {
            let mut row = record();
            row.id = format!("row{index}");
            row.msgid = Some(format!("m{index}"));
            row.conversation.target = target.into();
            app.archive.client.upsert(row);
        }
        let mut other = record();
        other.id = "other".into();
        other.msgid = Some("other".into());
        other.conversation.target = "#a[c]".into();
        app.archive.client.upsert(other);
        assert!(app.archive.client.flush_barrier(Duration::from_secs(30)));
        app.search.open = true;
        app.search.query.network = None;
        app.search.query.target = Some(" #A[B]\\^~ ".into());
        app.submit_search();
        finish_query(&mut app);
        let mut ids: Vec<_> = app.search.results.iter().map(|r| r.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["row0", "row1", "row2"]);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dates_validate_calendar_and_include_entire_end_day() {
        assert!(parse_date("2026-02-30", false).is_err());
        assert_eq!(
            parse_date("2024-02-29", true).unwrap().unwrap()
                - parse_date("2024-02-29", false).unwrap().unwrap(),
            86400
        );
        assert!(parse_date("", false).unwrap().is_none());
    }
}
