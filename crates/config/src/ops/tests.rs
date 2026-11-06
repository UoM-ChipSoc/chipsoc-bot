//! Each op on a fresh copy of a small, commented config.

use std::fs;
use std::path::Path;

use super::*;
use crate::bot::DriftMode;
use crate::schema::{ModAction, Trigger};
use crate::{Config, Snowflake};

fn write(dir: &Path, file: &str, body: &str) {
    let p = dir.join(file);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// A config with a comment in every file.
fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    write(p, "server.toml", "# the server\n[guild]\nname = \"T\"\n");
    write(
        p,
        "roles.toml",
        "# roles, top first\n[[role]]\nkey = \"admin\"\nname = \"Admin\"\n\n[[role]]\nkey = \"member\"\n\
         name = \"Member\" # everyone who joined\n\n[[role]]\nkey = \"spare\"\nname = \"Spare\"\n\n\
         [[role]]\nkey = \"chair\"\nname = \"Chair\"\n\n[[role]]\nkey = \"guest\"\nname = \"Guest\"\n\n[[role]]\nkey = \"everyone\"\nname = \"@everyone\"\n",
    );
    write(
        p,
        "categories/10-main.toml",
        "# the main category\n[category]\nkey = \"main\"\nname = \"Main\"\n\n[[channel]]\nkey = \"general\"\n\
         name = \"general\"\n\n[[channel]]\nkey = \"rules\"\nname = \"rules\"\npost = \"content/rules.md\"\n\n\
         # questions go here\n[[channel]]\nkey = \"help\"\ntype = \"forum\"\nname = \"help\"\ntags = [\"solved\"]\n\
         [[channel.overwrites]]\nrole = \"member\"\nallow = [\"SEND_MESSAGES\"]\n",
    );
    write(
        p,
        "categories/20-more.toml",
        "# more\n[category]\nkey = \"more\"\nname = \"More\"\n\n[[channel]]\nkey = \"resources\"\ntype = \"forum\"\n\
         name = \"resources\"\n",
    );
    write(p, "categories/30-empty.toml", "# nothing yet\n[category]\nkey = \"empty\"\nname = \"Empty\"\n");
    write(p, "categories/90-archive.toml", "# old things\n[category]\nkey = \"archive\"\nname = \"Archive\"\n");
    write(p, "content/rules.md", "Be kind. Say hi in {#general}.\n{button:volunteer|Volunteer}\n");
    write(
        p,
        "bot.toml",
        "# the bot\nname = \"T\"\nadmin_role = \"admin\"\narchive = \"archive\"\n\n[log]\n\
         people = \"general\" # joins and leaves\n\n# positions\n[[position]]\nrole = \"chair\"\nadmin = true\n\n\
         [[position]]\nrole = \"member\"\n\n[review]\nchannel = \"general\" # applications\n\n[help]\nforum = \"help\"\n\n\
         [projects]\nforum = \"help\"\n",
    );
    write(p, "members.toml", "# who holds what\nadmin = [\"7\"] # elected\n");
    write(
        p,
        "projects.toml",
        "# projects\n[[project]]\nslug = \"golem\"\nname = \"Golem\"\nstatus = \"active\"\nchannels = [\"general\"]\n\
         leads = [\"5\"]\n",
    );
    write(
        p,
        "onboarding.toml",
        "# onboarding\nenabled = true\n\n[[prompt]]\ntitle = \"Q1\"\n[[prompt.option]]\ntitle = \"A\"\nrole = \"guest\"\n\
         [[prompt.option]]\ntitle = \"B\"\nchannels = [\"general\"]\n\n# the second\n[[prompt]]\ntitle = \"Q2\"\n\
         [[prompt.option]]\ntitle = \"C\"\nchannels = [\"rules\"]\n",
    );
    write(
        p,
        "screening.toml",
        "# screening\nenabled = true\ndescription = \"Hi\"\nlabel = \"I agree\"\nrules = [\"Be kind\"]\n",
    );
    write(
        p,
        "automod.toml",
        "# automod\n[[rule]]\nkey = \"spam\"\nname = \"Spam\"\ntrigger = \"spam\"\nactions = [\"block\"] # quietly\n",
    );
    write(
        p,
        "federation.toml",
        "# partners\nname = \"us\"\n\n[[peer]]\nname = \"S\"\nshare = [\"general\"]\nshare_webhook_env = \"S_SHARE\"\n",
    );
    assert_eq!(crate::authoring::check(p).unwrap().errors, Vec::<String>::new(), "the fixture checks clean");
    d
}

fn user(n: u64) -> Snowflake {
    Snowflake(n)
}

fn s(v: &str) -> String {
    v.to_owned()
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_owned()).collect()
}

/// Comments in every file, so we can tell none were lost.
fn comments(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = read_all(dir)
        .unwrap()
        .values()
        .flat_map(|b| String::from_utf8_lossy(b).lines().map(str::to_owned).collect::<Vec<_>>())
        .filter(|l| !l.contains("{#"))
        .filter_map(|l| l.find('#').map(|i| l[i..].to_owned()))
        .collect();
    out.sort();
    out
}

