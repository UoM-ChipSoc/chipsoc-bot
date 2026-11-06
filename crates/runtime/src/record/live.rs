//! The live voice path: songbird's driver (with DAVE, Discord's end-to-end
//! encryption for calls) receiving a call and handing what it hears to the
//! recorder, and the gateway command that moves the bot in and out of calls.
//! It can't be tested without Discord; everything else sits behind [`Voice`].

use std::num::NonZeroU64;

use anyhow::{Context, Result};
use chipsoc_voice::capture::{Feed, Input};
use songbird::driver::{Channels, DecodeConfig, DecodeMode, SampleRate};
use songbird::events::{CoreEvent, Event, EventContext, EventHandler};
use songbird::id::{ChannelId, GuildId, UserId};
use songbird::{ConnectionInfo, Driver};
use twilight_gateway::MessageSender;
use twilight_model::gateway::payload::outgoing::UpdateVoiceState;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker};

use super::{BoxFuture, Call, Conn, Voice};

/// The real thing: the shard's gateway, and a songbird driver per call.
pub(crate) struct Live {
    gateway: MessageSender,
}

impl Live {
    pub fn new(gateway: MessageSender) -> Self {
        Self { gateway }
    }
}

fn nz(id: u64) -> Result<NonZeroU64> {
    NonZeroU64::new(id).context("an id of 0")
}

impl Voice for Live {
    fn move_to(&self, guild: Id<GuildMarker>, channel: Option<Id<ChannelMarker>>) {
        // Deafened, the bot would hear nothing; muted, it says nothing.
        if let Err(e) = self.gateway.command(&UpdateVoiceState::new(guild, channel, false, true)) {
            tracing::warn!(?e, "asking to join or leave a call failed");
        }
    }

    fn connect(&self, conn: Conn, feed: Feed) -> BoxFuture<'static, Result<Box<dyn Call>>> {
        Box::pin(async move {
            let config = songbird::Config::default()
                .decode_mode(DecodeMode::Decode(DecodeConfig::new(Channels::Mono, SampleRate::Hz48000)));
            let mut driver = Driver::new(config);
            let hear = Hear(feed);
            driver.add_global_event(Event::Core(CoreEvent::VoiceTick), hear.clone());
            driver.add_global_event(Event::Core(CoreEvent::SpeakingStateUpdate), hear.clone());
            driver.add_global_event(Event::Core(CoreEvent::DriverDisconnect), hear.clone());
            driver.add_global_event(Event::Core(CoreEvent::DriverReconnect), hear);
            let info = ConnectionInfo {
                channel_id: ChannelId(nz(conn.channel)?),
                endpoint: conn.endpoint,
                guild_id: GuildId(nz(conn.guild)?),
                session_id: conn.session,
                token: conn.token,
                user_id: UserId(nz(conn.me)?),
            };
            driver.connect(info).await.context("connecting to the call's voice server")?;
            Ok(Box::new(Connected(driver)) as Box<dyn Call>)
        })
    }
}

struct Connected(Driver);

impl Call for Connected {
    fn leave(&mut self) {
        self.0.leave();
    }
}

/// Hands songbird's events to the recorder.
#[derive(Clone)]
struct Hear(Feed);

#[async_trait::async_trait]
impl EventHandler for Hear {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::VoiceTick(t) => {
                let voices = t
                    .speaking
                    .iter()
                    .filter_map(|(ssrc, d)| d.decoded_voice.as_ref().map(|v| (*ssrc, v.clone())))
                    .collect();
                self.0.send(Input::Tick(voices));
            }
            EventContext::SpeakingStateUpdate(s) => {
                if let Some(u) = s.user_id {
                    self.0.send(Input::Speaker { ssrc: s.ssrc, user: u.0 });
                }
            }
            EventContext::DriverDisconnect(d) => {
                tracing::warn!(kind = ?d.kind, reason = ?d.reason, "the call's voice connection dropped");
            }
            EventContext::DriverReconnect(_) => tracing::info!("the call's voice connection is back"),
            _ => {}
        }
        None
    }
}
