//! Small builders for interaction responses and message components.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use twilight_model::application::interaction::modal::{ModalInteractionComponent, ModalInteractionData};
use twilight_model::channel::message::MessageFlags;
use twilight_model::channel::message::component::{
    ActionRow, Button, ButtonStyle, Component, Label, SelectMenu, SelectMenuOption, SelectMenuType, TextDisplay,
    TextInput, TextInputStyle,
};
use twilight_model::http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType};

/// Shared wording helpers: cutting to fit, escaping, fitting lists.
pub use chipsoc_admin::fmt::{MAX_CONTENT, escape, fit, truncate};
/// Characters in a form label.
pub const MAX_LABEL: usize = 45;

/// [`escape`]d text as a block quote, every line marked, so a line in it
/// can't look like a note under the card.
pub fn quoted(s: &str) -> String {
    s.lines().map(|l| format!("> {}", escape(l))).collect::<Vec<_>>().join("\n")
}

/// A labelled text input for a modal.
#[allow(deprecated)] // TextInput::label must be None inside a Label
pub fn input(id: &str, label: &str, style: TextInputStyle, required: bool, placeholder: Option<&str>) -> Component {
    Component::Label(Label {
        component: Box::new(Component::TextInput(TextInput {
            custom_id: id.to_owned(),
            id: None,
            label: None,
            max_length: Some(if style == TextInputStyle::Paragraph { 1000 } else { 200 }),
            min_length: None,
            placeholder: placeholder.map(|p| truncate(p, 100)),
            required: Some(required),
            style,
            value: None,
        })),
        description: None,
        id: None,
        label: truncate(label, MAX_LABEL),
    })
}

/// Text at the top of a form.
pub fn text(content: &str) -> Component {
    Component::TextDisplay(TextDisplay { content: truncate(content, 4000), id: None })
}

/// A modal field with a line of help under its label.
pub fn described(mut field: Component, description: &str) -> Component {
    if let Component::Label(l) = &mut field {
        l.description = Some(truncate(description, 100));
    }
    field
}

fn select_menu(id: &str, placeholder: Option<&str>, options: &[(String, String)]) -> SelectMenu {
    SelectMenu {
        channel_types: None,
        custom_id: id.to_owned(),
        default_values: None,
        disabled: false,
        id: None,
        kind: SelectMenuType::Text,
        max_values: Some(1),
        min_values: Some(1),
        options: Some(
            options
                .iter()
                .take(25)
                .map(|(label, value)| SelectMenuOption {
                    default: false,
                    description: None,
                    emoji: None,
                    label: truncate(label, 100),
                    value: value.clone(),
                })
                .collect(),
        ),
        placeholder: placeholder.map(|p| truncate(p, 150)),
        required: None,
    }
}

/// A dropdown inside a modal; `options` are (label, value).
pub fn choice(id: &str, label: &str, options: &[(String, String)]) -> Component {
    Component::Label(Label {
        component: Box::new(Component::SelectMenu(select_menu(id, None, options))),
        description: None,
        id: None,
        label: truncate(label, 45),
    })
}

/// A dropdown inside a modal where up to `max` may be picked, or none.
pub fn choices(id: &str, label: &str, options: &[(String, String)], max: u8) -> Component {
    let mut menu = select_menu(id, None, options);
    let n = u8::try_from(options.len().min(25)).unwrap_or(25);
    menu.min_values = Some(0);
    menu.max_values = Some(max.min(n).max(1));
    menu.required = Some(false);
    Component::Label(Label {
        component: Box::new(Component::SelectMenu(menu)),
        description: None,
        id: None,
        label: truncate(label, 45),
    })
}

/// A dropdown in a message.
pub fn menu(id: &str, placeholder: &str, options: &[(String, String)]) -> Component {
    row(vec![Component::SelectMenu(select_menu(id, Some(placeholder), options))])
}