fn role_keys(c: &Config) -> Vec<&str> {
    c.desired.roles.roles.iter().map(|r| r.key.as_str()).collect()
}

fn channel_keys<'a>(c: &'a Config, cat: &str) -> Vec<&'a str> {
    c.desired.category(cat).unwrap().channels.iter().map(|c| c.key.as_str()).collect()
}

fn onboarding(c: &Config) -> &crate::schema::Onboarding {
    c.desired.settings.onboarding.as_ref().unwrap()
}

fn screening(c: &Config) -> &crate::schema::Screening {
    c.desired.settings.screening.as_ref().unwrap()
}

fn help_kept_its_overwrite(c: &Config) -> bool {
    let ows = &c.desired.channel("help").unwrap().overwrites;
    ows.iter().any(|o| o.role.as_deref() == Some("member") && o.allow == ["SEND_MESSAGES"])
}

fn spam(name: &str) -> AutoModRule {
    AutoModRule {
        key: s("spam"),
        name: s(name),
        trigger: Trigger::Spam,
        enabled: true,
        actions: vec![ModAction::Block],
        ..Default::default()
    }
}

type Check = fn(&Config) -> bool;

/// Every op, with how to tell it worked.
fn ops() -> Vec<(Op, Check)> {
    let channel = |key: &str| Channel { key: key.into(), name: key.into(), ..Default::default() };
    let golem = |set| Op::SetProject { slug: s("golem"), set };
    let member = |set| Op::SetPosition { role: s("member"), set };
    let general = |set| Op::SetChannel { key: s("general"), set };
    let option = |set| Op::SetOption { prompt: 0, option: 1, set };
    let section = |section| Op::SetSection { section };
    vec![
        (Op::SetServer { set: ServerField::Name(s("New name")) }, |c| c.desired.server.guild.name == "New name"),
        (Op::SetServer { set: ServerField::SystemChannel(Some(s("general"))) }, |c| {
            c.desired.server.guild.system_channel.as_deref() == Some("general")
        }),
        (Op::SetServer { set: ServerField::VerificationLevel(Some(2)) }, |c| {
            c.desired.server.guild.verification_level == Some(2)
        }),
        (Op::OrderRoles { keys: strings(&["member", "admin"]) }, |c| role_keys(c)[..2] == ["member", "admin"]),
        (Op::OrderCategories { keys: strings(&["more", "main"]) }, |c| c.desired.categories[0].category.key == "more"),
        (Op::OrderChannels { category: Some(s("main")), keys: strings(&["help", "general", "rules"]) }, |c| {
            channel_keys(c, "main") == ["help", "general", "rules"]
        }),
        (Op::AddRole { role: Role { key: s("mod"), name: s("Mod"), ..Default::default() } }, |c| {
            c.desired.role("mod").is_some()
        }),
        (Op::SetRole { key: s("member"), set: RoleField::Name(s("Members")) }, |c| {
            c.desired.role("member").unwrap().name == "Members"
        }),
        (Op::SetRole { key: s("member"), set: RoleField::Color(Some(s("#ff0000"))) }, |c| {
            c.desired.role("member").unwrap().color.as_deref() == Some("#ff0000")
        }),
        (Op::SetRole { key: s("member"), set: RoleField::Hoist(true) }, |c| c.desired.role("member").unwrap().hoist),
        (Op::SetRole { key: s("member"), set: RoleField::Mentionable(true) }, |c| {
            c.desired.role("member").unwrap().mentionable
        }),
        (Op::SetRole { key: s("member"), set: RoleField::Permissions(strings(&["SEND_MESSAGES"])) }, |c| {
            c.desired.role("member").unwrap().permissions == ["SEND_MESSAGES"]
        }),
        (Op::MoveRole { key: s("member"), to: Move::Up }, |c| role_keys(c)[..2] == ["member", "admin"]),
        (Op::MoveRole { key: s("admin"), to: Move::To(2) }, |c| role_keys(c)[..3] == ["member", "spare", "admin"]),
        (Op::RemoveRole { key: s("spare") }, |c| c.desired.role("spare").is_none()),
        (Op::AddCategory { key: s("extra"), name: s("Extra") }, |c| {
            c.desired.categories.last().is_some_and(|cf| cf.category.key == "extra")
        }),
        (Op::RenameCategory { key: s("more"), name: s("More stuff") }, |c| {
            c.desired.category("more").unwrap().category.name == "More stuff"
        }),
        (Op::MoveCategory { key: s("more"), to: Move::Up }, |c| c.desired.categories[0].category.key == "more"),
        (Op::RemoveCategory { key: s("empty") }, |c| c.desired.category("empty").is_none()),
        (Op::AddChannel { category: Some(s("more")), channel: channel("links") }, |c| {
            channel_keys(c, "more") == ["resources", "links"]
        }),
        (Op::AddChannel { category: None, channel: channel("lobby") }, |c| c.desired.channel("lobby").is_some()),
        (general(ChannelField::Name(s("chat"))), |c| c.desired.channel("general").unwrap().name == "chat"),
        (general(ChannelField::Topic(Some(s("Say hi")))), |c| {
            c.desired.channel("general").unwrap().topic.as_deref() == Some("Say hi")
        }),
        (general(ChannelField::Kind(ChannelKind::Announcement)), |c| {
            c.desired.channel("general").unwrap().kind == ChannelKind::Announcement
        }),
        (general(ChannelField::Slowmode(5)), |c| c.desired.channel("general").unwrap().slowmode == 5),
        (general(ChannelField::Nsfw(true)), |c| c.desired.channel("general").unwrap().nsfw),
        (general(ChannelField::UserLimit(Some(4))), |c| c.desired.channel("general").unwrap().user_limit == Some(4)),
        (Op::SetChannel { key: s("help"), set: ChannelField::Tags(strings(&["solved", "new"])) }, |c| {
            c.desired.channel("help").unwrap().tags == ["solved", "new"]
        }),
        (
            Op::SetChannel {
                key: s("help"),
                set: ChannelField::Overwrites(vec![Overwrite {
                    role: Some(s("admin")),
                    deny: strings(&["SEND_MESSAGES"]),
                    ..Default::default()
                }]),
            },
            |c| {
                let o = &c.desired.channel("help").unwrap().overwrites;
                o.len() == 1 && o[0].role.as_deref() == Some("admin") && o[0].deny == ["SEND_MESSAGES"]
            },
        ),
        (general(ChannelField::Inherit(false)), |c| !c.desired.channel("general").unwrap().inherit),
        (
            Op::SetCategoryOverwrites {
                key: s("more"),
                overwrites: vec![Overwrite {
                    role: Some(s("everyone")),
                    deny: strings(&["VIEW_CHANNEL"]),
                    ..Default::default()
                }],
            },
            |c| c.desired.category("more").unwrap().category.overwrites.len() == 1,
        ),
        (Op::MoveChannel { key: s("help"), to: Move::Up }, |c| {
            channel_keys(c, "main") == ["general", "help", "rules"] && help_kept_its_overwrite(c)
        }),
        (Op::MoveChannelTo { key: s("help"), category: Some(s("more")) }, |c| {
            channel_keys(c, "more") == ["resources", "help"] && help_kept_its_overwrite(c)
        }),
        (Op::RemoveChannel { key: s("resources") }, |c| c.desired.channel("resources").is_none()),
        (Op::AddPosition { position: Position { role: s("spare"), ..Default::default() } }, |c| {
            c.bot.positions.iter().any(|p| p.role == "spare")
        }),
        (member(PositionField::Label(Some(s("Helper")))), |c| c.bot.positions[1].label.as_deref() == Some("Helper")),
        (member(PositionField::Admin(true)), |c| c.bot.positions[1].admin),
        (member(PositionField::Open(false)), |c| !c.bot.positions[1].open),
        (member(PositionField::Intro(Some(s("Hello")))), |c| c.bot.positions[1].intro.as_deref() == Some("Hello")),
        (
            member(PositionField::Questions(vec![
                Question::new("Why?"),
                Question { long: true, optional: true, ..Question::new("Anything else?") },
            ])),
            |c| c.bot.positions[1].questions.len() == 2 && c.bot.positions[1].questions[1].long,
        ),
        (Op::MovePosition { role: s("member"), to: Move::Up }, |c| c.bot.positions[0].role == "member"),
        (Op::RemovePosition { role: s("member") }, |c| c.bot.positions.len() == 1),
        (Op::AddHolder { role: s("admin"), user: user(8) }, |c| c.desired.holders["admin"].contains(&user(8))),
        (Op::RemoveHolder { role: s("admin"), user: user(7) }, |c| !c.desired.holders["admin"].contains(&user(7))),
        (Op::AddProject { name: s("Wyrm"), blurb: s("a GPU"), area: s("gpu"), status: ProjectStatus::Proposed }, |c| {
            c.projects.iter().any(|p| p.slug == "wyrm" && p.kind == "gpu")
        }),
        (golem(ProjectField::Name(s("Golem II"))), |c| c.projects[0].name == "Golem II"),
        (golem(ProjectField::Blurb(s("a CPU"))), |c| c.projects[0].blurb == "a CPU"),
        (golem(ProjectField::Area(s("cpu"))), |c| c.projects[0].kind == "cpu"),
        (golem(ProjectField::Status(ProjectStatus::Past)), |c| c.projects[0].status == ProjectStatus::Past),
        (golem(ProjectField::Stage(Some(s("taping out")))), |c| c.projects[0].stage.as_deref() == Some("taping out")),
        (golem(ProjectField::Github(Some(s("https://x/golem")))), |c| c.projects[0].github.is_some()),
        (golem(ProjectField::Channels(strings(&["general", "rtl"]))), |c| c.projects[0].channels == ["general", "rtl"]),
        (golem(ProjectField::Tags(strings(&["cpu"]))), |c| c.projects[0].tags == ["cpu"]),
        (golem(ProjectField::TeamRole(false)), |c| !c.projects[0].team_role),
        (golem(ProjectField::LeadRole(false)), |c| !c.projects[0].lead_role),
        (golem(ProjectField::Applications(true)), |c| c.projects[0].application.open),
        (golem(ProjectField::LeadQuestion(true)), |c| c.projects[0].application.lead_question),
        (golem(ProjectField::Questions(strings(&["What do you know?"]))), |c| {
            c.projects[0].application.questions == ["What do you know?"]
        }),
        (Op::AddLead { slug: s("golem"), user: user(6) }, |c| c.projects[0].leads == [user(5), user(6)]),
        (Op::RemoveLead { slug: s("golem"), user: user(5) }, |c| c.projects[0].leads.is_empty()),
        (Op::SetPost { channel: s("rules"), body: s("New rules.") }, |c| c.desired.posts["rules"] == "New rules.\n"),
        (Op::SetPost { channel: s("general"), body: s("Hello") }, |c| {
            c.desired.channel("general").unwrap().post.as_deref() == Some("content/general.md")
                && c.desired.posts["general"] == "Hello\n"
        }),
        (Op::SetBrief { slug: s("golem"), body: s("All about it") }, |c| {
            c.desired.project_posts["golem"] == "All about it\n"
        }),
        (Op::SetOnboarding { set: OnboardingField::Enabled(false) }, |c| !onboarding(c).enabled),
        (Op::SetOnboarding { set: OnboardingField::Advanced(true) }, |c| onboarding(c).advanced),
        (Op::SetOnboarding { set: OnboardingField::DefaultChannels(strings(&["general"])) }, |c| {
            onboarding(c).default_channels == ["general"]
        }),
        (
            Op::AddPrompt {
                prompt: OnbPrompt {
                    title: s("Q3"),
                    single_select: true,
                    required: false,
                    options: vec![OnbOption { title: s("D"), role: s("spare"), ..Default::default() }],
                },
            },
            |c| onboarding(c).prompts[2].options[0].role == "spare",
        ),
        (Op::SetPrompt { prompt: 0, set: PromptField::Title(s("Where?")) }, |c| {
            onboarding(c).prompts[0].title == "Where?"
        }),
        (Op::SetPrompt { prompt: 0, set: PromptField::SingleSelect(false) }, |c| {
            !onboarding(c).prompts[0].single_select
        }),
        (Op::SetPrompt { prompt: 0, set: PromptField::Required(true) }, |c| onboarding(c).prompts[0].required),
        (Op::MovePrompt { prompt: 1, to: Move::Up }, |c| {
            let p = &onboarding(c).prompts;
            p[0].title == "Q2" && p[0].options.len() == 1 && p[1].options.len() == 2
        }),
        (Op::RemovePrompt { prompt: 1 }, |c| onboarding(c).prompts.len() == 1),
        (
            Op::AddOption {
                prompt: 1,
                option: OnbOption { title: s("E"), channels: strings(&["general"]), ..Default::default() },
            },
            |c| onboarding(c).prompts[1].options[1].title == "E",
        ),
        (option(OptionField::Title(s("BB"))), |c| onboarding(c).prompts[0].options[1].title == "BB"),
        (option(OptionField::Description(Some(s("bee")))), |c| {
            onboarding(c).prompts[0].options[1].description.is_some()
        }),
        (option(OptionField::Emoji(Some(s("🐝")))), |c| onboarding(c).prompts[0].options[1].emoji.is_some()),
        (option(OptionField::Role(s("spare"))), |c| onboarding(c).prompts[0].options[1].role == "spare"),
        (option(OptionField::Channels(strings(&["rules"]))), |c| {
            onboarding(c).prompts[0].options[1].channels == ["rules"]
        }),
        (Op::MoveOption { prompt: 0, option: 1, to: Move::Up }, |c| onboarding(c).prompts[0].options[0].title == "B"),
        (Op::RemoveOption { prompt: 0, option: 0 }, |c| onboarding(c).prompts[0].options.len() == 1),
        (Op::SetScreening { set: ScreeningField::Enabled(false) }, |c| !screening(c).enabled),
        (Op::SetScreening { set: ScreeningField::Description(s("Welcome")) }, |c| {
            screening(c).description == "Welcome"
        }),
        (Op::SetScreening { set: ScreeningField::Label(s("Yes")) }, |c| screening(c).label == "Yes"),
        (Op::SetScreening { set: ScreeningField::Rules(strings(&["One", "Two"])) }, |c| screening(c).rules.len() == 2),
        (
            Op::AddRule {
                rule: AutoModRule {
                    key: s("words"),
                    name: s("Words"),
                    trigger: Trigger::Keyword,
                    enabled: true,
                    keywords: strings(&["*scam*"]),
                    actions: vec![ModAction::Block],
                    ..Default::default()
                },
            },
            |c| c.desired.settings.automod.iter().any(|r| r.key == "words"),
        ),
        (Op::SetRule { rule: spam("No spam") }, |c| c.desired.settings.automod[0].name == "No spam"),
        (Op::RemoveRule { key: s("spam") }, |c| c.desired.settings.automod.is_empty()),
        (Op::SetBot { set: BotField::Name(s("U")) }, |c| c.bot.name == "U"),
        (Op::SetBot { set: BotField::Timezone(s("Europe/Paris")) }, |c| c.bot.timezone == "Europe/Paris"),
        (Op::SetBot { set: BotField::Archive(Some(s("empty"))) }, |c| c.bot.archive.as_deref() == Some("empty")),
        (Op::SetBot { set: BotField::LogPeople(Some(s("rules"))) }, |c| c.bot.log.people.as_deref() == Some("rules")),
        (Op::SetBot { set: BotField::LogBot(Some(s("rules"))) }, |c| c.bot.log.bot.as_deref() == Some("rules")),
        (Op::SetBot { set: BotField::KeepDays(30) }, |c| c.bot.log.keep_days == 30),
        (section(Section::Review(Some(Review { channel: s("rules") }))), |c| {
            c.bot.review.as_ref().is_some_and(|r| r.channel == "rules")
        }),
        (section(Section::Review(None)), |c| c.bot.review.is_none()),
        (section(Section::Events(Some(Events { channel: s("general"), ..Events::default() }))), |c| {
            c.bot.events.is_some()
        }),
        (section(Section::Feeds(Some(Feeds { channel: s("resources") }))), |c| c.bot.feeds.is_some()),
        (
            section(Section::Ideas(Some(Ideas {
                forum: s("resources"),
                needs_lead_tag: Some(s("lead")),
                ..Default::default()
            }))),
            |c| c.bot.ideas.is_some(),
        ),
        (section(Section::Help(None)), |c| c.bot.help.is_none()),
        (
            section(Section::Projects(Some(ProjectsConf {
                forum: s("resources"),
                areas: strings(&["cpu"]),
                ..Default::default()
            }))),
            |c| c.bot.projects.as_ref().is_some_and(|p| p.forum == "resources"),
        ),
        (
            section(Section::Profile(Some(Profile {
                company_roles: strings(&["spare"]),
                company_question: s("Where do you work?"),
            }))),
            |c| c.bot.profile.is_some(),
        ),
        (section(Section::Resources(Some(Resources { forum: s("resources") }))), |c| c.bot.resources.is_some()),
        (section(Section::Web(Some(Web { url: s("https://x"), share_device: None }))), |c| c.bot.web.is_some()),
        (section(Section::Drift(DriftMode::Report)), |c| c.bot.drift.mode == DriftMode::Report),
        (Op::SetFederation { set: FederationField::Name(s("them")) }, |c| c.federation.name == "them"),
        (Op::SetFederation { set: FederationField::Inbox(Some(s("rules"))) }, |c| c.federation.inbox.is_some()),
        (Op::SetFederation { set: FederationField::Publish(strings(&["general"])) }, |c| {
            c.federation.publish == ["general"]
        }),
        (Op::AddPeer { peer: Peer { name: s("U"), ..Default::default() } }, |c| c.federation.peers.len() == 2),
        (
            Op::SetPeer {
                peer: Peer {
                    name: s("S"),
                    share: strings(&["rules"]),
                    share_webhook_env: s("S_SHARE"),
                    ..Default::default()
                },
            },
            |c| c.federation.peers[0].share == ["rules"],
        ),
        (Op::RemovePeer { name: s("S") }, |c| c.federation.peers.is_empty()),
    ]
}

