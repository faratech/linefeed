//! Bounded command templates. Expansion is pure and precedes every privacy gate.

use super::actions::{ActionId, KeyChord, ShortcutBinding};
use super::{ChatMessage, ConversationId, Session};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

const MAX_ALIASES: usize = 256;
const MAX_STEPS: usize = 32;
const MAX_BYTES: usize = 64 * 1024;
const MAX_DEPTH: usize = 8;
const MAX_JOBS: usize = 64;
const MACRO_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct AliasDef {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub scope: Option<String>,
    pub steps: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct AutomationConfig {
    #[serde(skip)]
    unavailable: bool,
    pub version: u32,
    pub aliases: Vec<AliasDef>,
    pub shortcuts: Vec<ShortcutBinding>,
}
impl Default for AutomationConfig {
    fn default() -> Self {
        Self {
            unavailable: false,
            version: 1,
            aliases: Vec::new(),
            shortcuts: super::actions::default_bindings(),
        }
    }
}

pub(crate) struct CommandPlan {
    pub steps: Vec<String>,
    pub sensitive: bool,
    pub aliased: bool,
}
struct ExpansionContext<'a> {
    scope: &'a str,
    target: Option<&'a str>,
    nick: &'a str,
}

fn split_word(input: &str) -> (&str, &str) {
    input
        .trim_start()
        .split_once(char::is_whitespace)
        .map_or((input.trim_start(), ""), |(word, rest)| {
            (word, rest.trim_start())
        })
}
fn command_word(line: &str) -> (&str, &str) {
    split_word(line.trim_start().trim_start_matches('/'))
}

impl AutomationConfig {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("Unsupported automation configuration version".into());
        }
        if self.aliases.len() > MAX_ALIASES || self.shortcuts.len() > 256 {
            return Err("At most 256 aliases and 256 shortcuts are allowed".into());
        }
        if self
            .aliases
            .iter()
            .flat_map(|alias| &alias.steps)
            .map(String::len)
            .sum::<usize>()
            > 1024 * 1024
        {
            return Err("Alias definitions together must fit within 1 MiB".into());
        }
        for (index, alias) in self.aliases.iter().enumerate() {
            if alias.id.is_empty()
                || alias.id.len() > 128
                || alias.name.is_empty()
                || alias.name.len() > 32
                || !alias.name.as_bytes()[0].is_ascii_alphabetic()
                || !alias
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err("Alias names must start with a letter and contain at most 32 ASCII letters, digits, _ or -".into());
            }
            if reserved(&alias.name) {
                return Err(format!("Built-in command {} is reserved", alias.name));
            }
            if self.aliases[..index].iter().any(|other| {
                other.id == alias.id
                    || (other.scope == alias.scope && other.name.eq_ignore_ascii_case(&alias.name))
            }) {
                return Err("Alias IDs and names within a scope must be unique".into());
            }
            if alias.steps.is_empty()
                || alias.steps.len() > 16
                || alias.steps.iter().map(String::len).sum::<usize>() > MAX_BYTES
            {
                return Err("An alias needs 1–16 steps, totaling at most 64 KiB".into());
            }
            for step in &alias.steps {
                if step.contains(['\r', '\n', '\0']) {
                    return Err("Put each macro command on a separate editor line".into());
                }
                let (word, _) = command_word(step);
                if word.is_empty() || !word.is_ascii() || word.contains('$') {
                    return Err("Each step must start with a literal command name".into());
                }
                validate_template(step)?;
            }
            self.check_cycle(
                alias,
                &mut Vec::new(),
                &mut std::collections::HashSet::new(),
            )?;
        }
        for (index, binding) in self.shortcuts.iter().enumerate() {
            if !binding.chord.valid() {
                return Err(
                    "Use a modifier or function key; Enter and Escape remain available to editors"
                        .into(),
                );
            }
            if self.shortcuts[..index]
                .iter()
                .any(|other| other.chord == binding.chord)
            {
                return Err(format!("Duplicate shortcut {}", binding.chord.label()));
            }
            if let ActionId::RunAlias { id } = &binding.action
                && !self.aliases.iter().any(|alias| &alias.id == id)
            {
                return Err("A shortcut references an alias that no longer exists".into());
            }
            if let ActionId::SelectOrdinal { index } = binding.action
                && index >= 1000
            {
                return Err("Window ordinal must be below 1000".into());
            }
        }
        Ok(())
    }
    fn check_cycle(
        &self,
        alias: &AliasDef,
        stack: &mut Vec<String>,
        visited: &mut std::collections::HashSet<String>,
    ) -> Result<(), String> {
        if stack.len() >= MAX_DEPTH || stack.contains(&alias.id) {
            return Err(
                "Alias recursion or nesting deeper than eight commands is not allowed".into(),
            );
        }
        if visited.contains(&alias.id) {
            return Ok(());
        }
        stack.push(alias.id.clone());
        for step in &alias.steps {
            let (word, _) = command_word(step);
            for child in self.aliases.iter().filter(|child| {
                child.name.eq_ignore_ascii_case(word)
                    && (child.scope.is_none()
                        || child.scope == alias.scope
                        || alias.scope.is_none())
            }) {
                self.check_cycle(child, stack, visited)?;
            }
        }
        stack.pop();
        visited.insert(alias.id.clone());
        Ok(())
    }
    fn lookup(&self, name: &str, scope: &str) -> Option<&AliasDef> {
        self.aliases
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(name) && a.scope.as_deref() == Some(scope))
            .or_else(|| {
                self.aliases
                    .iter()
                    .find(|a| a.name.eq_ignore_ascii_case(name) && a.scope.is_none())
            })
    }
    fn expand(&self, input: &str, context: &ExpansionContext<'_>) -> Result<CommandPlan, String> {
        if self.unavailable && !reserved(command_word(input).0) {
            return Err("Automation configuration is unavailable; repair it in Settings before running custom commands".into());
        }
        let mut plan = CommandPlan {
            steps: Vec::new(),
            sensitive: false,
            aliased: false,
        };
        self.expand_step(input, context, &mut Vec::new(), &mut plan)?;
        Ok(plan)
    }
    fn expand_id(&self, id: &str, context: &ExpansionContext<'_>) -> Result<CommandPlan, String> {
        if self.unavailable {
            return Err("Automation configuration is unavailable; repair it in Settings before running custom commands".into());
        }
        let alias = self
            .aliases
            .iter()
            .find(|alias| {
                alias.id == id
                    && (alias.scope.is_none() || alias.scope.as_deref() == Some(context.scope))
            })
            .ok_or("The selected alias is unavailable on this network")?;
        let mut plan = CommandPlan {
            steps: Vec::new(),
            sensitive: false,
            aliased: true,
        };
        self.expand_alias(alias, "", context, &mut Vec::new(), &mut plan)?;
        Ok(plan)
    }
    fn expand_alias(
        &self,
        alias: &AliasDef,
        args: &str,
        context: &ExpansionContext<'_>,
        stack: &mut Vec<String>,
        plan: &mut CommandPlan,
    ) -> Result<(), String> {
        if stack.len() >= MAX_DEPTH || stack.contains(&alias.id) {
            return Err("Alias recursion limit reached".into());
        }
        plan.aliased = true;
        stack.push(alias.id.clone());
        for template in &alias.steps {
            let expanded = render(template, args, context)?;
            self.expand_step(&expanded, context, stack, plan)?;
        }
        stack.pop();
        Ok(())
    }
    fn expand_step(
        &self,
        line: &str,
        context: &ExpansionContext<'_>,
        stack: &mut Vec<String>,
        plan: &mut CommandPlan,
    ) -> Result<(), String> {
        let (name, args) = command_word(line);
        if !line.starts_with("//")
            && let Some(alias) = self.lookup(name, context.scope)
        {
            self.expand_alias(alias, args, context, stack, plan)?;
        } else {
            if plan.steps.len() >= MAX_STEPS
                || line.len() + plan.steps.iter().map(String::len).sum::<usize>() > MAX_BYTES
            {
                return Err("Expanded command plan exceeds 32 commands or 64 KiB".into());
            }
            let step = if line.starts_with('/') {
                line.to_string()
            } else {
                format!("/{line}")
            };
            if plan.aliased {
                validate_expanded_step(&step)?;
            }
            plan.sensitive |= sensitive_line(&step, context.target);
            plan.steps.push(step);
        }
        Ok(())
    }
}
fn sensitive_line(line: &str, target: Option<&str>) -> bool {
    if super::commands::command_line_contains_credentials(line) {
        return true;
    }
    let (command, args) = command_word(line);
    target.is_some_and(|target| {
        matches!(command.to_ascii_uppercase().as_str(), "SAY" | "ME")
            && super::commands::service_message_contains_credentials(target, args)
    })
}