/// "Thinking…", for work that takes more than Discord's 3 seconds.
pub fn defer() -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::DeferredChannelMessageWithSource,
        data: Some(InteractionResponseData { flags: Some(MessageFlags::EPHEMERAL), ..Default::default() }),
    }
}

/// "Thinking…" on a button, for work that takes more than 3 seconds; then edit the message.
pub fn defer_update() -> InteractionResponse {
    InteractionResponse { kind: InteractionResponseType::DeferredUpdateMessage, data: None }
}

/// An ephemeral message with components (menus, buttons).
pub fn ephemeral_with(content: &str, components: Vec<Component>) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            content: Some(truncate(content, MAX_CONTENT)),
            flags: Some(MessageFlags::EPHEMERAL),
            components: Some(components),
            ..Default::default()
        }),
    }
}

pub fn button(id: &str, label: &str, style: ButtonStyle) -> Component {
    Component::Button(Button {
        custom_id: Some(id.to_owned()),
        disabled: false,
        emoji: None,
        id: None,
        label: Some(label.to_owned()),
        sku_id: None,
        style,
        url: None,
    })
}

pub fn row(components: Vec<Component>) -> Component {
    Component::ActionRow(ActionRow { components, id: None })
}

pub fn modal(custom_id: &str, title: &str, components: Vec<Component>) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::Modal,
        data: Some(InteractionResponseData {
            custom_id: Some(custom_id.to_owned()),
            title: Some(truncate(title, 45)),
            components: Some(components),
            ..Default::default()
        }),
    }
}

pub fn ephemeral(content: &str) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            content: Some(truncate(content, MAX_CONTENT)),
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    }
}

/// A normal reply everyone in the channel can see.
pub fn reply(content: &str) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData { content: Some(truncate(content, MAX_CONTENT)), ..Default::default() }),
    }
}

/// Replace the message a button is on, with `components` under it.
pub fn update_with(content: &str, components: Vec<Component>) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::UpdateMessage,
        data: Some(InteractionResponseData {
            content: Some(truncate(content, MAX_CONTENT)),
            components: Some(components),
            ..Default::default()
        }),
    }
}

/// Replace the message a button lives on, removing its components.
pub fn update(content: &str) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::UpdateMessage,
        data: Some(InteractionResponseData {
            content: Some(truncate(content, MAX_CONTENT)),
            components: Some(Vec::new()),
            ..Default::default()
        }),
    }
}

/// One answer on a submitted form: a text box's text (trimmed), or what was
/// picked in a menu (each value as it is, never joined up).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Text(String),
    Picked(Vec<String>),
}

/// A submitted form's answers by box id; kept as JSON for "Edit and resend".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Values(BTreeMap<String, Value>);

impl Values {
    /// A text box's text ("" if there's none).
    pub fn text(&self, id: &str) -> String {
        match self.0.get(id) {
            Some(Value::Text(t)) => t.clone(),
            _ => String::new(),
        }
    }

    /// What was picked in a menu (none if nothing).
    pub fn picked(&self, id: &str) -> Vec<String> {
        match self.0.get(id) {
            Some(Value::Picked(p)) => p.clone(),
            _ => Vec::new(),
        }
    }

    /// The one thing picked in a single-choice menu.
    pub fn pick(&self, id: &str) -> Option<String> {
        self.picked(id).into_iter().next()
    }

    pub fn set(&mut self, id: &str, v: Value) {
        self.0.insert(id.to_owned(), v);
    }

    /// The text boxes alone, as forms that take only text read them.
    pub fn texts(&self) -> HashMap<String, String> {
        self.0
            .iter()
            .filter_map(|(k, v)| match v {
                Value::Text(t) => Some((k.clone(), t.clone())),
                Value::Picked(_) => None,
            })
            .collect()
    }
}