/// Ops that take a whole entry out, comments above it included.
fn removes(op: &Op) -> bool {
    op.describe().starts_with("Turn off")
        || matches!(
            op,
            Op::RemoveRole { .. }
                | Op::RemoveCategory { .. }
                | Op::RemoveChannel { .. }
                | Op::RemovePosition { .. }
                | Op::RemovePrompt { .. }
                | Op::RemoveOption { .. }
                | Op::RemoveRule { .. }
                | Op::RemovePeer { .. }
                | Op::RemoveHolder { .. }
        )
}

#[test]
fn every_op_edits_the_files_keeping_comments() {
    for (op, ok) in ops() {
        let what = op.describe();
        assert!(!what.is_empty() && !what.contains('\n'), "{what:?}");
        let json = serde_json::to_string(&op).unwrap();
        assert_eq!(serde_json::from_str::<Op>(&json).unwrap(), op, "{json}");

        let d = fixture();
        let before = comments(d.path());
        let change = apply_op(d.path(), &op).unwrap_or_else(|e| panic!("{what}: {e:#}"));
        assert!(!change.files.is_empty(), "{what}: nothing written");
        assert_eq!(change.summary, what);
        assert_eq!(crate::authoring::check(d.path()).unwrap().errors, Vec::<String>::new(), "{what}");
        let cfg = Config::load(d.path()).unwrap();
        assert!(ok(&cfg), "{what}: not as asked\n{:#?}", read_all(d.path()).unwrap().keys());
        if !removes(&op) {
            let after = comments(d.path());
            let lost: Vec<&String> = before.iter().filter(|c| !after.contains(c)).collect();
            assert!(lost.is_empty(), "{what}: lost comments {lost:?}");
        }
    }
}