fn validate_expanded_step(line: &str) -> Result<(), String> {
    let (command, args) = command_word(line);
    let required = match command.to_ascii_uppercase().as_str() {
        "MSG" | "PRIVMSG" | "NOTICE" | "N" | "DESCRIBE" | "REPLY" | "REACT" | "AUTH" | "LOGIN" => 2,
        "JOIN" | "J" | "NICK" | "RAW" | "QUOTE" | "NS" | "NICKSERV" | "AS" | "AUTHSERV"
        | "IDENTIFY" | "ID" | "SIDENTIFY" => 1,
        _ => 0,
    };
    if args.split_whitespace().count() < required {
        return Err(format!(
            "Alias step {command} is missing required arguments"
        ));
    }
    if matches!(command.to_ascii_uppercase().as_str(), "RAW" | "QUOTE")
        && args.contains(['\r', '\n'])
    {
        return Err("Raw macro commands cannot contain line breaks".into());
    }
    Ok(())
}
fn validate_template(template: &str) -> Result<(), String> {
    // A placeholder parse with all arguments supplied checks syntax without
    // logging or retaining a rendered example in the configuration.
    render(
        template,
        "one two three four five six seven eight nine",
        &ExpansionContext {
            scope: "",
            target: Some("#target"),
            nick: "nickname",
        },
    )
    .map(|_| ())
}
fn render(template: &str, args: &str, context: &ExpansionContext<'_>) -> Result<String, String> {
    let mut positions = Vec::new();
    let mut start = None;
    for (index, character) in args.char_indices() {
        if character.is_whitespace() {
            if let Some(begin) = start.take() {
                positions.push((begin, index));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(begin) = start {
        positions.push((begin, args.len()));
    }
    let mut result = String::new();
    let mut chars = template.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch != '$' {
            result.push(ch);
            continue;
        }
        let Some((_, next)) = chars.next() else {
            return Err("Use $$ for a literal dollar sign".into());
        };
        match next {
            '$' => result.push('$'),
            '*' => result.push_str(args),
            '1'..='9' => {
                let index = next as usize - '1' as usize;
                let &(begin, end) = positions
                    .get(index)
                    .ok_or_else(|| format!("Missing required argument ${}", index + 1))?;
                if chars.peek().is_some_and(|(_, c)| *c == '-') {
                    chars.next();
                    result.push_str(&args[begin..]);
                } else {
                    result.push_str(&args[begin..end]);
                }
            }
            't' | 'n' => {
                let mut name = String::from(next);
                while chars
                    .peek()
                    .is_some_and(|(_, c)| c.is_ascii_alphabetic() || *c == '_')
                {
                    name.push(chars.next().unwrap().1);
                }
                match name.as_str() {
                    "target" => {
                        result.push_str(context.target.ok_or("This alias requires a conversation")?)
                    }
                    "nick" => result.push_str(context.nick),
                    _ => {
                        return Err(
                            "Supported placeholders: $1..$9, $1-, $*, $target, $nick, $$".into(),
                        );
                    }
                }
            }
            _ => return Err("Supported placeholders: $1..$9, $1-, $*, $target, $nick, $$".into()),
        }
        if result.len() > MAX_BYTES {
            return Err("Expanded alias exceeds 64 KiB".into());
        }
    }
    if result.contains('\0') {
        return Err("NUL is not allowed in commands".into());
    }
    Ok(result)
}

pub(crate) struct MacroJob {
    generation: Option<u64>,
    conversation: Option<ConversationId>,
    target: Option<String>,
    steps: VecDeque<String>,
}

impl Session {
    pub(crate) fn run_alias_id(&mut self, id: &str) {
        let scope = self.automation_scope();
        match self.automation.expand_id(
            id,
            &ExpansionContext {
                scope: &scope,
                target: self.current_target_name(),
                nick: &self.my_nick,
            },
        ) {
            Ok(plan) => self.execute_alias_plan(plan),
            Err(error) => self.add_message_to_current(
                ChatMessage::system(&format!("Alias not run: {error}")).without_logging(),
            ),
        }
    }
    fn automation_scope(&self) -> String {
        self.logical_network_key()
            .map(|key| key.storage_key())
            .unwrap_or_default()
    }
    pub(crate) fn set_automation(&mut self, config: Arc<AutomationConfig>) {
        self.automation = config;
    }
    fn command_plan(&self, input: &str) -> Result<CommandPlan, String> {
        self.command_plan_for_target(input, self.current_target_name())
    }
    fn command_plan_for_target(
        &self,
        input: &str,
        target: Option<&str>,
    ) -> Result<CommandPlan, String> {
        self.automation.expand(
            input,
            &ExpansionContext {
                scope: &self.automation_scope(),
                target,
                nick: &self.my_nick,
            },
        )
    }
    pub(crate) fn draft_is_sensitive(&self, input: &str) -> bool {
        self.draft_is_sensitive_for(self.current_target_name(), input)
    }
    pub(crate) fn draft_is_sensitive_for(&self, target: Option<&str>, input: &str) -> bool {
        let input = input.trim_start();
        // Preserve the existing conservative history filter for bare service commands too.
        if super::commands::command_line_contains_credentials(input) {
            return true;
        }
        if !input.starts_with('/') {
            return target.is_some_and(|target| {
                super::commands::service_message_contains_credentials(target, input)
            });
        }
        self.command_plan_for_target(input, target)
            .map_or(true, |plan| plan.sensitive)
    }
    pub(crate) fn draft_for_owner_is_sensitive(
        &self,
        owner: Option<super::conversations::ConversationId>,
        input: &str,
    ) -> bool {
        match owner {
            Some(id) => self
                .target_name(id)
                .is_none_or(|target| self.draft_is_sensitive_for(Some(target), input)),
            None => self.draft_is_sensitive_for(None, input),
        }
    }
    pub fn process_command(&mut self, input: &str) {
        if input.is_empty() {
            return;
        }
        match self.command_plan(input) {
            Err(error) => self.add_message_to_current(
                ChatMessage::system(&format!("Alias not run: {error}")).without_logging(),
            ),
            Ok(plan) if !plan.aliased => self.process_builtin_command(input),
            Ok(plan) => self.execute_alias_plan(plan),
        }
    }
    fn execute_alias_plan(&mut self, plan: CommandPlan) {
        if plan.steps.len() == 1 {
            self.process_builtin_command(&plan.steps[0]);
            return;
        }
        if self.macro_jobs.len() >= MAX_JOBS {
            self.add_message_to_current(
                ChatMessage::system("Macro queue is full").without_logging(),
            );
            return;
        }
        self.macro_jobs.push_back(MacroJob {
            generation: self.connection.generation(),
            conversation: self.current_conversation,
            target: self.current_target_name().map(str::to_owned),
            steps: plan.steps.into(),
        });
        self.run_macro_jobs(Instant::now());
    }
    pub(crate) fn run_macro_jobs(&mut self, now: Instant) {
        if now < self.next_macro_at {
            return;
        }
        let Some(mut job) = self.macro_jobs.pop_front() else {
            return;
        };
        if job.generation != self.connection.generation()
            || job.conversation.and_then(|id| self.target_name(id)) != job.target.as_deref()
        {
            self.add_server_message(
                ChatMessage::system(
                    "Macro cancelled because its connection or conversation changed",
                )
                .without_logging(),
            );
            return;
        }
        let Some(step) = job.steps.pop_front() else {
            return;
        };
        let before = self.current_conversation;
        self.current_conversation = job.conversation;
        self.automation_send_failed = false;
        self.process_builtin_command(&step);
        if before.is_none_or(|id| self.target_name(id).is_some()) {
            self.current_conversation = before;
        }
        self.next_macro_at = now + MACRO_INTERVAL;
        if !self.automation_send_failed && !job.steps.is_empty() {
            self.macro_jobs.push_front(job);
        }
    }
    pub(super) fn expand_auto_perform(&mut self, script: &str) -> String {
        let mut steps = Vec::new();
        for line in script.lines().filter(|line| !line.trim().is_empty()) {
            match self.command_plan(line.trim()) {
                Ok(plan) => steps.extend(plan.steps),
                Err(error) => {
                    self.add_server_message(
                        ChatMessage::system(&format!("Auto-perform not run: {error}"))
                            .without_logging(),
                    );
                    return String::new();
                }
            }
            if steps.len() > MAX_STEPS || steps.iter().map(String::len).sum::<usize>() > MAX_BYTES {
                self.add_server_message(
                    ChatMessage::system("Auto-perform exceeds 32 commands or 64 KiB")
                        .without_logging(),
                );
                return String::new();
            }
        }
        steps.join("\n")
    }
    pub(super) fn alias_completions(&self, prefix: &str) -> Vec<String> {
        let prefix = prefix.trim_start_matches('/').to_ascii_lowercase();
        let scope = self.automation_scope();
        let mut names = self
            .automation
            .aliases
            .iter()
            .filter(|alias| {
                (alias.scope.is_none() || alias.scope.as_deref() == Some(scope.as_str()))
                    && alias.name.to_ascii_lowercase().starts_with(&prefix)
            })
            .map(|alias| format!("/{}", alias.name))
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        names
    }
}

pub(crate) struct AutomationUi {
    pub config: Arc<AutomationConfig>,
    pub editing: AutomationConfig,
    pub path: Option<PathBuf>,
    pub error: Option<String>,
    /// The file on disk could not be loaded. `editing` holds defaults, so
    /// committing would destroy the user's file; it is refused until the file
    /// is explicitly discarded (and kept as a backup).
    blocked: bool,
    selected: Option<usize>,
    name: String,
    scope: String,
    body: String,
    capture: Option<usize>,
}
impl Default for AutomationUi {
    fn default() -> Self {
        let config = AutomationConfig::default();
        Self {
            config: Arc::new(config.clone()),
            editing: config,
            path: None,
            error: None,
            blocked: false,
            selected: None,
            name: String::new(),
            scope: String::new(),
            body: String::new(),
            capture: None,
        }
    }
}
impl AutomationUi {
    pub(crate) fn load_from(path: PathBuf) -> Self {
        let mut ui = Self {
            path: Some(path.clone()),
            ..Self::default()
        };
        let bytes = std::fs::File::open(&path).and_then(|file| {
            let mut bytes = Vec::new();
            file.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > 8 * 1024 * 1024 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Automation file exceeds 8 MiB",
                ));
            }
            Ok(bytes)
        });
        match bytes {
            Ok(bytes) => match serde_json::from_slice::<AutomationConfig>(&bytes) {
                Ok(config) => match config.validate() {
                    Ok(()) => {
                        ui.config = Arc::new(config.clone());
                        ui.editing = config;
                    }
                    Err(error) => ui.error = Some(error),
                },
                Err(_) => {
                    ui.error =
                        Some("Automation file is invalid; preserved without overwriting".into())
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                ui.error = Some(
                    "Automation configuration could not be read; preserved without overwriting"
                        .into(),
                )
            }
        }
        if ui.error.is_some() {
            let unavailable = AutomationConfig {
                unavailable: true,
                ..AutomationConfig::default()
            };
            ui.config = Arc::new(unavailable);
            ui.blocked = true;
        }
        ui
    }
    /// Move the unloadable file aside to a backup so the editor may start
    /// fresh. Aliases stay unavailable until the next commit.
    pub(crate) fn discard_unusable_file(&mut self) -> Result<(), String> {
        if !self.blocked {
            return Ok(());
        }
        if let Some(path) = &self.path {
            match crate::persistence::quarantine(path) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("Could not back up the unreadable automation file".into()),
            }
        }
        self.blocked = false;
        self.error = None;
        Ok(())
    }
    pub(crate) fn commit(&mut self) -> Result<(), String> {
        if self.blocked {
            return Err(
                "The existing automation file is unreadable or invalid and was not \
                 overwritten. Fix the file, or discard it (a backup is kept)."
                    .into(),
            );
        }
        self.editing.validate()?;
        if let Some(path) = &self.path {
            let bytes = serde_json::to_vec_pretty(&self.editing)
                .map_err(|_| "Cannot serialize automation".to_string())?;
            crate::persistence::write_private_atomic(path, &bytes)
                .map_err(|_| "Cannot save automation configuration".to_string())?;
        }
        self.config = Arc::new(self.editing.clone());
        Ok(())
    }
    pub(crate) fn draw(&mut self, ui: &mut egui::Ui, current_scope: &str) -> bool {
        let captured_events = if self.capture.is_some() && !ui.is_sizing_pass() {
            ui.ctx().input_mut(|input| {
                let events = input.events.clone();
                input.events.retain(|event| {
                    !matches!(
                        event,
                        egui::Event::Key { .. } | egui::Event::Text(_) | egui::Event::Paste(_)
                    )
                });
                events
            })
        } else {
            Vec::new()
        };
        if captured_events.iter().any(|event| {
            matches!(
                event,
                egui::Event::Key {
                    key: egui::Key::Escape,
                    pressed: true,
                    ..
                }
            )
        }) {
            self.capture = None;
        }
        let mut applied = false;
        ui.separator();
        egui::CollapsingHeader::new("Aliases, macros and keyboard shortcuts").show(ui, |ui| {
            ui.label("Aliases have 1–16 commands. Built-in command names are reserved.");
            ui.label("Parameters: $1..$9, $1- (remainder), $*, $target, $nick, $$.");
            egui::ComboBox::from_id_salt("alias_selection")
                .selected_text(
                    self.selected
                        .and_then(|i| self.editing.aliases.get(i))
                        .map_or("New alias", |a| a.name.as_str()),
                )
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.selected.is_none(), "New alias")
                        .clicked()
                    {
                        self.selected = None;
                        self.name.clear();
                        self.body.clear();
                        self.scope.clear();
                    }
                    for (index, alias) in self.editing.aliases.iter().enumerate() {
                        if ui
                            .selectable_label(self.selected == Some(index), &alias.name)
                            .clicked()
                        {
                            self.selected = Some(index);
                            self.name = alias.name.clone();
                            self.body = alias.steps.join("\n");
                            self.scope = alias.scope.clone().unwrap_or_default();
                        }
                    }
                });
            ui.horizontal_wrapped(|ui| {
                ui.label("Name:");
                ui.text_edit_singleline(&mut self.name);
            });
            let mut scoped = !self.scope.is_empty();
            if ui
                .add_enabled(
                    !current_scope.is_empty(),
                    egui::Checkbox::new(&mut scoped, "Only this network"),
                )
                .changed()
            {
                self.scope = if scoped {
                    current_scope.into()
                } else {
                    String::new()
                };
            }
            ui.add(
                egui::TextEdit::multiline(&mut self.body)
                    .desired_rows(4)
                    .desired_width(ui.available_width()),
            );
            ui.horizontal_wrapped(|ui| {
                if ui.button("Add / replace alias").clicked() {
                    let alias = AliasDef {
                        id: self
                            .selected
                            .and_then(|i| self.editing.aliases.get(i))
                            .map_or_else(super::new_profile_id, |a| a.id.clone()),
                        name: self.name.trim().into(),
                        scope: (!self.scope.is_empty()).then(|| self.scope.clone()),
                        steps: self
                            .body
                            .lines()
                            .filter(|s| !s.trim().is_empty())
                            .map(|s| s.trim().into())
                            .collect(),
                    };
                    let mut next = self.editing.clone();
                    if let Some(index) = self.selected {
                        next.aliases[index] = alias;
                    } else {
                        next.aliases.push(alias);
                    }
                    match next.validate() {
                        Ok(()) => {
                            self.selected = Some(self.selected.unwrap_or(next.aliases.len() - 1));
                            self.editing = next;
                            self.error = None;
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
                if ui
                    .add_enabled(self.selected.is_some(), egui::Button::new("Delete alias"))
                    .clicked()
                    && let Some(index) = self.selected.take()
                {
                    let removed = self.editing.aliases.remove(index);
                    self.editing.shortcuts.retain(|b| {
                        b.action
                            != ActionId::RunAlias {
                                id: removed.id.clone(),
                            }
                    });
                    self.name.clear();
                    self.body.clear();
                    self.capture = None;
                }
            });
            ui.separator();
            ui.label("Click a shortcut to capture a replacement. Escape cancels capture.");
            if let Some(index) = self.capture {
                for event in captured_events {
                    if let egui::Event::Key {
                        key,
                        modifiers,
                        pressed: true,
                        repeat: false,
                        ..
                    } = event
                    {
                        if key == egui::Key::Escape {
                            self.capture = None;
                            break;
                        }
                        let chord = KeyChord::from_event(key, modifiers);
                        if chord.valid() {
                            self.editing.shortcuts[index].chord = chord;
                            self.capture = None;
                            break;
                        }
                        self.error = Some("Use a modifier or a function key".into());
                    }
                }
            }
            let mut remove_binding = None;
            for (index, binding) in self.editing.shortcuts.iter().enumerate() {
                ui.horizontal_wrapped(|ui| {
                    ui.label(binding.action.label());
                    if ui
                        .button(if self.capture == Some(index) {
                            "Press shortcut…".into()
                        } else {
                            binding.chord.label()
                        })
                        .clicked()
                    {
                        self.capture = Some(index);
                    }
                    if ui.small_button("Disable").clicked() {
                        remove_binding = Some(index);
                    }
                });
            }
            if let Some(index) = remove_binding {
                self.editing.shortcuts.remove(index);
                self.capture = None;
            }
            if let Some(index) = self.selected
                && let Some(alias) = self.editing.aliases.get(index)
                && ui.button("Add shortcut for selected alias").clicked()
            {
                self.editing.shortcuts.push(ShortcutBinding {
                    chord: KeyChord::new("F12", false, false, false),
                    action: ActionId::RunAlias {
                        id: alias.id.clone(),
                    },
                });
                self.capture = Some(self.editing.shortcuts.len() - 1);
            }
            ui.horizontal_wrapped(|ui| {
                if ui.button("Reset keyboard defaults").clicked() {
                    self.editing.shortcuts = super::actions::default_bindings();
                    self.capture = None;
                }
                if self.blocked
                    && ui
                        .button("Discard unreadable file and start fresh")
                        .on_hover_text("The file is kept next to it as automation.json.bak")
                        .clicked()
                    && let Err(error) = self.discard_unusable_file()
                {
                    self.error = Some(error);
                }
                if ui
                    .add_enabled(!self.blocked, egui::Button::new("Apply and save automation"))
                    .clicked()
                {
                    match self.commit() {
                        Ok(()) => {
                            applied = true;
                            self.error = None;
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
            });
            if let Some(error) = &self.error {
                ui.colored_label(egui::Color32::RED, error);
            }
        });
        applied
    }
}

fn reserved(name: &str) -> bool {
    const BUILTINS: &[&str] = &[
        "ACCEPT",
        "ADMIN",
        "ALIAS",
        "AME",
        "AMSG",
        "AS",
        "AUTH",
        "AUTHENTICATE",
        "AUTHSERV",
        "AWAY",
        "BACK",
        "BAN",
        "BOTSERV",
        "BOUNCER",
        "BS",
        "CHANSERV",
        "CHATHISTORY",
        "CLEAR",
        "CLOSE",
        "CNOTICE",
        "CPRIVMSG",
        "CS",
        "CTCP",
        "CYCLE",
        "DEHALFOP",
        "DEHOP",
        "DEOP",
        "DESCRIBE",
        "DEVOICE",
        "DISCONNECT",
        "ECHO",
        "GHOST",
        "GREP",
        "GROUP",
        "H",
        "HALFOP",
        "HELP",
        "HISTORY",
        "HOP",
        "HOSTSERV",
        "HS",
        "ID",
        "IDENTIFY",
        "IGNORE",
        "INFO",
        "INVITE",
        "ISON",
        "J",
        "JOIN",
        "K",
        "KB",
        "KICK",
        "KICKBAN",
        "KNOCK",
        "LABEL",
        "LASTLOG",
        "LEAVE",
        "LINKS",
        "LIST",
        "LOGIN",
        "LUSERS",
        "M",
        "MARKREAD",
        "ME",
        "MEMOSERV",
        "METADATA",
        "MODE",
        "MON",
        "MONITOR",
        "MOTD",
        "MS",
        "MSG",
        "N",
        "NAMES",
        "NEWPASS",
        "NICK",
        "NICKSERV",
        "NOTICE",
        "NS",
        "ONOTICE",
        "OP",
        "OPER",
        "OPERSERV",
        "OS",
        "PART",
        "PASS",
        "PERFORM",
        "PERSISTENCE",
        "PING",
        "PRIVMSG",
        "Q",
        "QUERY",
        "QUIT",
        "QUOTE",
        "RAW",
        "REACT",
        "RECONNECT",
        "RECOVER",
        "REDACT",
        "REGAIN",
        "REGISTER",
        "REJOIN",
        "RELEASE",
        "RELOCATE",
        "RENAME",
        "REPLY",
        "RESETPASS",
        "SAY",
        "SEARCH",
        "SERVER",
        "SETNAME",
        "SETPASS",
        "SETTINGS",
        "SHELP",
        "SIDENTIFY",
        "SILENCE",
        "SLAP",
        "STATS",
        "SVERSION",
        "TIME",
        "TOKEN",
        "TOPIC",
        "TRACE",
        "TYPING",
        "UMODE",
        "UNBAN",
        "UNIGNORE",
        "UPLOAD",
        "USERHOST",
        "V",
        "VERIFY",
        "VERSION",
        "VOICE",
        "WALLCHOPS",
        "WALLOPS",
        "WALLVOICES",
        "WC",
        "WEBPUSH",
        "WHO",
        "WHOIS",
        "WHOWAS",
        "WINDOW",
    ];
    BUILTINS.contains(&name.to_ascii_uppercase().as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irc::{IrcCommand, IrcMessage};
    fn alias(name: &str, steps: &[&str]) -> AliasDef {
        AliasDef {
            id: format!("id-{name}"),
            name: name.into(),
            scope: None,
            steps: steps.iter().map(|step| step.to_string()).collect(),
        }
    }
    fn config(aliases: Vec<AliasDef>) -> AutomationConfig {
        AutomationConfig {
            aliases,
            ..AutomationConfig::default()
        }
    }
    fn context() -> ExpansionContext<'static> {
        ExpansionContext {
            scope: "network",
            target: Some("#room"),
            nick: "me",
        }
    }
    fn session(
        config: AutomationConfig,
    ) -> (Session, tokio::sync::mpsc::UnboundedReceiver<IrcCommand>) {
        let mut session = Session::with_settings(super::super::Settings {
            logging_enabled: false,
            ..super::super::Settings::default()
        });
        session.set_automation(Arc::new(config));
        session.set_session_nick("me".into(), true);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        session.test_set_registered(tx);
        (session, rx)
    }
    #[test]
    fn shortcut_runs_the_selected_alias_id_when_a_network_alias_shadows_its_name() {
        let global = alias("greet", &["/raw PING :chosen.example"]);
        let (mut session, mut rx) = session(config(vec![global.clone()]));
        session.nickname = "me".into();
        session.active_session = Some(session.session_from_form().unwrap());
        let mut scoped = alias("greet", &["/raw PING :shadow.example"]);
        scoped.id = "network-greet".into();
        scoped.scope = Some(session.automation_scope());
        session.set_automation(Arc::new(config(vec![global.clone(), scoped])));

        session.run_alias_id(&global.id);
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Raw("PING :chosen.example".into())
        );
    }

    #[test]
    fn indented_login_alias_drafts_are_still_sensitive() {
        let (session, _) = session(config(vec![alias("mylogin", &["/ns IDENTIFY $1"])]));
        assert!(session.draft_is_sensitive("  /mylogin synthetic-secret"));
    }

    #[test]
    fn selected_alias_id_keeps_scoped_resolution_for_nested_aliases() {
        let mut nested = alias("child", &["/raw PING :network.example"]);
        nested.id = "scoped-child".into();
        nested.scope = Some("network".into());
        let cfg = config(vec![
            alias("parent", &["/child"]),
            alias("child", &["/raw PING :global.example"]),
            nested,
        ]);
        cfg.validate().unwrap();
        assert_eq!(
            cfg.expand_id("id-parent", &context()).unwrap().steps,
            ["/raw PING :network.example"]
        );
        assert!(
            cfg.expand_id(
                "scoped-child",
                &ExpansionContext {
                    scope: "other-network",
                    ..context()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn positional_parameters_preserve_remainder_and_never_split_command_boundaries() {
        let cfg = config(vec![alias(
            "greet",
            &["/msg $1 Hi $2-; literal | $$ $nick $target"],
        )]);
        cfg.validate().unwrap();
        let plan = cfg
            .expand("/greet alice several  words", &context())
            .unwrap();
        assert_eq!(
            plan.steps,
            ["/msg alice Hi several  words; literal | $ me #room"]
        );
    }
    #[test]
    fn missing_argument_aborts_entire_macro_before_any_step_runs() {
        let (mut session, mut rx) =
            session(config(vec![alias("two", &["/echo start", "/msg $2 hi"])]));
        session.process_command("/two only-one");
        assert!(rx.try_recv().is_err());
        assert!(session.macro_jobs.is_empty());
        assert!(
            !session
                .server_messages
                .iter()
                .any(|message| message.content == "start")
        );
        assert!(session.draft_is_sensitive("/two only-one"));
    }
    #[test]
    fn aliases_cannot_shadow_builtins_and_cycles_are_rejected() {
        assert!(
            config(vec![alias("join", &["/echo bad"])])
                .validate()
                .is_err()
        );
        assert!(
            config(vec![alias("one", &["/two"]), alias("two", &["/one"])])
                .validate()
                .is_err()
        );
    }
    #[test]
    fn nested_sensitive_alias_never_enters_command_history() {
        let cfg = config(vec![
            alias("identifyme", &["/msg AuthServ AUTH account $1"]),
            alias("mylogin", &["/identifyme $1"]),
        ]);
        let (mut session, mut rx) = session(cfg);
        session.input_text = "/mylogin scratch-secret".into();
        session.process_input();
        assert!(session.command_history.is_empty());
        assert!(
            matches!(rx.try_recv(),Ok(IrcCommand::Privmsg(target,content))
            if target=="AuthServ" && content=="AUTH account scratch-secret")
        );
        assert!(session.draft_is_sensitive("/mylogin scratch-secret"));
    }
    #[test]
    fn scoped_alias_overrides_global_and_missing_scope_uses_global() {
        let mut local = alias("greet", &["/echo local"]);
        local.id = "scoped".into();
        local.scope = Some("network".into());
        let cfg = config(vec![alias("greet", &["/echo global"]), local]);
        cfg.validate().unwrap();
        assert_eq!(
            cfg.expand("/greet", &context()).unwrap().steps,
            ["/echo local"]
        );
        assert_eq!(
            cfg.expand(
                "/greet",
                &ExpansionContext {
                    scope: "other",
                    ..context()
                }
            )
            .unwrap()
            .steps,
            ["/echo global"]
        );
    }
    #[test]
    fn expansion_is_bounded_even_for_nonrecursive_fanout() {
        let cfg = config(vec![
            alias("leaf", &["/echo a", "/echo b", "/echo c"]),
            alias(
                "many",
                &[
                    "/leaf", "/leaf", "/leaf", "/leaf", "/leaf", "/leaf", "/leaf", "/leaf",
                    "/leaf", "/leaf", "/leaf",
                ],
            ),
        ]);
        assert!(cfg.expand("/many", &context()).is_err());
    }
    #[test]
    fn startup_alias_is_expanded_before_authentication_history_gate() {
        let (mut session, mut rx) = session(config(vec![alias(
            "myidentify",
            &["/msg AuthServ AUTH account placeholder"],
        )]));
        session.auto_perform = "/myidentify".into();
        session.auto_join_channels = "#room".into();
        session.set_invisible = false;
        session.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/chathistory").unwrap(),
        );
        session.handle_incoming_message(IrcMessage::parse(":srv 001 me :welcome").unwrap());
        session.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #room").unwrap());
        let before: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            !before
                .iter()
                .any(|command| matches!(command, IrcCommand::ChathistoryLatest(..)))
        );
        session.run_timed_tasks();
        assert!(
            std::iter::from_fn(|| rx.try_recv().ok()).any(
                |command| matches!(command,IrcCommand::Privmsg(target,_) if target=="AuthServ")
            )
        );
    }
    #[test]
    fn macro_plan_survives_definition_edit_but_not_connection_replacement() {
        let (mut session, mut rx) = session(config(vec![alias(
            "steps",
            &["/raw PING one", "/raw PING two"],
        )]));
        session.process_command("/steps");
        assert!(matches!(rx.try_recv(),Ok(IrcCommand::Raw(line)) if line=="PING one"));
        session.set_automation(Arc::new(config(vec![alias(
            "steps",
            &["/raw PING changed"],
        )])));
        session.run_macro_jobs(Instant::now() + Duration::from_secs(1));
        assert!(matches!(rx.try_recv(),Ok(IrcCommand::Raw(line)) if line=="PING two"));
        session.process_command("/steps");
        session.set_automation(Arc::new(config(vec![alias(
            "again",
            &["/raw PING first", "/raw PING second"],
        )])));
        session.next_macro_at = Instant::now();
        session.process_command("/again");
        assert!(!session.macro_jobs.is_empty());
        session.begin_connection_attempt();
        session.run_macro_jobs(Instant::now() + Duration::from_secs(2));
        assert!(session.macro_jobs.is_empty());
    }
    #[test]
    fn memory_store_commit_does_not_require_a_profile_path() {
        let mut store = AutomationUi::default();
        store.editing.aliases.push(alias("hi", &["/echo hi"]));
        store.commit().unwrap();
        assert_eq!(store.config.aliases.len(), 1);
        assert!(store.path.is_none());
    }
    #[test]
    fn invalid_shortcut_conflict_does_not_replace_runtime_configuration() {
        let mut store = AutomationUi::default();
        store
            .editing
            .shortcuts
            .push(store.editing.shortcuts[0].clone());
        assert!(store.commit().is_err());
        assert_eq!(
            store.config.shortcuts,
            super::super::actions::default_bindings()
        );
    }
    #[test]
    fn rejected_reply_stale_channel_and_wire_validation_abort_remaining_macro_steps() {
        for first in [
            "/reply parent body".to_string(),
            "/msg #stale body".to_string(),
            format!("/raw PRIVMSG #room :{}", "x".repeat(600)),
        ] {
            let (mut session, mut rx) = session(config(vec![alias(
                "halt",
                &[&first, "/raw PING must-not-run"],
            )]));
            let active = session.session_from_form().unwrap();
            session.activate_session(active);
            let room = session.ensure_history_target("#room");
            session.conversation_channel_mut(room).unwrap().joined = true;
            session.ensure_history_target("#stale");
            session.select_conversation(Some(room));
            session.process_command("/halt");
            assert!(
                session.automation_send_failed,
                "rejection was not propagated: {first}"
            );
            assert!(
                session.macro_jobs.is_empty(),
                "macro was not stopped: {first}"
            );
            session.run_macro_jobs(Instant::now() + Duration::from_secs(2));
            assert!(
                rx.try_recv().is_err(),
                "a failed send allowed a later command: {first}"
            );
        }
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    #[test]
    fn private_store_roundtrips_and_invalid_input_is_preserved() {
        let directory = std::env::temp_dir().join(format!(
            "linefeed-automation-{}",
            super::super::new_profile_id()
        ));
        let path = directory.join("automation.json");
        let mut store = AutomationUi {
            path: Some(path.clone()),
            ..AutomationUi::default()
        };
        store.editing.aliases.push(AliasDef {
            id: "greeting".into(),
            name: "greeting".into(),
            scope: None,
            steps: vec!["/echo hello".into()],
        });
        store.commit().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            AutomationUi::load_from(path.clone()).config.aliases[0].name,
            "greeting"
        );
        std::fs::write(&path, b"invalid configuration").unwrap();
        let invalid = AutomationUi::load_from(path.clone());
        assert!(invalid.error.is_some());
        assert_eq!(std::fs::read(&path).unwrap(), b"invalid configuration");
        let mut session = Session::with_settings(super::super::Settings {
            logging_enabled: false,
            ..Default::default()
        });
        session.set_automation(invalid.config);
        assert!(session.draft_is_sensitive("/greeting private-argument"));
        session.process_command("/greeting private-argument");
        assert!(
            !session
                .server_messages
                .iter()
                .any(|message| message.content.contains("private-argument"))
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn scratch_automation_path(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "linefeed-automation-{name}-{}",
            super::super::new_profile_id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory.join("automation.json")
    }

    #[test]
    fn an_invalid_file_is_not_overwritten_until_it_is_explicitly_discarded() {
        let reserved_alias = br#"{"version":1,"aliases":[{"id":"x","name":"away","steps":["/echo hi"]}],"shortcuts":[]}"#;
        let cases: [&[u8]; 3] = [
            b"invalid configuration",
            reserved_alias,
            br#"{"version":2,"aliases":[],"shortcuts":[]}"#,
        ];
        for (n, original) in cases.into_iter().enumerate() {
            let path = scratch_automation_path(&format!("blocked{n}"));
            std::fs::write(&path, original).unwrap();
            let mut ui = AutomationUi::load_from(path.clone());
            assert!(ui.error.is_some(), "case {n}");

            assert!(ui.commit().is_err(), "case {n}: commit must be refused");
            assert_eq!(std::fs::read(&path).unwrap(), original, "case {n}");

            ui.discard_unusable_file().unwrap();
            assert!(ui.error.is_none());
            assert_eq!(
                std::fs::read(path.with_file_name("automation.json.bak")).unwrap(),
                original,
                "case {n}: the backup keeps the original bytes"
            );
            ui.commit().unwrap();
            assert!(AutomationUi::load_from(path.clone()).error.is_none());
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn a_missing_file_commits_without_a_discard() {
        let path = scratch_automation_path("missing");
        let mut ui = AutomationUi::load_from(path.clone());
        assert!(ui.error.is_none());
        ui.commit().unwrap();
        assert!(path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn discarding_twice_keeps_every_backup() {
        let path = scratch_automation_path("twice");
        for original in [&b"first broken"[..], &b"second broken"[..]] {
            std::fs::write(&path, original).unwrap();
            let mut ui = AutomationUi::load_from(path.clone());
            ui.discard_unusable_file().unwrap();
        }
        assert_eq!(
            std::fs::read(path.with_file_name("automation.json.bak")).unwrap(),
            b"first broken"
        );
        assert_eq!(
            std::fs::read(path.with_file_name("automation.json.bak.1")).unwrap(),
            b"second broken"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