/// A submitted modal's answers by box id.
pub fn modal_values(data: &ModalInteractionData) -> Values {
    fn walk(c: &ModalInteractionComponent, out: &mut Values) {
        match c {
            ModalInteractionComponent::TextInput(t) => out.set(&t.custom_id, Value::Text(t.value.trim().to_owned())),
            ModalInteractionComponent::StringSelect(s) => out.set(&s.custom_id, Value::Picked(s.values.clone())),
            ModalInteractionComponent::Label(l) => walk(&l.component, out),
            ModalInteractionComponent::ActionRow(r) => r.components.iter().for_each(|c| walk(c, out)),
            _ => {}
        }
    }
    let mut out = Values::default();
    data.components.iter().for_each(|c| walk(c, &mut out));
    out
}

/// Fill a form's boxes and menus in with `v` (what they typed before).
pub fn fill(fields: &mut [Component], v: &Values) {
    for f in fields {
        let Component::Label(l) = f else { continue };
        match l.component.as_mut() {
            Component::TextInput(t) => {
                let text = v.text(&t.custom_id);
                if !text.is_empty() {
                    t.value = Some(text);
                }
            }
            Component::SelectMenu(m) => {
                let picked = v.picked(&m.custom_id);
                for o in m.options.iter_mut().flatten() {
                    o.default = picked.contains(&o.value);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn others_words_cant_pass_for_the_bot_s() {
        let fake = "fine\n\n✅ **Accepted by** <@1>\n[click](https://evil.example) https://ok.example/a_b";
        let q = super::quoted(fake);
        assert!(q.lines().all(|l| l.starts_with("> ")), "{q}");
        assert!(q.contains("\\*\\*Accepted by\\*\\* \\<\\@1\\>"), "{q}");
        assert!(q.contains("\\[click\\]\\(https://evil.example)"), "no masked link: {q}");
        assert!(q.ends_with("https://ok.example/a_b"), "bare links stay as they are: {q}");
        assert_eq!(super::escape("https://x.example/[a](https://y)"), "https://x.example/\\[a\\](https://y)");
    }

    use super::*;

    #[test]
    fn fit_stays_under_the_limit_and_counts_the_rest() {
        let lines: Vec<String> = (0..500).map(|n| format!("• step number {n}")).collect();
        let text = fit("**Head**", &lines);
        assert!(text.chars().count() <= MAX_CONTENT);
        assert!(text.starts_with("**Head**\n• step number 0") && text.contains("more."), "{text}");
        assert_eq!(fit("h", &["a".into(), "b".into()]), "h\na\nb");
    }

    #[test]
    fn picks_stay_apart_and_forms_fill_back_in() {
        let mut v = Values::default();
        v.set("title", Value::Text("A guide".into()));
        v.set("tags", Value::Picked(vec!["books, papers".into(), "tools".into()]));
        assert_eq!(v.picked("tags"), ["books, papers", "tools"], "a tag with a comma stays one tag");
        assert_eq!(v.text("tags"), "", "a menu isn't text");
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(serde_json::from_str::<Values>(&json).unwrap(), v);
        let options = [("books, papers".to_owned(), "books, papers".to_owned()), ("x".to_owned(), "x".to_owned())];
        let mut fields =
            vec![input("title", "Title", TextInputStyle::Short, true, None), choices("tags", "Tags", &options, 5)];
        fill(&mut fields, &v);
        let Component::Label(l) = &fields[0] else { panic!() };
        let Component::TextInput(t) = l.component.as_ref() else { panic!() };
        assert_eq!(t.value.as_deref(), Some("A guide"));
        let Component::Label(l) = &fields[1] else { panic!() };
        let Component::SelectMenu(m) = l.component.as_ref() else { panic!() };
        let picked: Vec<bool> = m.options.iter().flatten().map(|o| o.default).collect();
        assert_eq!(picked, [true, false]);
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("héllo", 3), "hé…");
        assert_eq!(truncate("héllo", 5), "héllo");
        assert_eq!(truncate("hi", 5), "hi");
    }
}