#[test]
fn edits_keep_inline_comments_and_place_new_entries_with_their_kind() {
    let d = fixture();
    let p = d.path();
    apply_op(p, &Op::SetRole { key: s("member"), set: RoleField::Name(s("Members")) }).unwrap();
    assert!(fs::read_to_string(p.join("roles.toml")).unwrap().contains("name = \"Members\" # everyone who joined"));
    apply_op(p, &Op::SetRule { rule: spam("No spam") }).unwrap();
    let automod = fs::read_to_string(p.join("automod.toml")).unwrap();
    assert!(automod.contains("actions = [\"block\"] # quietly") && automod.starts_with("# automod"), "{automod}");
    apply_op(p, &Op::SetSection { section: Section::Review(Some(Review { channel: s("rules") })) }).unwrap();
    let bot = fs::read_to_string(p.join("bot.toml")).unwrap();
    assert!(bot.contains("channel = \"rules\" # applications"), "{bot}");
    apply_op(p, &Op::AddPosition { position: Position { role: s("spare"), ..Default::default() } }).unwrap();
    let bot = fs::read_to_string(p.join("bot.toml")).unwrap();
    let spare = bot.find("role = \"spare\"").unwrap();
    assert!(spare > bot.find("role = \"member\"").unwrap(), "{bot}");
    assert!(spare < bot.find("[review]").unwrap(), "new positions go with the others: {bot}");
}

#[test]
fn unchanged_values_write_nothing() {
    let d = fixture();
    let c = apply_op(d.path(), &Op::SetRole { key: s("member"), set: RoleField::Name(s("Member")) }).unwrap();
    assert!(c.files.is_empty(), "{c:?}");
}

#[test]
fn an_option_can_be_added_to_a_question_added_without_any() {
    let d = fixture();
    let prompt = OnbPrompt { title: s("Pronouns?"), ..Default::default() };
    apply_op(d.path(), &Op::AddPrompt { prompt }).unwrap();
    let n = schema(d.path()).unwrap().onboarding.map_or(0, |o| o.prompts.len());
    let option = OnbOption { title: s("they/them"), role: s("guest"), ..Default::default() };
    apply_op(d.path(), &Op::AddOption { prompt: n - 1, option }).unwrap();
    let o = schema(d.path()).unwrap().onboarding.unwrap();
    assert_eq!(o.prompts[n - 1].options[0].title, "they/them");
}

#[test]
fn new_warnings_are_reported() {
    let d = fixture();
    let many = PositionField::Questions(["a", "b", "c", "d", "e", "f"].into_iter().map(Question::new).collect());
    let c = apply_op(d.path(), &Op::SetPosition { role: s("member"), set: many }).unwrap();
    assert!(c.warnings.iter().any(|w| w.contains("first 5")), "{c:?}");
}

#[test]
fn breaking_ops_are_refused_and_nothing_is_written() {
    let url = Peer { name: s("X"), webhook_env: s("https://discord.com/api/webhooks/1"), ..Default::default() };
    let breaking = [
        (Op::RemoveRole { key: s("member") }, "unknown role `member`"),
        (Op::SetChannel { key: s("help"), set: ChannelField::Kind(ChannelKind::Text) }, "must be a forum"),
        (Op::SetBot { set: BotField::AdminRole(None) }, "needs admin_role"),
        (Op::AddHolder { role: s("ghost"), user: user(1) }, "unknown role `ghost`"),
        (Op::SetPosition { role: s("member"), set: PositionField::Questions(Vec::new()) }, "at least one question"),
        (Op::SetRole { key: s("member"), set: RoleField::Color(Some(s("red"))) }, "bad colour"),
        (Op::RemoveChannel { key: s("general") }, "unknown channel `general`"),
        (
            Op::SetCategoryOverwrites {
                key: s("main"),
                overwrites: vec![Overwrite { role: Some(s("ghost")), ..Default::default() }],
            },
            "ghost",
        ),
        (Op::AddPeer { peer: url }, "not hold the URL"),
        (Op::SetPost { channel: s("rules"), body: s("{button:launch|Go}") }, "unknown button action"),
    ];
    for (op, want) in breaking {
        let d = fixture();
        let before = read_all(d.path()).unwrap();
        let err = apply_op(d.path(), &op).expect_err(&op.describe());
        let problems = err.downcast_ref::<Problems>().unwrap_or_else(|| panic!("{}: {err:#}", op.describe()));
        assert!(problems.0.iter().any(|p| p.contains(want)), "{}: {problems}", op.describe());
        assert_eq!(read_all(d.path()).unwrap(), before, "{}: wrote anyway", op.describe());
    }
    let impossible = [
        (Op::MoveRole { key: s("admin"), to: Move::Up }, "can't move there"),
        (Op::RemoveCategory { key: s("main") }, "still has channels"),
        (Op::AddRole { role: Role { key: s("admin"), name: s("A"), ..Default::default() } }, "already exists"),
        (Op::SetRole { key: s("ghost"), set: RoleField::Hoist(true) }, "no role `ghost`"),
        (Op::SetChannel { key: s("proj-golem-general"), set: ChannelField::Nsfw(true) }, "projects.toml"),
        (Op::AddHolder { role: s("admin"), user: user(7) }, "already holds"),
        (Op::RemoveLead { slug: s("golem"), user: user(9) }, "isn't a lead"),
        (Op::SetPrompt { prompt: 5, set: PromptField::Required(true) }, "no number 6"),
        (Op::MoveChannelTo { key: s("general"), category: Some(s("main")) }, "already there"),
        (Op::SetPost { channel: s("../../x"), body: s("hi") }, "no channel"),
    ];
    for (op, want) in impossible {
        let d = fixture();
        let before = read_all(d.path()).unwrap();
        let err = format!("{:#}", apply_op(d.path(), &op).expect_err(&op.describe()));
        assert!(err.contains(want), "{}: {err}", op.describe());
        assert_eq!(read_all(d.path()).unwrap(), before, "{}: wrote anyway", op.describe());
    }
}

#[test]
fn files_outside_the_config_are_refused() {
    let d = fixture();
    let name = d.path().file_name().unwrap().to_string_lossy();
    let post = Some(format!("../{name}/content/rules.md"));
    let channel = Channel { key: s("x"), name: s("x"), post, ..Default::default() };
    let err = apply_op(d.path(), &Op::AddChannel { category: None, channel }).unwrap_err();
    assert!(err.downcast_ref::<Problems>().unwrap().0[0].contains("inside the config folder"), "{err:#}");
}

#[test]
fn problems_the_config_already_had_do_not_block_edits() {
    let d = fixture();
    let p = d.path();
    write(p, "members.toml", "ghost = [\"1\"]\n");
    assert!(!crate::authoring::check(p).unwrap().errors.is_empty());
    apply_op(p, &Op::SetRole { key: s("member"), set: RoleField::Hoist(true) }).unwrap();
    let c = apply_op(p, &Op::RemoveHolder { role: s("ghost"), user: user(1) }).unwrap();
    assert_eq!(c.files, ["members.toml"]);
}

#[test]
fn schema_shows_what_the_files_say() {
    let d = fixture();
    let s = schema(d.path()).unwrap();
    assert_eq!(s.roles.len(), 6, "project roles aren't editable here");
    assert_eq!(s.posts["rules"].file, "content/rules.md");
    assert!(s.posts["rules"].body.starts_with("Be kind"));
    assert_eq!(s.holders["admin"], [user(7)]);
    assert!(s.choices.permissions.iter().any(|p| p == "SEND_MESSAGES"));
    assert!(serde_json::to_value(&s).unwrap()["bot"]["position"].is_array(), "fields are named as in bot.toml");
}

#[test]
fn ops_made_together_are_all_or_nothing() {
    let d = fixture();
    let p = d.path();
    let good = Op::SetRole { key: s("member"), set: RoleField::Hoist(true) };
    let bad = Op::SetChannel { key: s("ghost"), set: ChannelField::Topic(None) };
    assert!(apply_ops(p, &[good.clone(), bad]).is_err());
    assert!(!Config::load(p).unwrap().desired.role("member").unwrap().hoist, "the first wasn't written either");
    let other = Op::SetRole { key: s("spare"), set: RoleField::Mentionable(true) };
    let change = apply_ops(p, &[good, other]).unwrap();
    assert!(change.summary.starts_with("2 changes: Role member: hoist on; Role spare"), "{}", change.summary);
    let c = Config::load(p).unwrap();
    assert!(c.desired.role("member").unwrap().hoist && c.desired.role("spare").unwrap().mentionable);
}

#[test]
fn leaving_active_closes_a_projects_applications() {
    for status in [ProjectStatus::Past, ProjectStatus::Proposed] {
        let d = fixture();
        let p = d.path();
        apply_op(p, &Op::SetProject { slug: s("golem"), set: ProjectField::Applications(true) }).unwrap();
        let change = apply_op(p, &Op::SetProject { slug: s("golem"), set: ProjectField::Status(status) }).unwrap();
        assert!(change.summary.contains("applications to golem closed"), "{}", change.summary);
        let c = Config::load(p).unwrap();
        assert_eq!(c.projects[0].status, status);
        assert!(!c.projects[0].application.open, "{status}: closed in the same edit");
    }
    // Opening them on a project that isn't active is warned about.
    let d = fixture();
    let p = d.path();
    apply_op(p, &Op::SetProject { slug: s("golem"), set: ProjectField::Status(ProjectStatus::Past) }).unwrap();
    let change = apply_op(p, &Op::SetProject { slug: s("golem"), set: ProjectField::Applications(true) }).unwrap();
    assert!(change.warnings.iter().any(|w| w.contains("doesn't offer it until it's active")), "{:?}", change.warnings);
}

#[test]
fn keys_that_arent_keys_are_refused() {
    let d = fixture();
    for key in ["Mod", "mod_team", "a b", ""] {
        let role = Role { key: s(key), name: s("Mod"), ..Default::default() };
        let err = apply_op(d.path(), &Op::AddRole { role }).unwrap_err();
        assert!(format!("{err:#}").contains("lower-case letters, digits and dashes"), "{key:?}: {err:#}");
    }
}

#[test]
fn renaming_a_project_moves_its_keys_and_keeps_its_discord_objects() {
    let d = fixture();
    let p = d.path();
    write(
        p,
        "lock.toml",
        "guild = \"1\"\n\n[roles]\nproj-golem-team = \"10\"\nproj-golem-lead = \"11\"\nmember = \"12\"\n\n\
         [categories]\nproj-golem = \"20\"\n\n[channels]\nproj-golem-general = \"30\"\nproj-golem-old = \"31\"\n\n\
         [project_posts.golem]\nthread = \"40\"\nmessage = \"41\"\n\n[applied]\n\"role:proj-golem-team\" = \"h1\"\n\
         \"channel:proj-golem-general\" = \"h2\"\n\"project-post:golem\" = \"h3\"\n\"role:member\" = \"h4\"\n",
    );
    write(p, "members.toml", "# who holds what\nadmin = [\"7\"] # elected\nproj-golem-team = [\"8\"]\n");
    write(
        p,
        "content/rules.md",
        "Be kind. Ask in {#proj-golem-general}, ping {@proj-golem-lead}.\n{button:apply:golem|Join}\n",
    );
    let before = comments(p);
    let change = apply_op(p, &Op::RenameProject { from: s("golem"), to: s("robot") }).unwrap();
    assert_eq!(change.summary, "Rename project golem to robot");

    let c = Config::load(p).unwrap();
    assert_eq!(c.projects[0].slug, "robot");
    let l = &c.desired.lock;
    assert_eq!(l.role("proj-robot-team"), Some(Snowflake(10)));
    assert_eq!(l.role("proj-robot-lead"), Some(Snowflake(11)));
    assert_eq!(l.category("proj-robot"), Some(Snowflake(20)));
    assert_eq!(l.channels.get("proj-robot-general"), Some(&Snowflake(30)));
    assert_eq!(l.channels.get("proj-robot-old"), Some(&Snowflake(31)), "a channel it had once");
    assert_eq!(l.project_posts.get("robot").map(|t| t.thread), Some(Snowflake(40)));
    assert!(l.roles.keys().chain(l.channels.keys()).all(|k| !k.contains("golem")), "{l:?}");
    let applied: Vec<&str> = l.applied.hashes.keys().map(String::as_str).collect();
    assert_eq!(applied, ["channel:proj-robot-general", "project-post:robot", "role:member", "role:proj-robot-team"]);
    assert_eq!(l.role("member"), Some(Snowflake(12)), "others untouched");
    assert_eq!(c.members.get("proj-robot-team"), Some(&vec![Snowflake(8)]));
    let rules = fs::read_to_string(p.join("content/rules.md")).unwrap();
    assert_eq!(rules, "Be kind. Ask in {#proj-robot-general}, ping {@proj-robot-lead}.\n{button:apply:robot|Join}\n");
    assert_eq!(comments(p), before, "comments kept");

    // Refused: a slug that's taken, not a key, or a project that isn't there.
    let err = |op: Op| format!("{:#}", apply_op(p, &op).unwrap_err());
    assert!(err(Op::RenameProject { from: s("robot"), to: s("Bad Slug") }).contains("lower-case"));
    assert!(err(Op::RenameProject { from: s("golem"), to: s("x") }).contains("no project"));
    assert!(err(Op::RenameProject { from: s("robot"), to: s("robot") }).contains("already has"));
}

#[test]
fn a_rename_leaves_a_longer_slugs_keys_alone() {
    let golem = crate::schema::Project { slug: s("golem"), channels: strings(&["general"]), ..Default::default() };
    let golem2 = crate::schema::Project { slug: s("golem-2"), ..Default::default() };
    let mut lock = crate::schema::Lock::default();
    for k in ["proj-golem-general", "proj-golem-2", "proj-golem-2-team", "proj-golem-2-general"] {
        lock.channels.insert(s(k), Snowflake(1));
    }
    let r = project_renames(&golem, "robot", std::slice::from_ref(&golem2), &lock);
    assert_eq!(r.get("proj-golem-general").map(String::as_str), Some("proj-robot-general"));
    assert!(r.keys().all(|k| !k.starts_with("proj-golem-2")), "{r:?}");
}

#[test]
fn a_positions_list_follows_bot_toml() {
    let d = fixture();
    let p = d.path();
    write(p, "content/rules.md", "Open roles:\n{positions}\nProjects:\n{projects:open}\n");
    let post = |p: &Path| Config::load(p).unwrap().desired.posts["rules"].clone();
    assert_eq!(post(p), "Open roles:\n- **Chair**\n- **Member**\nProjects:\nNothing open right now.\n");
    apply_op(p, &Op::SetPosition { role: s("member"), set: PositionField::Open(false) }).unwrap();
    apply_op(p, &Op::SetPosition { role: s("chair"), set: PositionField::Intro(Some(s("Runs meetings.\nMore."))) })
        .unwrap();
    apply_op(p, &Op::SetProject { slug: s("golem"), set: ProjectField::Blurb(s("a CPU")) }).unwrap();
    apply_op(p, &Op::SetProject { slug: s("golem"), set: ProjectField::Applications(true) }).unwrap();
    assert_eq!(post(p), "Open roles:\n- **Chair**: Runs meetings.\nProjects:\n- **Golem**: a CPU\n");
}
